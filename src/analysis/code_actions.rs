//! `textDocument/codeAction` — the refactorings that need no Ruby.
//!
//! # Why these four, and why no quick fix
//!
//! Of the five families a Ruby editor is used to, exactly one needs a linter: autocorrect, which
//! RuboCop serves over its own `textDocument/codeAction`, so a user running both servers already
//! has it. The other four are rewrites over a Prism tree, and Prism is already linked into this
//! binary — so this module composes with RuboCop rather than competing with it.
//!
//! There is no `quickfix`, and the reason is the shape of the diagnostic table rather than a
//! preference: two of its ten rules are statements about the user's code and the other eight are
//! rubydex saying *it* gave up. A fix needs a rule that knows what the code should say instead,
//! and "the indexer could not follow this" does not.
//!
//! # This is the second module in the crate that writes
//!
//! [`rename`](super::rename) states the bar: every other wrong answer shows the user something
//! unhelpful and they look elsewhere, while a wrong answer here edits their files. The test is
//! not "how much can be refactored" but "what can be refactored *exactly*". Three things enforce
//! it, in increasing order of how much they catch:
//!
//! - **Nothing is offered where the file does not parse.** A tree built by error recovery hands
//!   out spans that do not nest, and an edit placed by one of those lands anywhere.
//! - **Every guard refuses rather than approximating.** Where the analysis cannot answer — a
//!   local the extraction would have to hand back, a block whose delimiters do not bind the same
//!   way, an accessor that would read a different variable — the action is not offered. The
//!   refusal is silent, unlike a rename's: the user pressed no key asking for this one, and an
//!   action absent from a menu is the right way to say no.
//! - **Every action is applied to a copy of the buffer and the result is parsed before it is
//!   offered.** Exactness as a gate rather than as something measured afterwards. It is not
//!   decoration: it catches spellings the guards above do not model, such as a `do … end` block
//!   carrying an `ensure` (which a brace block cannot hold) and two adjacent string literals
//!   joined by a line continuation (one string with two nodes in it). The first is what an RSpec
//!   `around` hook looks like.
//!
//! The re-parse cannot be the only guard, because Ruby's two block delimiters bind differently
//! and both spellings parse:
//!
//! ```ruby
//! def show(x) = "show(#{x.inspect})"
//! puts show [1, 2].map { |n| n * 2 }     # => show([2, 4])
//! puts show [1, 2].map do |n| n * 2 end  # => show(#<Enumerator: [1, 2]:map>)
//! ```
//!
//! Same code, delimiters swapped, a different program that still runs. Toggling block style
//! therefore needs a precedence test, and extract-to-method needs to pass the locals it reads —
//! slicing a selection out verbatim gives a method that raises `NameError` on its first call.
//!
//! # Titles are not messages
//!
//! `messages.rs` holds every sentence about the *workspace*. A code action's title is part of an
//! answer to one request, like a hover card or a completion label, so it lives here.

use ruby_prism::{
    ArgumentsNode, BlockArgumentNode, BlockNode, BlockParameterNode, CallNode, ClassNode, DefNode,
    ElseNode, EnsureNode, LocalVariableTargetNode, Location, ModuleNode, Node, ParametersNode,
    ParseResult, RescueNode, SplatNode, StatementsNode, Visit,
};

use super::scopes;

/// The name an extracted local is given, before it is made free of the file.
const VARIABLE: &str = "extracted";

/// The name an extracted method is given, before it is made free of the file.
const METHOD: &str = "extracted_method";

/// One span of the document replaced with text.
///
/// Byte offsets, as everything inside `analysis` is; the caller converts once, from the same
/// read that produced them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub start: u32,
    pub end: u32,
    pub text: String,
}

/// Which menu an action belongs in.
///
/// The two the protocol has for this, and the server advertises exactly these: a client filters
/// on the kind before it asks, so a kind advertised and never returned shows an empty submenu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Extract,
    Rewrite,
}

/// One refactoring, and the whole of what choosing it does.
#[derive(Debug, PartialEq, Eq)]
pub struct Action {
    pub title: String,
    pub kind: Kind,
    pub edits: Vec<Edit>,
}

/// Every refactoring available over `source[start..end]`.
///
/// A cursor is an empty range, and two of the four answer for one: an accessor and a block
/// toggle need a position, the two extractions need a selection.
#[must_use]
pub fn at(source: &str, start: u32, end: u32) -> Vec<Action> {
    let parsed = ruby_prism::parse(source.as_bytes());
    // Nothing is offered where the file does not parse. Recovery invents spans that do not nest
    // — the trap `locator::spans` exists for — and an edit placed by one of those lands
    // somewhere else in the buffer.
    if parsed.errors().next().is_some() {
        return Vec::new();
    }
    let (start, end) = (start.min(source.len() as u32), end.min(source.len() as u32));

    let mut actions = accessors(source, &parsed, start);
    actions.extend(toggle_block(source, &parsed, start, end));
    actions.extend(extract_variable(source, &parsed, start, end));
    actions.extend(extract_method(source, &parsed, start, end));
    actions.retain(|action| survives(source, action));
    actions
}

/// Whether applying `action` leaves a file Prism can still parse.
///
/// The cheapest available proxy for exactness, and it is a gate rather than a report: an edit
/// that breaks the buffer is a bug, and one the user never sees is better than one measured
/// after the fact.
fn survives(source: &str, action: &Action) -> bool {
    let mut edited = source.to_owned();
    let mut edits = action.edits.clone();
    // Back to front, so that each span still means what it meant when it was produced.
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.start));
    for edit in edits {
        edited.replace_range(edit.start as usize..edit.end as usize, &edit.text);
    }
    ruby_prism::parse(edited.as_bytes())
        .errors()
        .next()
        .is_none()
}

// ---------------------------------------------------------------------------
// attr_reader / attr_writer / attr_accessor
// ---------------------------------------------------------------------------

/// The accessors an instance variable under the cursor could be given.
///
/// Exact by construction: an insertion at the top of the class body, which cannot change what
/// any existing line means. The one thing that *could* be wrong is which variable the accessor
/// would read, and [`scopes::accessor_site`] is what settles it — `attr_reader :count` reads an
/// instance's `@count`, so it is not offered for the `@count` inside `def self.count`, where it
/// would write an accessor that runs, returns `nil`, and looks right.
fn accessors(source: &str, parsed: &ParseResult<'_>, offset: u32) -> Vec<Action> {
    let Some((name, body)) = scopes::accessor_site(source, offset) else {
        return Vec::new();
    };
    let bare = name.trim_start_matches('@').to_owned();
    let declared = Declared::of(parsed, body, &bare);
    // An `attr_accessor` already there covers both halves, so all three would be noise; two
    // separate declarations cover both halves too, and then only the third is.
    let declares = |macro_name: &str| declared.iter().any(|it| it == macro_name);
    let reads = declares("attr_reader") || declares("attr_accessor");
    let writes = declares("attr_writer") || declares("attr_accessor");

    let indent = indent_before(source, body);
    let mut actions = Vec::new();
    for macro_name in ["attr_reader", "attr_writer", "attr_accessor"] {
        let wanted = match macro_name {
            "attr_reader" => !reads,
            "attr_writer" => !writes,
            _ => !(reads && writes),
        };
        if !wanted {
            continue;
        }
        actions.push(Action {
            title: format!("Declare {macro_name} :{bare}"),
            kind: Kind::Rewrite,
            // The body's own indentation is reused twice over: once for the line being written
            // and once to put back the indentation of the statement it displaces.
            edits: vec![Edit {
                start: body,
                end: body,
                text: format!("{macro_name} :{bare}\n{indent}"),
            }],
        });
    }
    actions
}

/// Which `attr_` macros a namespace body already declares for one name.
///
/// **Direct statements of the body only.** An `attr_reader` inside an `if` in the body is
/// conditional and one inside a nested class belongs to the nested class, so neither is a reason
/// to withhold the offer.
struct Declared<'a> {
    body: u32,
    name: &'a str,
    found: Vec<String>,
}

impl<'a> Declared<'a> {
    fn of(parsed: &ParseResult<'_>, body: u32, name: &'a str) -> Vec<String> {
        let mut walk = Declared {
            body,
            name,
            found: Vec::new(),
        };
        walk.visit(&parsed.node());
        walk.found
    }

    fn scan(&mut self, body: Option<&Node<'_>>) {
        let Some(statements) = body.and_then(ruby_prism::Node::as_statements_node) else {
            return;
        };
        if statements.location().start_offset() as u32 != self.body {
            return;
        }
        for statement in statements.body().iter() {
            let Some(call) = statement.as_call_node() else {
                continue;
            };
            if call.receiver().is_some() {
                continue;
            }
            let macro_name = String::from_utf8_lossy(call.name().as_slice()).into_owned();
            if !matches!(
                macro_name.as_str(),
                "attr_reader" | "attr_writer" | "attr_accessor"
            ) {
                continue;
            }
            let named = call.arguments().is_some_and(|arguments| {
                arguments.arguments().iter().any(|argument| {
                    argument
                        .as_symbol_node()
                        .is_some_and(|symbol| symbol.unescaped() == self.name.as_bytes())
                })
            });
            if named {
                self.found.push(macro_name);
            }
        }
    }
}

impl<'pr> Visit<'pr> for Declared<'_> {
    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        self.scan(node.body().as_ref());
        ruby_prism::visit_class_node(self, node);
    }

    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        self.scan(node.body().as_ref());
        ruby_prism::visit_module_node(self, node);
    }
}

// ---------------------------------------------------------------------------
// Toggle block style
// ---------------------------------------------------------------------------

/// Swap a block's delimiters, when the two spellings mean the same thing.
///
/// The guard is the whole of it. `{ }` binds to the nearest call and `do … end` binds to
/// the outermost command, so the swap is safe only where there is no command call in between —
/// and the same test also refuses `it "works" do … end`, where the brace form is not a different
/// program but a syntax error.
fn toggle_block(source: &str, parsed: &ParseResult<'_>, start: u32, end: u32) -> Vec<Action> {
    let calls = Calls::of(parsed, start, end);
    // Innermost first, so a cursor inside a nested block toggles the block it is in rather than
    // the one around it.
    let Some(at) = calls.iter().rposition(|call| call.block.is_some()) else {
        return Vec::new();
    };
    let owner = &calls[at];
    // A command call cannot take a brace block at all, and one that encloses the owner's call in
    // its own arguments is exactly the case above: the `do` form would bind to it instead.
    let bound = owner.command()
        || calls[..at].iter().any(|outer| {
            outer.command()
                && outer
                    .arguments
                    .is_some_and(|args| args.0 <= owner.start && owner.end <= args.1)
        });
    if bound {
        return Vec::new();
    }

    let (opening, closing) = owner.block.expect("the owner is the call that has a block");
    let braces = &source[opening.0 as usize..opening.1 as usize] == "{";
    let (open, close) = if braces { ("do", "end") } else { ("{", "}") };
    let title = if braces {
        "Convert to a do…end block"
    } else {
        "Convert to a { } block"
    };
    vec![Action {
        title: title.to_owned(),
        kind: Kind::Rewrite,
        edits: vec![
            Edit {
                start: opening.0,
                end: opening.1,
                // `do|n|` and `{|n|` both parse; a space is written anyway, because the point of
                // the action is the reading and not the parsing.
                text: format!("{open}{}", spacer(source, opening.1, true)),
            },
            Edit {
                start: closing.0,
                end: closing.1,
                text: format!("{}{close}", spacer(source, closing.0, false)),
            },
        ],
    }]
}

/// A single space, where putting one in is what keeps the result readable.
fn spacer(source: &str, at: u32, after: bool) -> &'static str {
    let touching = if after {
        source[at as usize..].starts_with(|it: char| !it.is_whitespace())
    } else {
        source[..at as usize].ends_with(|it: char| !it.is_whitespace())
    };
    if touching { " " } else { "" }
}

/// What a call has to say about a block hanging off it.
struct Call {
    start: u32,
    end: u32,
    /// `true` where the call is written without parentheses.
    bare: bool,
    arguments: Option<(u32, u32)>,
    /// The block's opening and closing delimiters, when it has a block rather than a lambda.
    block: Option<((u32, u32), (u32, u32))>,
}

impl Call {
    /// A command call: no parentheses and at least one argument, which is the shape whose two
    /// block spellings do not mean the same thing.
    fn command(&self) -> bool {
        self.bare && self.arguments.is_some()
    }
}

/// Every call containing the selection, outermost first.
struct Calls {
    start: u32,
    end: u32,
    found: Vec<Call>,
}

impl Calls {
    fn of(parsed: &ParseResult<'_>, start: u32, end: u32) -> Vec<Call> {
        let mut walk = Calls {
            start,
            end,
            found: Vec::new(),
        };
        walk.visit(&parsed.node());
        walk.found
    }
}

impl<'pr> Visit<'pr> for Calls {
    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        let (start, end) = span(&node.location());
        if start <= self.start && self.end <= end {
            self.found.push(Call {
                start,
                end,
                bare: node.opening_loc().is_none(),
                arguments: node.arguments().map(|args| span(&args.location())),
                block: node
                    .block()
                    .and_then(|block| block.as_block_node())
                    .map(|block| (span(&block.opening_loc()), span(&block.closing_loc()))),
            });
        }
        ruby_prism::visit_call_node(self, node);
    }
}

// ---------------------------------------------------------------------------
// Extract to variable
// ---------------------------------------------------------------------------

/// Lift the selected expression onto a local of its own, in front of the statement it is in.
///
/// The rewrite is trivial and the *placement* is the difficulty. Hoisting an expression out of a
/// branch, a loop or the right of an `&&` changes how often it runs, so nothing between the
/// selection and the statement may be a construct that decides whether or how many times its
/// child is evaluated — and the statement itself has to be a line, which is what refuses the
/// two spellings that look like statements and are not: a ternary's arms and `foo.bar if baz`.
fn extract_variable(source: &str, parsed: &ParseResult<'_>, start: u32, end: u32) -> Vec<Action> {
    let Some((start, end)) = trim(source, start, end) else {
        return Vec::new();
    };
    if start == end {
        return Vec::new();
    }
    let chain = Chain::of(parsed, start, end);
    // Innermost, so that `foo(x)` with `x` selected extracts the argument and not the argument
    // list that happens to span the same bytes.
    let Some(selected) = chain
        .iter()
        .rposition(|step| step.value && (step.start, step.end) == (start, end))
    else {
        return Vec::new();
    };
    // The innermost statement list that is a real one, and then the child of it the path goes
    // through. A `#{}` body has already been demoted by `Chain::of`, so the search walks past
    // it rather than writing a line inside an interpolation.
    let statement = chain[..selected]
        .iter()
        .rposition(|step| step.shape == Shape::Statements)
        // Total, and the fallback answers "the selection is its own statement", which the guard
        // below then declines. A chain always holds the file's own statement list, so this is
        // where a selection Prism placed nowhere ends up rather than a case with anything to do.
        .map_or(selected, |at| at + 1);
    // The statement itself is in the range, and that is the half of this test that catches the
    // commonest wrong answer: in `user && user.name` the statement *is* the `&&`, so nothing is
    // crossed on the way out to it, and hoisting the right operand in front of it is what turns
    // a guard into a `NoMethodError`.
    if !chain[statement..selected]
        .iter()
        .all(|step| step.shape == Shape::Transparent)
    {
        return Vec::new();
    }
    // A selection that is already a whole statement has nowhere to go: `extracted = puts x`
    // followed by `extracted` is the same program written worse, and it would sit in the menu
    // beside the extraction that actually wants a whole statement.
    if statement == selected {
        return Vec::new();
    }
    let statement = &chain[statement];
    let Some((line, indent)) = span_owns_its_lines(source, statement.start, statement.end) else {
        return Vec::new();
    };
    // A heredoc's body follows the line its opener is on, so an opener that moves up a line
    // leaves the body behind. Nothing that fails to parse on its own may be lifted anywhere.
    if ruby_prism::parse(&source.as_bytes()[start as usize..end as usize])
        .errors()
        .next()
        .is_some()
    {
        return Vec::new();
    }

    let name = free_name(source, VARIABLE);
    vec![Action {
        title: format!("Extract into local variable `{name}`"),
        kind: Kind::Extract,
        edits: vec![
            Edit {
                start: line,
                end: line,
                text: format!(
                    "{indent}{name} = {}\n",
                    &source[start as usize..end as usize]
                ),
            },
            Edit {
                start,
                end,
                text: name,
            },
        ],
    }]
}

// ---------------------------------------------------------------------------
// Extract to method
// ---------------------------------------------------------------------------

/// Lift a run of whole statements into a method beside the one they are in.
///
/// The only one of the four that needs an analysis, and the analysis is
/// [`scopes::crossing`]: a local the run reads before it writes becomes a parameter, and a local
/// it writes that is touched afterwards has no single value the new method could hand back, so
/// that case is declined rather than approximated — passing no parameters at all is the trade
/// this module's opening paragraph refuses.
fn extract_method(source: &str, parsed: &ParseResult<'_>, start: u32, end: u32) -> Vec<Action> {
    let Some((start, end)) = trim(source, start, end) else {
        return Vec::new();
    };
    if start == end {
        return Vec::new();
    }
    let chain = Chain::of(parsed, start, end);
    // The run has to be whole statements: the innermost statement list containing the selection
    // must have a child starting exactly where it starts and one ending exactly where it ends.
    let Some(statements) = chain
        .iter()
        .rposition(|step| step.shape == Shape::Statements)
    else {
        return Vec::new();
    };
    let children = &chain[statements].children;
    if !children.iter().any(|&(at, _)| at == start) || !children.iter().any(|&(_, at)| at == end) {
        return Vec::new();
    }
    // Placed by the same rule the variable extraction uses, and here it does the same work: a
    // run that does not own its lines is one of the spellings that only looks like a statement.
    let Some((line, indent)) = span_owns_its_lines(source, start, end) else {
        return Vec::new();
    };

    // Searched over what is *outside* the statement list, which is what makes it the method the
    // run is inside rather than the method the run is: selecting a whole `def` finds no
    // enclosing one and is declined, where a search over the whole chain would find itself.
    let Some(enclosing) = chain[..statements]
        .iter()
        .rev()
        .find_map(|step| step.def.as_ref())
    else {
        return Vec::new();
    };
    let Some(closing) = enclosing.closing else {
        return Vec::new();
    };
    if !enclosing.own {
        return Vec::new();
    }
    let Some((_, outer)) = line_start(source, enclosing.start) else {
        return Vec::new();
    };

    // Everything the run does that an ordinary call cannot do: `return` and its relatives leave
    // the *new* method, `yield` has no block to reach, and `super` resolves against the new
    // name. A multi-line literal is refused because the body is re-indented, and re-indenting a
    // string changes what it says while leaving a file that still parses.
    if Inside::any(source, parsed, start, end) {
        return Vec::new();
    }
    let crossing = scopes::crossing(source, start, end);
    if crossing.escapes {
        return Vec::new();
    }
    if !crossing.reads.iter().all(|name| is_plain_parameter(name)) {
        return Vec::new();
    }
    let body = &source[line as usize..end as usize];
    if ruby_prism::parse(body.as_bytes()).errors().next().is_some() {
        return Vec::new();
    }

    let name = free_name(source, METHOD);
    let parameters = if crossing.reads.is_empty() {
        String::new()
    } else {
        format!("({})", crossing.reads.join(", "))
    };
    let moved = reindent(body, &indent, &format!("{outer}  "));
    vec![Action {
        title: format!("Extract into method `{name}`"),
        kind: Kind::Extract,
        edits: vec![
            Edit {
                start,
                end,
                text: format!("{name}{parameters}"),
            },
            Edit {
                start: closing,
                end: closing,
                text: format!(
                    "\n\n{outer}def {}{name}{parameters}\n{moved}\n{outer}end",
                    if enclosing.singleton { "self." } else { "" }
                ),
            },
        ],
    }]
}

/// A name that can be written as a positional parameter and read back unchanged.
///
/// `it` and `_1` are read at every occurrence and written at none, because Ruby supplies them
/// rather than the file declaring them — the same reason [`rename`](super::rename) refuses them.
fn is_plain_parameter(name: &str) -> bool {
    name != "it" && !name.starts_with('_')
}

/// Re-indent a run of whole lines from `from` to `to`.
///
/// Relative indentation is kept: every line moves by the same amount, measured from the
/// shallowest line in the run rather than from the first, so a nested `end` stays nested.
fn reindent(body: &str, from: &str, to: &str) -> String {
    body.lines()
        .map(|line| match line.strip_prefix(from) {
            // A blank line keeps nothing; a line shallower than the run's own indentation is
            // inside a literal, which `Inside` has already refused.
            Some(rest) if !rest.trim().is_empty() => format!("{to}{rest}"),
            _ => line.trim_end().to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether the selection holds anything that cannot be moved into a method of its own.
struct Inside<'s> {
    source: &'s str,
    start: u32,
    end: u32,
    found: bool,
}

impl Inside<'_> {
    fn any(source: &str, parsed: &ParseResult<'_>, start: u32, end: u32) -> bool {
        let mut walk = Inside {
            source,
            start,
            end,
            found: false,
        };
        walk.visit(&parsed.node());
        walk.found
    }
}

impl Inside<'_> {
    fn check(&mut self, node: &Node<'_>) {
        let (start, end) = span(&node.location());
        if start < self.start || self.end < end {
            return;
        }
        // Every escape a method boundary would change the meaning of. `return` and its relatives
        // would leave the *new* method, `yield` has no block to reach from one, and `super`
        // resolves against the name of the method it is written in.
        self.found |= matches!(
            node,
            Node::ReturnNode { .. }
                | Node::BreakNode { .. }
                | Node::NextNode { .. }
                | Node::RedoNode { .. }
                | Node::RetryNode { .. }
                | Node::YieldNode { .. }
                | Node::SuperNode { .. }
                | Node::ForwardingSuperNode { .. }
                | Node::ForwardingArgumentsNode { .. }
        );
        // A literal spelled over more than one line is refused because the body is re-indented
        // on the way out, and re-indenting a string changes what it says while leaving a file
        // that still parses — which is the one failure the parse gate above cannot see.
        self.found |= is_literal(node) && self.source[start as usize..end as usize].contains('\n');
    }
}

impl<'pr> Visit<'pr> for Inside<'_> {
    // Both hooks, and the leaf one is not padding: `redo`, `retry` and a plain string literal
    // have no children, so a visitor that only watched branches would let a multi-line string
    // through — and re-indenting one leaves a file that still parses and no longer says the
    // same thing, which is the one failure the parse gate cannot see.
    fn visit_branch_node_enter(&mut self, node: Node<'pr>) {
        self.check(&node);
    }

    fn visit_leaf_node_enter(&mut self, node: Node<'pr>) {
        self.check(&node);
    }
}

/// Whether a node is a literal whose own text is part of what it means.
fn is_literal(node: &Node<'_>) -> bool {
    matches!(
        node,
        Node::StringNode { .. }
            | Node::InterpolatedStringNode { .. }
            | Node::XStringNode { .. }
            | Node::InterpolatedXStringNode { .. }
            | Node::SymbolNode { .. }
            | Node::InterpolatedSymbolNode { .. }
            | Node::RegularExpressionNode { .. }
            | Node::InterpolatedRegularExpressionNode { .. }
            | Node::MatchLastLineNode { .. }
            | Node::InterpolatedMatchLastLineNode { .. }
    )
}

// ---------------------------------------------------------------------------
// The chain the two extractions are placed by
// ---------------------------------------------------------------------------

/// What crossing a node on the way out to a statement does to what it contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A list whose direct children are statements. Where the placement stops.
    Statements,
    /// A `#{}` interpolation. Its statement list is not a place a line can be written, so the
    /// search goes past it — the one reason this is a shape of its own.
    Embedded,
    /// Evaluated exactly once, unconditionally, when the statement around it runs.
    Transparent,
    /// Everything else, which is **the default**: a node this module has not thought about is
    /// one it declines to reach through. `&&`, a ternary's arms, a loop's condition, a
    /// parameter's default and `rescue`'s modifier form all land here, and each of them decides
    /// whether or how often its child runs.
    Opaque,
}

/// One node on the path from the file to the selection.
struct Step {
    shape: Shape,
    /// Whether the node on its own is a value a local could hold.
    value: bool,
    start: u32,
    end: u32,
    /// The span of each direct child, for [`Shape::Statements`] and nothing else.
    children: Vec<(u32, u32)>,
    /// What a `def` is, for the one action that has to write a second one beside it.
    def: Option<Def>,
}

/// The method a selection is inside.
struct Def {
    start: u32,
    /// Just past the `end` keyword, which is where a sibling method is written. `None` for an
    /// endless `def`, which has no statement list to extract from anyway.
    closing: Option<u32>,
    /// `def self.x`, which a method extracted out of it has to be too, or the call will not
    /// resolve.
    singleton: bool,
    /// `false` for `def obj.x`, where what the receiver is needs types.
    own: bool,
}

/// Every node containing the selection, outermost first.
///
/// Pre-order is what makes this the ancestor chain rather than a list that has to be sorted:
/// a node containing the selection is on the path to it, and a walk announces a parent before
/// its children.
struct Chain {
    start: u32,
    end: u32,
    found: Vec<Step>,
}

impl Chain {
    fn of(parsed: &ParseResult<'_>, start: u32, end: u32) -> Vec<Step> {
        let mut walk = Chain {
            start,
            end,
            found: Vec::new(),
        };
        walk.visit(&parsed.node());
        // A statement list that is a `#{}` body is not a place a line can go, so it is demoted
        // to an ordinary transparent step and the search walks on past it. Done here rather
        // than in the classification because it is the *parent* that decides it.
        for at in 1..walk.found.len() {
            if walk.found[at].shape == Shape::Statements
                && walk.found[at - 1].shape == Shape::Embedded
            {
                walk.found[at].shape = Shape::Transparent;
            }
        }
        walk.found
    }

    /// Record a node reached through one of the thirteen typed methods the generic hook never
    /// fires for.
    fn hole(&mut self, node: &Node<'_>) {
        let (start, end) = span(&node.location());
        let (shape, value) = classify(node);
        // `CallNode` and `ConstantPathNode` arrive both ways — the second only from a match
        // write and a constant-path assignment — so the same node can be announced twice in a
        // row. One `dedup` costs less than knowing which way it came, and it has to compare the
        // classification as well as the span: a file's `ProgramNode` and the statement list
        // inside it are the same bytes, and dropping the second would leave top-level code with
        // no statement list to be extracted from.
        if self.found.last().is_some_and(|last| {
            (last.start, last.end) == (start, end) && last.shape == shape && last.value == value
        }) {
            return;
        }
        self.take(node);
    }

    fn take(&mut self, node: &Node<'_>) {
        let (start, end) = span(&node.location());
        if start > self.start || self.end > end {
            return;
        }
        let (shape, value) = classify(node);
        self.found.push(Step {
            shape,
            value,
            start,
            end,
            children: Vec::new(),
            def: None,
        });
    }
}

impl<'pr> Visit<'pr> for Chain {
    fn visit_branch_node_enter(&mut self, node: Node<'pr>) {
        self.take(&node);
    }

    fn visit_leaf_node_enter(&mut self, node: Node<'pr>) {
        self.take(&node);
    }

    // The thirteen the generic hook never sees, announced here and then deferred to, exactly as
    // `ranges::Selection` does it. `BlockArgumentNode` is the one that arrives that way only
    // from an index assignment carrying a block, which Ruby's own parser rejects; it is written
    // anyway rather than left as the one hole a reader would have to rediscover.

    fn visit_statements_node(&mut self, node: &StatementsNode<'pr>) {
        let at = span(&node.location());
        self.hole(&node.as_node());
        // The span test is what ties the children to *this* node. A statement list that does
        // not contain the selection records nothing, and without the test its children would be
        // written onto whichever step happened to be last — which is the enclosing list, whose
        // own children are the ones the extraction is measured against.
        if let Some(step) = self.found.last_mut()
            && (step.start, step.end) == at
        {
            step.children = node.body().iter().map(|it| span(&it.location())).collect();
        }
        ruby_prism::visit_statements_node(self, node);
    }

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        self.hole(&node.as_node());
        ruby_prism::visit_call_node(self, node);
    }

    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        // Announced generically a moment ago, so this only annotates what is already there. The
        // span test is the whole of the condition and does the containment test with it: a step
        // exists only for a node that contains the selection, so a `def` that does not contain
        // it never matches the step on top.
        let (start, end) = span(&node.location());
        if let Some(step) = self.found.last_mut()
            && (step.start, step.end) == (start, end)
        {
            let receiver = node.receiver();
            step.def = Some(Def {
                start,
                closing: node.end_keyword_loc().map(|at| span(&at).1),
                singleton: matches!(receiver, Some(Node::SelfNode { .. })),
                own: receiver.is_none_or(|it| matches!(it, Node::SelfNode { .. })),
            });
        }
        ruby_prism::visit_def_node(self, node);
    }

    fn visit_arguments_node(&mut self, node: &ArgumentsNode<'pr>) {
        self.hole(&node.as_node());
        ruby_prism::visit_arguments_node(self, node);
    }

    fn visit_block_node(&mut self, node: &BlockNode<'pr>) {
        self.hole(&node.as_node());
        ruby_prism::visit_block_node(self, node);
    }

    fn visit_block_argument_node(&mut self, node: &BlockArgumentNode<'pr>) {
        self.hole(&node.as_node());
        ruby_prism::visit_block_argument_node(self, node);
    }

    fn visit_block_parameter_node(&mut self, node: &BlockParameterNode<'pr>) {
        self.hole(&node.as_node());
        ruby_prism::visit_block_parameter_node(self, node);
    }

    fn visit_else_node(&mut self, node: &ElseNode<'pr>) {
        self.hole(&node.as_node());
        ruby_prism::visit_else_node(self, node);
    }

    fn visit_ensure_node(&mut self, node: &EnsureNode<'pr>) {
        self.hole(&node.as_node());
        ruby_prism::visit_ensure_node(self, node);
    }

    fn visit_local_variable_target_node(&mut self, node: &LocalVariableTargetNode<'pr>) {
        self.hole(&node.as_node());
        ruby_prism::visit_local_variable_target_node(self, node);
    }

    fn visit_parameters_node(&mut self, node: &ParametersNode<'pr>) {
        self.hole(&node.as_node());
        ruby_prism::visit_parameters_node(self, node);
    }

    fn visit_rescue_node(&mut self, node: &RescueNode<'pr>) {
        self.hole(&node.as_node());
        ruby_prism::visit_rescue_node(self, node);
    }

    fn visit_splat_node(&mut self, node: &SplatNode<'pr>) {
        self.hole(&node.as_node());
        ruby_prism::visit_splat_node(self, node);
    }
}

/// How a node behaves when the placement reaches through it, and whether it is a value.
///
/// **Opaque and not a value is the default**, and that is the whole safety argument for this
/// table: a node kind nobody here has thought about declines both questions rather than being
/// assumed harmless. Prism has a hundred and fifty of them.
fn classify(node: &Node<'_>) -> (Shape, bool) {
    match node {
        Node::StatementsNode { .. } => (Shape::Statements, false),
        Node::EmbeddedStatementsNode { .. } => (Shape::Embedded, false),

        // Values that are also transparent: reaching through one of these evaluates it once,
        // where the statement runs, and lifting it out is the same program.
        Node::CallNode { .. }
        | Node::ArrayNode { .. }
        | Node::HashNode { .. }
        | Node::RangeNode { .. }
        | Node::ParenthesesNode { .. }
        | Node::InterpolatedStringNode { .. }
        | Node::InterpolatedSymbolNode { .. }
        | Node::InterpolatedXStringNode { .. }
        | Node::InterpolatedRegularExpressionNode { .. } => (Shape::Transparent, true),

        // Values that nothing reaches through, because they have no children worth selecting.
        Node::IntegerNode { .. }
        | Node::FloatNode { .. }
        | Node::RationalNode { .. }
        | Node::ImaginaryNode { .. }
        | Node::StringNode { .. }
        | Node::SymbolNode { .. }
        | Node::XStringNode { .. }
        | Node::RegularExpressionNode { .. }
        | Node::TrueNode { .. }
        | Node::FalseNode { .. }
        | Node::NilNode { .. }
        | Node::SelfNode { .. }
        | Node::LocalVariableReadNode { .. }
        | Node::InstanceVariableReadNode { .. }
        | Node::ClassVariableReadNode { .. }
        | Node::GlobalVariableReadNode { .. }
        | Node::ConstantReadNode { .. }
        | Node::ConstantPathNode { .. }
        | Node::LambdaNode { .. }
        | Node::DefinedNode { .. }
        | Node::AndNode { .. }
        | Node::OrNode { .. } => (Shape::Opaque, true),

        // Transparent and not a value. Each of these evaluates its children unconditionally as
        // the statement runs, and none of them is a thing a local could hold: an argument list,
        // a hash entry, the value side of an assignment, the operand of an escape.
        Node::ProgramNode { .. }
        | Node::ArgumentsNode { .. }
        | Node::AssocNode { .. }
        | Node::AssocSplatNode { .. }
        | Node::KeywordHashNode { .. }
        | Node::SplatNode { .. }
        | Node::ReturnNode { .. }
        | Node::BreakNode { .. }
        | Node::NextNode { .. }
        | Node::YieldNode { .. }
        | Node::SuperNode { .. }
        | Node::LocalVariableWriteNode { .. }
        | Node::InstanceVariableWriteNode { .. }
        | Node::ClassVariableWriteNode { .. }
        | Node::GlobalVariableWriteNode { .. }
        | Node::ConstantWriteNode { .. }
        | Node::ConstantPathWriteNode { .. }
        | Node::MultiWriteNode { .. }
        | Node::LocalVariableOperatorWriteNode { .. }
        | Node::InstanceVariableOperatorWriteNode { .. }
        | Node::ClassVariableOperatorWriteNode { .. }
        | Node::GlobalVariableOperatorWriteNode { .. }
        | Node::ConstantOperatorWriteNode { .. }
        | Node::IndexOperatorWriteNode { .. } => (Shape::Transparent, false),

        _ => (Shape::Opaque, false),
    }
}

/// Where a span's own line starts and what it is indented by — `None` unless the span begins a
/// line and ends one.
///
/// The test that refuses the two spellings which look like a statement and are not. `a ? b : c`
/// puts each arm in a statement list of its own, and so does `foo.bar if baz`; in both, lifting
/// a line out in front of "the statement" writes it where it will run when it should not, or
/// where Ruby cannot parse it. Neither arm begins its line and ends it, and nothing else that
/// matters fails to.
fn span_owns_its_lines(source: &str, start: u32, end: u32) -> Option<(u32, String)> {
    let (line, indent) = line_start(source, start)?;
    let rest = source[end as usize..]
        .split('\n')
        .next()
        .unwrap_or_default()
        .trim_start();
    // A trailing comment is still the statement's own line; anything else on it is not.
    (rest.is_empty() || rest.starts_with('#')).then_some((line, indent))
}

/// The offset the line holding `at` begins at, and the whitespace in front of `at` on it —
/// `None` when anything else is in front of it.
fn line_start(source: &str, at: u32) -> Option<(u32, String)> {
    let line = source[..at as usize]
        .rfind('\n')
        .map_or(0, |newline| newline + 1) as u32;
    let indent = &source[line as usize..at as usize];
    indent
        .chars()
        .all(char::is_whitespace)
        .then(|| (line, indent.to_owned()))
}

/// The whitespace in front of `at` on its line, or nothing when `at` does not begin one.
///
/// An insertion at the top of a class body reuses it twice: once for the line it writes and once
/// to put back the indentation of the statement it displaces. A body that opens mid-line —
/// `class Foo; def bar` — has none, and the result is ugly rather than wrong.
fn indent_before(source: &str, at: u32) -> String {
    line_start(source, at)
        .map(|(_, indent)| indent)
        .unwrap_or_default()
}

/// The selection with its whitespace taken off both ends, or `None` when it is inside out.
fn trim(source: &str, start: u32, end: u32) -> Option<(u32, u32)> {
    let text = source.get(start as usize..end as usize)?;
    let front = text.len() - text.trim_start().len();
    let back = text.len() - text.trim_end().len();
    Some((start + front as u32, end - back as u32))
}

/// `base`, or the first `base_2`, `base_3`… the file does not already write.
///
/// A whole-word search over the source rather than a scope walk, and deliberately over-strict:
/// a mention in a comment is enough to move on to the next name. A name that is free because
/// nothing in the file spells it cannot shadow a local, a method called without a receiver, or
/// anything else — which is a property of the search rather than of a list of the things it
/// would have had to enumerate.
fn free_name(source: &str, base: &str) -> String {
    let mut nth = 1;
    loop {
        let candidate = if nth == 1 {
            base.to_owned()
        } else {
            format!("{base}_{nth}")
        };
        if !mentions(source, &candidate) {
            return candidate;
        }
        nth += 1;
    }
}

/// Whether `source` writes `word` as a whole word anywhere.
fn mentions(source: &str, word: &str) -> bool {
    let boundary = |at: Option<char>| at.is_none_or(|it| !it.is_alphanumeric() && it != '_');
    source.match_indices(word).any(|(at, _)| {
        boundary(source[..at].chars().next_back())
            && boundary(source[at + word.len()..].chars().next())
    })
}

fn span(at: &Location<'_>) -> (u32, u32) {
    (at.start_offset() as u32, at.end_offset() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The selection a fixture marks: `~` twice for a range, once for a bare cursor.
    fn marked(source: &str) -> (String, u32, u32) {
        let start = source.find('~').expect("a ~ marking the selection") as u32;
        let rest = source.replacen('~', "", 1);
        let end = rest[start as usize..]
            .find('~')
            .map_or(start, |at| start + at as u32);
        (rest.replacen('~', "", 1), start, end)
    }

    fn offered(source: &str) -> Vec<Action> {
        let (source, start, end) = marked(source);
        at(&source, start, end)
    }

    /// Whether anything offered over a selection is one of a family, by a word in its title.
    ///
    /// One helper rather than a closure at each site, and for a reason worth naming: a
    /// `!offers(…).iter().any(…)` over a list that is *meant* to be empty never runs its own
    /// closure, so a module held to every line would be held to lines the assertions guarantee
    /// nothing ever reaches.
    fn offers(source: &str, family: &str) -> bool {
        titles(source).iter().any(|title| title.contains(family))
    }

    fn titles(source: &str) -> Vec<String> {
        offered(source)
            .into_iter()
            .map(|action| action.title)
            .collect()
    }

    /// The file as choosing one of the offered actions leaves it.
    ///
    /// **The drawing is the rewritten Ruby**, which is `renaming.md`'s rule and is here for its
    /// reason: a span one byte out writes visibly broken code — a name run into the one beside
    /// it, an `end` eaten, a block delimiter left unmatched — where a list of offsets shows
    /// nobody anything.
    fn applied(source: &str, title: &str) -> String {
        let (source, start, end) = marked(source);
        let actions = at(&source, start, end);
        // The message is built before it is needed rather than in a closure, so that a helper in
        // a module held to every line does not carry two lines no test ever runs.
        let offered: Vec<&str> = actions.iter().map(|it| it.title.as_str()).collect();
        let missing = format!("no action titled {title:?}; offered {offered:?}");
        let action = actions
            .iter()
            .find(|action| action.title == title)
            .expect(&missing);
        let mut edited = source.clone();
        let mut edits = action.edits.clone();
        edits.sort_by_key(|edit| std::cmp::Reverse(edit.start));
        for edit in edits {
            edited.replace_range(edit.start as usize..edit.end as usize, &edit.text);
        }
        edited
    }

    // -----------------------------------------------------------------------
    // attr_reader / attr_writer / attr_accessor
    // -----------------------------------------------------------------------

    #[test]
    fn an_instance_variable_offers_the_three_accessors_at_the_top_of_its_class() {
        let source = "class Story\n  def bump\n    ~@views = @views + 1\n  end\nend\n";
        assert_eq!(
            titles(source),
            [
                "Declare attr_reader :views",
                "Declare attr_writer :views",
                "Declare attr_accessor :views",
            ]
        );
        assert_eq!(
            applied(source, "Declare attr_accessor :views"),
            "class Story\n  attr_accessor :views\n  def bump\n    @views = @views + 1\n  end\nend\n"
        );
        // A module is a namespace like any other, and the cursor may be on a read rather than
        // on the assignment.
        assert_eq!(
            applied(
                "module Sized\n  def big?\n    @size~ > 10\n  end\nend\n",
                "Declare attr_reader :size"
            ),
            "module Sized\n  attr_reader :size\n  def big?\n    @size > 10\n  end\nend\n"
        );
    }

    #[test]
    fn an_accessor_is_not_offered_where_it_would_read_a_different_variable() {
        // The whole of what makes this action exact, and every one of these is an `@count` a
        // regular expression would have offered for. An `attr_reader :count` declares an
        // instance method reading an *instance's* `@count`: on the class object it would return
        // `nil` and look right, and at the top level there is no class body to write it into.
        for source in [
            "class Foo\n  def self.count\n    ~@count\n  end\nend\n",
            "class Foo\n  class << self\n    def count\n      ~@count\n    end\n  end\nend\n",
            "class Foo\n  @count = ~0\nend\n",
            "def count\n  ~@count\nend\n",
            "obj = Object.new\nclass << obj\n  def count\n    ~@count\n  end\nend\n",
        ] {
            assert!(
                titles(source).is_empty(),
                "offered something for {source:?}"
            );
        }
    }

    #[test]
    fn an_accessor_the_class_already_declares_is_not_offered_again() {
        let body = "\n  def bump\n    ~@count = 1\n  end\nend\n";
        for (declared, expected) in [
            ("", vec!["attr_reader", "attr_writer", "attr_accessor"]),
            (
                "  attr_reader :count\n",
                vec!["attr_writer", "attr_accessor"],
            ),
            (
                "  attr_writer :count\n",
                vec!["attr_reader", "attr_accessor"],
            ),
            ("  attr_accessor :count\n", vec![]),
            ("  attr_reader :count\n  attr_writer :count\n", vec![]),
            // Everything a body can hold that is not a declaration of this name: another
            // name, no name at all, a name that is not a symbol, a receiver, another macro,
            // and a statement that is not a call.
            (
                "  attr_reader :other\n",
                vec!["attr_reader", "attr_writer", "attr_accessor"],
            ),
            (
                "  attr_reader\n",
                vec!["attr_reader", "attr_writer", "attr_accessor"],
            ),
            (
                "  attr_reader count\n",
                vec!["attr_reader", "attr_writer", "attr_accessor"],
            ),
            (
                "  self.attr_reader :count\n",
                vec!["attr_reader", "attr_writer", "attr_accessor"],
            ),
            (
                "  include Countable\n",
                vec!["attr_reader", "attr_writer", "attr_accessor"],
            ),
            (
                "  COUNT = 1\n",
                vec!["attr_reader", "attr_writer", "attr_accessor"],
            ),
            // Nested, and conditional: neither is a declaration of *this* class's accessor,
            // so neither is a reason to withhold the offer.
            (
                "  class Inner\n    attr_accessor :count\n  end\n",
                vec!["attr_reader", "attr_writer", "attr_accessor"],
            ),
            (
                "  attr_accessor :count if false\n",
                vec!["attr_reader", "attr_writer", "attr_accessor"],
            ),
        ] {
            let source = format!("class Empty\nend\nclass Foo\n{declared}{body}");
            let offered: Vec<String> = titles(&source)
                .iter()
                .map(|title| title.replace("Declare ", "").replace(" :count", ""))
                .collect();
            assert_eq!(offered, expected, "for a class declaring {declared:?}");
        }
    }

    #[test]
    fn a_class_body_that_does_not_begin_a_line_is_written_to_anyway() {
        // Ugly rather than wrong, which is the trade the whole action is built on: the
        // insertion cannot change what any existing line means, so the worst case is a line
        // break where a reader would not have put one.
        assert_eq!(
            applied(
                "class Foo; def bar; ~@count; end; end\n",
                "Declare attr_reader :count"
            ),
            "class Foo; attr_reader :count\ndef bar; @count; end; end\n"
        );
    }

    // -----------------------------------------------------------------------
    // Toggle block style
    // -----------------------------------------------------------------------

    #[test]
    fn a_block_toggles_between_its_two_spellings() {
        assert_eq!(
            applied("[1, 2].each { |n| ~puts n }\n", "Convert to a do…end block"),
            "[1, 2].each do |n| puts n end\n"
        );
        assert_eq!(
            applied(
                "[1, 2].each do |n|\n  ~puts n\nend\n",
                "Convert to a { } block"
            ),
            "[1, 2].each { |n|\n  puts n\n}\n"
        );
        // A cursor on the call rather than inside the block, and the innermost block wins when
        // one is nested in another.
        assert_eq!(
            applied("[1].ea~ch { puts 1 }\n", "Convert to a do…end block"),
            "[1].each do puts 1 end\n"
        );
        assert_eq!(
            applied(
                "outer do\n  inner { ~puts 1 }\nend\n",
                "Convert to a do…end block"
            ),
            "outer do\n  inner do puts 1 end\nend\n"
        );
    }

    #[test]
    fn the_delimiters_are_spaced_apart_from_what_they_touch() {
        assert_eq!(
            applied("[1].each {|n| ~n}\n", "Convert to a do…end block"),
            "[1].each do |n| n end\n"
        );
        assert_eq!(
            applied("[1].each do |n| ~n end\n", "Convert to a { } block"),
            "[1].each { |n| n }\n"
        );
    }

    #[test]
    fn a_block_on_a_command_call_is_left_alone() {
        // Ruby's two block delimiters do not bind the same way, and the difference is not
        // theoretical: with braces the block belongs to `map`, and with `do…end` it belongs to
        // the command call outside it — which still parses, still runs, and prints something
        // else. The `it "works" do` case is the same test catching the other failure: the brace
        // form of that one is not a different program but a syntax error.
        for source in [
            "def show(x) = x\nputs show [1, 2].map { |n| ~n * 2 }\n",
            "def show(x) = x\nputs show [1, 2].map do |n| ~n * 2 end\n",
            "it \"works\" do\n  ~expect(1).to eq(1)\nend\n",
            "raise Error, [1].map { ~1 }.first\n",
        ] {
            assert!(!offers(source, "block"), "offered a toggle for {source:?}");
        }
        // A command call the block's own call is not an *argument* of is no reason to refuse:
        // here the block hangs off the receiver, where neither spelling can move it.
        assert_eq!(
            applied("foo { ~1 }.bar baz\n", "Convert to a do…end block"),
            "foo do 1 end.bar baz\n"
        );
        // And parentheses settle it whatever the arguments are.
        assert_eq!(
            applied("puts([1].map { ~1 })\n", "Convert to a do…end block"),
            "puts([1].map do 1 end)\n"
        );
    }

    #[test]
    fn a_position_with_no_block_around_it_offers_no_toggle() {
        assert!(!offers("x = ~1\n", "block"));
        // A lambda is not a block: `->() {}` has both spellings too, and swapping them is a
        // different question with a different node behind it.
        assert!(!offers("f = -> { ~1 }\n", "block"));
    }

    // -----------------------------------------------------------------------
    // Extract to variable
    // -----------------------------------------------------------------------

    #[test]
    fn an_expression_is_lifted_onto_a_local_in_front_of_its_statement() {
        assert_eq!(
            applied(
                "def f\n  puts ~story.title~\nend\n",
                "Extract into local variable `extracted`"
            ),
            "def f\n  extracted = story.title\n  puts extracted\nend\n"
        );
        // The right of an assignment, an argument in the middle of a list, and a receiver in a
        // chain: three places the statement is not the selection and the insertion is still one
        // line above it, at the statement's own indentation.
        assert_eq!(
            applied(
                "def f\n  x = ~a.b~\nend\n",
                "Extract into local variable `extracted`"
            ),
            "def f\n  extracted = a.b\n  x = extracted\nend\n"
        );
        assert_eq!(
            applied(
                "def f\n  go(a: ~x.y~, b: 2)\nend\n",
                "Extract into local variable `extracted`"
            ),
            "def f\n  extracted = x.y\n  go(a: extracted, b: 2)\nend\n"
        );
        assert_eq!(
            applied(
                "def f\n  ~a.b~.c\nend\n",
                "Extract into local variable `extracted`"
            ),
            "def f\n  extracted = a.b\n  extracted.c\nend\n"
        );
    }

    #[test]
    fn the_selection_stays_where_it_would_run_the_same_number_of_times() {
        // Placement is the whole item. Each of these has a statement the expression could be
        // hoisted in front of, and in each the hoist would change the program: it would be
        // evaluated when the guard says it should not be, on every turn of a loop rather than
        // once, at a different time, or where Ruby cannot parse a line at all.
        for source in [
            "def f\n  user && ~user.name~\nend\n",
            "def f\n  a ? ~b.c~ : d\nend\n",
            "def f\n  ~foo.bar~ if baz\nend\n",
            "def f\n  while ~foo.bar~\n    x\n  end\nend\n",
            "def f\n  until ~foo.bar~\n    x\n  end\nend\n",
            "def f(x = ~compute.now~)\n  x\nend\n",
            "def f\n  @x ||= ~foo.bar~\nend\n",
            "def f\n  foo.bar(&~:to_s~)\nend\n",
            "def f\n  ~foo.bar~ rescue nil\nend\n",
            "def f\n  puts \"got #{~foo.bar~}\"\nend\n",
        ] {
            assert!(
                !offers(source, "local"),
                "offered an extraction for {source:?}"
            );
        }
        // Inside a block and inside a branch it stays inside, because the statement list it
        // lands in is the block's or the branch's own.
        assert_eq!(
            applied(
                "def f\n  [1].each do |n|\n    puts ~n.to_s~\n  end\nend\n",
                "Extract into local variable `extracted`"
            ),
            "def f\n  [1].each do |n|\n    extracted = n.to_s\n    puts extracted\n  end\nend\n"
        );
        assert_eq!(
            applied(
                "def f\n  if c\n    puts ~a.b~\n  end\nend\n",
                "Extract into local variable `extracted`"
            ),
            "def f\n  if c\n    extracted = a.b\n    puts extracted\n  end\nend\n"
        );
    }

    #[test]
    fn a_selection_that_is_not_one_expression_is_not_extracted() {
        for source in [
            // A cursor rather than a selection.
            "def f\n  puts ~foo.bar\nend\n",
            // Half of a name, and half of a chain.
            "def f\n  puts ~foo.ba~r\nend\n",
            "def f\n  puts ~foo.~bar\nend\n",
            // Two statements: no node spans them, and the other extraction is the one for it.
            "def f\n  ~a\n  b~\nend\n",
            // A whole statement, which would produce `extracted = puts x` and then `extracted`.
            "def f\n  ~foo.bar~\nend\n",
            // A splat and a target: neither is a value a local can hold.
            "def f\n  go(~*args~)\nend\n",
            "def f\n  ~x~ = 1\nend\n",
        ] {
            assert!(
                !offers(source, "local"),
                "offered an extraction for {source:?}"
            );
        }
    }

    #[test]
    fn nothing_that_does_not_parse_on_its_own_is_lifted_anywhere() {
        // A heredoc's body follows the line its opener is written on, so an opener that moves up
        // a line leaves the body behind — and the opener alone does not parse, which is what
        // this refuses on rather than on a rule about heredocs.
        assert!(!offers(
            "def f\n  puts ~<<-TEXT~\n    hi\n  TEXT\nend\n",
            "local"
        ));
        // The same gate catching something that is not a heredoc at all: a hash key's symbol is
        // a node whose own text is `a:`, and `a:` is not a program.
        assert!(!offers("def f\n  x = { ~a:~ 1 }\nend\n", "local"));
        // And in the run an extraction takes: the opener is a whole statement, its body is not.
        assert!(!offers(
            "def f\n  x = 1\n  if x\n    ~puts(<<-TEXT)~\n      hi\n    TEXT\n  end\nend\n",
            "method"
        ));
    }

    #[test]
    fn the_new_name_is_one_the_file_does_not_already_write() {
        assert_eq!(
            applied(
                "def f\n  extracted = 1\n  puts ~foo.bar~\nend\n",
                "Extract into local variable `extracted_2`"
            ),
            "def f\n  extracted = 1\n  extracted_2 = foo.bar\n  puts extracted_2\nend\n"
        );
        // A whole-word search, so a longer name that merely contains the candidate is not a
        // reason to move on — and a mention in a comment is, deliberately.
        assert_eq!(
            titles("def f\n  extracted_thing = 1\n  puts ~foo.bar~\nend\n"),
            ["Extract into local variable `extracted`"]
        );
        assert_eq!(
            titles("def f\n  # extracted\n  puts ~foo.bar~\nend\n"),
            ["Extract into local variable `extracted_2`"]
        );
        // Both edges of "whole word", and both kinds of neighbour: a letter and an underscore.
        for spelling in [
            "zextracted",
            "extractedz",
            "extracted_thing",
            "thing_extracted",
        ] {
            assert_eq!(
                titles(&format!("def f\n  {spelling} = 1\n  puts ~foo.bar~\nend\n")),
                ["Extract into local variable `extracted`"],
                "{spelling} is not the word `extracted`"
            );
        }
        // And a mention at the very start of the file, where there is no character in front.
        assert_eq!(
            titles("extracted = 1\ndef f\n  puts ~foo.bar~\nend\n"),
            ["Extract into local variable `extracted_2`"]
        );
    }

    // -----------------------------------------------------------------------
    // Extract to method
    // -----------------------------------------------------------------------

    #[test]
    fn a_run_of_statements_becomes_a_method_beside_the_one_it_was_in() {
        assert_eq!(
            applied(
                "def report\n  total = count\n  ~puts total\n  puts total * 2~\n  total\nend\n",
                "Extract into method `extracted_method`"
            ),
            "def report\n  total = count\n  extracted_method(total)\n  total\nend\n\
             \ndef extracted_method(total)\n  puts total\n  puts total * 2\nend\n"
        );
        // Nothing borrowed, so no parameters and no parentheses on either side.
        assert_eq!(
            applied(
                "def f\n  ~puts 1~\nend\n",
                "Extract into method `extracted_method`"
            ),
            "def f\n  extracted_method\nend\n\ndef extracted_method\n  puts 1\nend\n"
        );
    }

    #[test]
    fn the_locals_the_run_reads_from_outside_become_its_parameters() {
        // An extraction that slices the selection out verbatim and passes nothing produces a
        // method that raises `NameError` on its first call.
        assert_eq!(
            applied(
                "def f\n  a = 1\n  b = 2\n  ~puts a + b\n  c = a * 2\n  puts c~\nend\n",
                "Extract into method `extracted_method`"
            ),
            "def f\n  a = 1\n  b = 2\n  extracted_method(a, b)\nend\n\
             \ndef extracted_method(a, b)\n  puts a + b\n  c = a * 2\n  puts c\nend\n"
        );
        // A block parameter declared outside the run is a local like any other, and one
        // declared inside it is not borrowed at all.
        assert_eq!(
            applied(
                "def f\n  [1].each do |n|\n    ~puts n~\n  end\nend\n",
                "Extract into method `extracted_method`"
            ),
            "def f\n  [1].each do |n|\n    extracted_method(n)\n  end\nend\n\
             \ndef extracted_method(n)\n  puts n\nend\n"
        );
        assert_eq!(
            applied(
                "def f\n  ~[1].each { |n| puts n }~\nend\n",
                "Extract into method `extracted_method`"
            ),
            "def f\n  extracted_method\nend\n\ndef extracted_method\n  [1].each { |n| puts n }\nend\n"
        );
    }

    #[test]
    fn a_local_the_run_writes_and_the_rest_of_the_method_reads_is_declined() {
        // There is no single value the new method could hand back for it, and approximating is
        // the trade this module refuses. Both spellings of "touched afterwards" count.
        for source in [
            "def f\n  ~y = 1~\n  puts y\nend\n",
            "def f\n  ~y = 1~\n  y += 1\nend\n",
            "def f\n  y = 0\n  ~y = 1~\n  puts y\nend\n",
        ] {
            assert!(
                !offers(source, "method"),
                "offered an extraction for {source:?}"
            );
        }
        // A local the run writes and nothing afterwards reads is simply the new method's own.
        assert_eq!(
            applied(
                "def f\n  ~y = 1\n  puts y~\n  puts 2\nend\n",
                "Extract into method `extracted_method`"
            ),
            "def f\n  extracted_method\n  puts 2\nend\n\
             \ndef extracted_method\n  y = 1\n  puts y\nend\n"
        );
    }

    #[test]
    fn nothing_that_would_mean_something_else_inside_a_method_is_extracted() {
        // `return` and its relatives would leave the *new* method, `yield` has no block to
        // reach from one, and `super` resolves against the name of the method it is written in.
        for escape in [
            "return 1", "break", "next", "redo", "retry", "yield 1", "super", "super(1)",
        ] {
            let source = format!("def f\n  [1].each do\n    ~{escape}~\n  end\nend\n");
            assert!(
                !offers(&source, "method"),
                "offered an extraction for {escape:?}"
            );
        }
    }

    #[test]
    fn a_literal_spelled_over_more_than_one_line_is_not_re_indented() {
        // The one failure the parse gate cannot see: a string that is moved and re-indented
        // still parses, and no longer says what it said.
        assert!(!offers(
            "def f\n  if c\n    ~puts \"one\ntwo\"~\n  end\nend\n",
            "method"
        ));
    }

    #[test]
    fn the_extracted_body_keeps_its_shape_at_the_new_depth() {
        // Every line moves by the same amount, measured from the shallowest line in the run, so
        // the nesting inside it survives — and a blank line stays blank rather than becoming
        // whitespace.
        assert_eq!(
            applied(
                "def f\n  x = 1\n  if x\n    ~puts x\n    \n    [1].each do |n|\n      puts n\n    end~\n  end\nend\n",
                "Extract into method `extracted_method`"
            ),
            "def f\n  x = 1\n  if x\n    extracted_method(x)\n  end\nend\n\
             \ndef extracted_method(x)\n  puts x\n\n  [1].each do |n|\n    puts n\n  end\nend\n"
        );
    }

    #[test]
    fn a_singleton_method_extracts_into_a_singleton_method() {
        // Otherwise the call does not resolve: the extracted body runs where `self` is the
        // class object, and an instance method is not reachable from there.
        assert_eq!(
            applied(
                "class Foo\n  def self.f\n    x = 1\n    ~puts x~\n  end\nend\n",
                "Extract into method `extracted_method`"
            ),
            "class Foo\n  def self.f\n    x = 1\n    extracted_method(x)\n  end\n\
             \n  def self.extracted_method(x)\n    puts x\n  end\nend\n"
        );
        // `def obj.f` is an island — what the receiver is needs types — and an endless `def`
        // has no `end` to write beside and no statement list to take a run from.
        assert!(!offers(
            "obj = Object.new\ndef obj.f\n  x = 1\n  ~puts x~\nend\n",
            "method"
        ));
        assert!(!offers("def f = ~puts 1~\n", "method"));
    }

    #[test]
    fn a_run_outside_a_method_has_nowhere_to_extract_to() {
        for source in [
            "~puts 1~\n",
            "class Foo\n  ~include Bar~\nend\n",
            "def f\n  puts ~1~\nend\n",
            "def f\n  a ? ~b~ : c\nend\n",
            "def f\n  ~puts 1~ if x\nend\n",
        ] {
            assert!(
                !offers(source, "method"),
                "offered an extraction for {source:?}"
            );
        }
    }

    #[test]
    fn a_parameter_the_run_would_need_and_cannot_be_written_is_declined() {
        // `it` and `_1` are read at every occurrence and written at none, because Ruby supplies
        // them rather than the file declaring them — the same reason `rename` refuses them.
        for source in [
            "def f\n  [1].each do\n    ~puts it~\n  end\nend\n",
            "def f\n  [1].each do\n    ~puts _1~\n  end\nend\n",
        ] {
            assert!(
                !offers(source, "method"),
                "offered an extraction for {source:?}"
            );
        }
        // Two locals spelled alike are *not* a case, and working out why is what removed a
        // guard rather than adding one: a run lies inside one scope, an inner `n` shadows an
        // outer one for the whole of it, and a block inside the run that declares its own `n`
        // writes it before it reads it. So only one variable per name can ever be borrowed —
        // and if that reasoning is wrong, `def m(n, n)` is a syntax error, which is the last
        // gate's job rather than a guard's.
        assert_eq!(
            titles("def f\n  n = 1\n  ~[2].each { |n| puts n }\n  puts n~\nend\n"),
            ["Extract into method `extracted_method`"]
        );
    }

    // -----------------------------------------------------------------------
    // The gates every action goes through
    // -----------------------------------------------------------------------

    #[test]
    fn a_statement_that_does_not_own_its_line_is_not_written_in_front_of() {
        // The same selection one line down, where there is a line to write on.
        assert!(offers(
            "def f\n  [1].each do\n    puts ~x.y~\n  end\nend\n",
            "local"
        ));
        // A one-line block: there is no line above `puts x.y` that is still inside the block,
        // so the only placement available would run the expression once per file rather than
        // once per iteration.
        assert!(!offers("def f\n  [1].each { puts ~x.y~ }\nend\n", "local"));
    }

    #[test]
    fn a_method_with_no_end_and_a_method_that_shares_a_line_are_both_declined() {
        // An endless `def` has no `end` to write a sibling beside — and it can still hold a
        // statement list, which is what makes this a case rather than an impossibility.
        assert!(!offers("def f = (\n  ~puts 1~\n  puts 2\n)\n", "method"));
        // A `def` that does not begin its line has no indentation to write the sibling at.
        assert!(!offers("1; def f\n  ~puts 1~\nend\n", "method"));
        // And a `def` the selection is not inside is not the enclosing one.
        assert_eq!(
            applied(
                "def other\n  puts 0\nend\n\ndef f\n  ~puts 1~\nend\n",
                "Extract into method `extracted_method`"
            ),
            "def other\n  puts 0\nend\n\ndef f\n  extracted_method\nend\n\
             \ndef extracted_method\n  puts 1\nend\n"
        );
    }

    #[test]
    fn a_run_inside_a_begin_block_is_extracted_like_any_other() {
        // Four of the thirteen typed hooks are only reached through spellings no other fixture
        // here writes — a block parameter, a destructuring assignment, `rescue` and `ensure` —
        // and a walk that stopped announcing one of them would leave a hole in the chain that
        // nothing else would show.
        assert_eq!(
            applied(
                "def f(&blk)\n  a, b = 1, 2\n  begin\n    ~puts a~\n  rescue => e\n    puts e\n  ensure\n    puts b\n  end\nend\n",
                "Extract into method `extracted_method`"
            ),
            "def f(&blk)\n  a, b = 1, 2\n  begin\n    extracted_method(a)\n  rescue => e\n    puts e\n  ensure\n    puts b\n  end\nend\n\
             \ndef extracted_method(a)\n  puts a\nend\n"
        );
    }

    #[test]
    fn a_selection_no_statement_list_holds_is_answered_with_nothing() {
        // A file that is one comment: Prism's program covers no bytes, so nothing at all
        // contains the selection and every walk here starts empty rather than at a root.
        assert!(at("# hi there\n", 2, 4).is_empty());
        // The same, with a `def` in the file: the walk reaches one with nothing recorded yet,
        // which is the only way a node it is meant to annotate is not the one on top.
        assert!(at("# hi there\ndef f\nend\n", 2, 4).is_empty());
    }

    #[test]
    fn an_action_is_a_title_a_kind_and_a_list_of_spans() {
        // The whole of what one is, pinned once. Everything else here draws the file the edits
        // produce, which is the right assertion for a rewrite and says nothing about the shape
        // the caller converts — two spans, one of them empty, and which menu it belongs in.
        let (source, start, end) = marked("def f\n  puts ~a.b~\nend\n");
        assert_eq!(
            at(&source, start, end),
            [Action {
                title: "Extract into local variable `extracted`".to_owned(),
                kind: Kind::Extract,
                edits: vec![
                    Edit {
                        start: 6,
                        end: 6,
                        text: "  extracted = a.b\n".to_owned(),
                    },
                    Edit {
                        start: 13,
                        end: 16,
                        text: "extracted".to_owned(),
                    },
                ],
            }]
        );
    }

    #[test]
    fn nothing_is_offered_where_the_file_does_not_parse() {
        // Recovery hands out spans that do not nest, and an edit placed by one of those lands
        // somewhere else in the buffer.
        assert!(titles("def f\n  puts ~story.title~\n").is_empty());
    }

    #[test]
    fn an_action_whose_result_would_not_parse_is_dropped_before_it_is_offered() {
        // The last gate, and it is the one no guard above it can stand in for: it does not know
        // *why* an edit is wrong, only that the buffer would stop being Ruby. Asked here of an
        // action built by hand, because the guards above are what keep the four from producing
        // one — which is the point of having them.
        let source = "def f\n  puts 1\nend\n";
        let sound = Action {
            title: String::new(),
            kind: Kind::Rewrite,
            edits: vec![Edit {
                start: 9,
                end: 13,
                text: "warn".to_owned(),
            }],
        };
        let broken = Action {
            title: String::new(),
            kind: Kind::Rewrite,
            edits: vec![Edit {
                start: 16,
                end: 19,
                text: String::new(),
            }],
        };
        assert!(survives(source, &sound));
        assert!(!survives(source, &broken));
    }

    #[test]
    fn a_range_the_document_does_not_hold_answers_with_nothing() {
        // Clamped rather than trusted: a client may ask about a position in a buffer it has
        // already changed, and a slice past the end of a `String` panics on the analysis
        // thread, where a panic is a server that stops answering anything at all.
        assert!(at("def f\nend\n", 0, 9_999).is_empty());
        assert!(at("def f\nend\n", 9_999, 9_999).is_empty());
        // Inside out, which no editor sends and nothing here may assume it will not.
        assert!(at("def f\n  puts 1\nend\n", 13, 9).is_empty());
    }

    #[test]
    fn a_selection_of_nothing_but_whitespace_is_not_a_selection() {
        assert!(at("def f\n  puts 1\nend\n", 6, 8).is_empty());
    }
}
