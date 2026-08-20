//! What the cursor is in the middle of typing.
//!
//! # Why this parses instead of reading the graph
//!
//! Every other feature answers a question about text the user has finished writing, and rubydex's
//! graph is the record of that. Completion is the opposite: it fires on text that is, by
//! definition, half-written and usually not valid Ruby. `Foo::` is a syntax error. So is `foo.`.
//!
//! Prism recovers from both, and the recovery is precise enough to classify the cursor: `Foo::`
//! becomes a `ConstantPathNode` whose name is missing and whose empty name span sits exactly
//! where the cursor is, and `foo.` becomes a `CallNode` with an empty message span in the same
//! place. That is the whole trick — the shapes below are read off Prism's error recovery rather
//! than reconstructed from the raw text.
//!
//! Scanning the text backwards from the cursor would be simpler and would fire inside comments,
//! inside strings, and on the `.` of a decimal literal. It is used here for exactly one thing —
//! finding where the half-typed word starts — and only after Prism has established that the
//! cursor is in a place where Ruby code can be written at all.
//!
//! # Why this module has no graph
//!
//! What the receiver *is* takes a graph; where the receiver is *written* does not. Keeping the
//! split here means the classification can be tested against nothing but a string, which is the
//! only way the awkward cases (a trailing `.` on the line above an `end`, a cursor in the
//! whitespace after a comma) are cheap enough to enumerate.

use ruby_prism::{
    CallNode, ConstantPathNode, LocalVariableWriteNode, Location, MatchLastLineNode, Node,
    ParseResult, RegularExpressionNode, StringNode, SymbolNode, Visit, XStringNode,
};

/// What the cursor is positioned to complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    /// A bare word, or nothing at all: everything reachable from here.
    Expression,
    /// After `::`, as in `Foo::` or `Foo::Ba`.
    NamespaceAccess { receiver: Receiver },
    /// After `.` or `&.`, as in `foo.`, `Foo.ba`, `self.`.
    MethodCall { receiver: Receiver },
    /// Inside an argument list, as in `foo(`, `bar(1, `. Everything an expression offers, plus
    /// the called method's keyword parameters — so it carries an offset inside that method's
    /// name for the caller to resolve.
    Argument { name: u32 },
}

/// The thing to the left of the `.` or the `::`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Receiver {
    /// A constant path. The offset is inside its last segment, which is where the graph files
    /// the resolved reference — `HR::Person.` points into `Person`, not into `HR`.
    Constant(u32),
    /// An *instance* of a constant: `Foo.new.`, or a local holding one. The offset means what it
    /// means for `Constant`; what differs is which side of the class is being asked about.
    Instance(u32),
    /// A literal, named by the class Ruby gives it. This is not inference: the parser has
    /// already decided that `"x"` is a `String` and `[1]` an `Array`, and reading the node kind
    /// is reading that decision back.
    Literal(&'static str),
    /// A literal `self`.
    SelfObject,
    /// Nothing at all, as in `::Foo`: the receiver is the top-level scope.
    TopLevel,
    /// An instance variable, a method's return value, a local assigned something we do not
    /// follow: a type it would take real inference to know, and ya-lsp has none.
    Unknown,
}

/// A classified cursor, and the half-typed word it sits at the end of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub context: Context,
    /// The span a completion replaces. Empty when the cursor is not inside a word, which is the
    /// usual case immediately after typing `.`.
    pub start: u32,
    pub end: u32,
}

/// What the cursor at `offset` is completing.
///
/// `None` where Ruby cannot be written — inside a comment, or inside a string, symbol or regexp
/// literal. Suggesting constants in the middle of an error message is worse than suggesting
/// nothing, and unlike a client-side word list the server can tell the difference.
#[must_use]
pub fn at(source: &str, offset: u32) -> Option<Cursor> {
    let result = ruby_prism::parse(source.as_bytes());
    if in_comment(&result, offset) {
        return None;
    }

    let mut finder = Finder {
        offset,
        in_literal: false,
        operator: None,
        arguments: None,
        local: None,
        locals: Vec::new(),
        source,
    };
    finder.visit(&result.node());
    if finder.in_literal {
        return None;
    }
    finder.type_the_local();

    // An operator wins over the argument list it is written inside: in `foo(bar.` the cursor is
    // in both, and what it is completing is `bar`'s methods.
    let context = match (finder.operator, finder.arguments) {
        (Some(context), _) => context,
        (None, Some(name)) => Context::Argument { name },
        (None, None) => Context::Expression,
    };

    let (start, end) = word_at(source, offset);
    Some(Cursor {
        context,
        start,
        end,
    })
}

/// The half-typed word the cursor is at the end of, as a span.
///
/// Ruby names are `[A-Za-z0-9_]` with three complications, and all three change what gets
/// replaced: a leading `@`, `@@` or `$` is part of the name, and a trailing `?` or `!` is part of
/// a method's name. Missing the last one turns accepting `empty?` into `empty?empty?`.
fn word_at(source: &str, offset: u32) -> (u32, u32) {
    let bytes = source.as_bytes();
    let mut start = (offset as usize).min(bytes.len());

    // `a ? b : c` also ends in `?`, so the mark only counts when a name precedes it.
    let mark = start > 0 && matches!(bytes[start - 1], b'?' | b'!');
    if mark {
        start -= 1;
    }
    let after_mark = start;
    while start > 0 && is_name_byte(bytes[start - 1]) {
        start -= 1;
    }
    if mark && start == after_mark {
        // Nothing but the mark: a ternary, not a predicate.
        return (offset, offset);
    }

    // Sigils only ever lead.
    if start > 0 && bytes[start - 1] == b'$' {
        start -= 1;
    } else {
        while start > 0 && bytes[start - 1] == b'@' {
            start -= 1;
        }
    }

    (start as u32, offset)
}

/// Ruby names are ASCII word characters plus anything non-ASCII: `имя` is a legal local, and a
/// run of continuation bytes can only ever be whole characters, so this stays on a boundary.
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

fn in_comment(result: &ParseResult<'_>, offset: u32) -> bool {
    result.comments().any(|comment| {
        let location = comment.location();
        // Inclusive of the end: a comment's span stops at the last character on the line, and a
        // cursor parked past it is still inside the comment.
        location.start_offset() as u32 <= offset && offset <= location.end_offset() as u32
    })
}

struct Finder<'s> {
    offset: u32,
    source: &'s str,
    /// Set when the cursor is inside a literal with no code in it.
    in_literal: bool,
    /// The innermost `::` or `.` the cursor is completing after.
    operator: Option<Context>,
    /// An offset inside the name of the innermost call whose argument list holds the cursor.
    arguments: Option<u32>,
    /// The span of the local variable the innermost operator's receiver is, when it is one.
    ///
    /// Held rather than resolved on the spot because the walk is pre-order: an assignment
    /// earlier in the file has not necessarily been visited by the time the call is.
    local: Option<(u32, u32)>,
    /// Every completed assignment to a local seen before the cursor, and what it assigned.
    locals: Vec<LocalWrite>,
}

/// One `x = <something>`, kept as a span rather than a name so matching costs no allocation.
struct LocalWrite {
    name: (u32, u32),
    /// The end of the assigned *value*, which is what "before the cursor" has to mean. Using
    /// the name would let `x = x.` type `x` by the half-written statement it is part of.
    at: u32,
    receiver: Receiver,
}

impl<'pr> Visit<'pr> for Finder<'_> {
    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        // A pre-order walk visits the cursor's ancestors outermost first, and only ancestors can
        // contain the cursor — so overwriting on every hit leaves the innermost one.
        if let Some(operator) = node.call_operator_loc()
            && let Some(message) = node.message_loc()
            // From just past the operator to the end of the message. The message is normally
            // empty and exactly at the cursor, but a trailing `.` on the line above an `end`
            // makes Prism read the `end` as the method name, and the cursor is then before it.
            && operator.end_offset() as u32 <= self.offset
            && self.offset <= message.end_offset() as u32
        {
            let receiver = node.receiver();
            self.operator = Some(Context::MethodCall {
                receiver: receiver_of(receiver.as_ref()),
            });
            self.local = receiver.as_ref().and_then(local_span);
        }

        if let Some(region) = self.argument_region(node)
            && region.0 <= self.offset
            && self.offset <= region.1
            && let Some(message) = node.message_loc()
        {
            self.arguments = Some(message.start_offset() as u32);
        }

        ruby_prism::visit_call_node(self, node);
    }

    fn visit_constant_path_node(&mut self, node: &ConstantPathNode<'pr>) {
        let delimiter = node.delimiter_loc();
        let name = node.name_loc();
        if delimiter.end_offset() as u32 <= self.offset && self.offset <= name.end_offset() as u32 {
            self.local = None;
            self.operator = Some(Context::NamespaceAccess {
                // A path with no parent is the leading-`::` form. It is the one place a missing
                // receiver means something specific rather than something unknown.
                receiver: match node.parent().as_ref() {
                    Some(parent) => receiver_of(Some(parent)),
                    None => Receiver::TopLevel,
                },
            });
        }
        ruby_prism::visit_constant_path_node(self, node);
    }

    fn visit_local_variable_write_node(&mut self, node: &LocalVariableWriteNode<'pr>) {
        let value = node.value();
        let at = value.location().end_offset() as u32;
        if at <= self.offset {
            let name = node.name_loc();
            self.locals.push(LocalWrite {
                name: (name.start_offset() as u32, name.end_offset() as u32),
                at,
                receiver: receiver_of(Some(&value)),
            });
        }
        ruby_prism::visit_local_variable_write_node(self, node);
    }

    fn visit_string_node(&mut self, node: &StringNode<'pr>) {
        self.note_literal(&node.content_loc());
    }

    fn visit_symbol_node(&mut self, node: &SymbolNode<'pr>) {
        if let Some(value) = node.value_loc() {
            self.note_literal(&value);
        }
    }

    fn visit_regular_expression_node(&mut self, node: &RegularExpressionNode<'pr>) {
        self.note_literal(&node.content_loc());
    }

    fn visit_x_string_node(&mut self, node: &XStringNode<'pr>) {
        self.note_literal(&node.content_loc());
    }

    fn visit_match_last_line_node(&mut self, node: &MatchLastLineNode<'pr>) {
        self.note_literal(&node.content_loc());
    }
}

impl Finder<'_> {
    /// The span between a call's parentheses, or the span of its bare argument list.
    ///
    /// Prism puts a synthetic zero-width closing paren at the last token it managed to read, so
    /// `foo(1, ` closes at the comma and the cursor sits past the end. Stepping over trailing
    /// separators recovers that without letting the region run past the end of the line.
    fn argument_region(&self, node: &CallNode<'_>) -> Option<(u32, u32)> {
        let (start, end) = match (node.opening_loc(), node.closing_loc()) {
            (Some(opening), Some(closing)) => {
                (opening.end_offset() as u32, closing.start_offset() as u32)
            }
            // A paren-less call: `link_to "x", foo`.
            _ => {
                let arguments = node.arguments()?.location();
                (
                    arguments.start_offset() as u32,
                    arguments.end_offset() as u32,
                )
            }
        };

        let bytes = self.source.as_bytes();
        let mut end = end as usize;
        while end < bytes.len() && matches!(bytes[end], b' ' | b'\t' | b',') {
            end += 1;
        }
        Some((start, end as u32))
    }

    /// Give the receiver a type when it is a local we watched being assigned one.
    ///
    /// The nearest preceding assignment wins, and that is the whole of the analysis. A variable
    /// reassigned inside a branch, or in a block that never runs, will be answered by whichever
    /// assignment is textually last — which is a *wrong* answer rather than an absent one, and
    /// the only place in this module where that is true. It stops at literals and `.new`: it
    /// never follows a method's return value, because that is `Chain#infer` and a different
    /// project. `depth` is ignored for the same reason — a block's `x` and the outer `x` are
    /// treated as one variable, which is what they usually are.
    fn type_the_local(&mut self) {
        let Some((start, end)) = self.local else {
            return;
        };
        let Some(Context::MethodCall {
            receiver: Receiver::Unknown,
        }) = self.operator
        else {
            return;
        };

        let name = &self.source[start as usize..end as usize];
        let typed = self
            .locals
            .iter()
            .filter(|write| {
                self.source[write.name.0 as usize..write.name.1 as usize] == *name
                    && !matches!(write.receiver, Receiver::Unknown)
            })
            .max_by_key(|write| write.at);

        if let Some(write) = typed {
            self.operator = Some(Context::MethodCall {
                receiver: write.receiver,
            });
        }
    }

    /// `content` is the literal's text, delimiters excluded.
    ///
    /// The delimiters are excluded deliberately and the bounds are inclusive: a cursor on the
    /// closing quote of `"foo"` is where the next `.` gets typed, while a cursor at the end of
    /// `:foo` — which has no closing delimiter at all — is still inside the symbol.
    fn note_literal(&mut self, content: &Location<'_>) {
        if content.start_offset() as u32 <= self.offset
            && self.offset <= content.end_offset() as u32
        {
            self.in_literal = true;
        }
    }
}

fn receiver_of(node: Option<&Node<'_>>) -> Receiver {
    let Some(node) = node else {
        return Receiver::Unknown;
    };
    // `(1..9).each` — parentheses are how a range or a ternary gets a receiver at all, so
    // seeing through a single-statement one is not an optimisation, it is the common spelling.
    if let Some(inner) = unparenthesised(node) {
        return receiver_of(Some(&inner));
    }
    if node.as_self_node().is_some() {
        return Receiver::SelfObject;
    }
    if is_constant(node) {
        // The end of the path, which is inside its last segment: `HR::Person` resolves as a
        // whole, and the reference the graph holds for it ends here too.
        return Receiver::Constant(node.location().end_offset() as u32);
    }
    if let Some(class) = literal_class(node) {
        return Receiver::Literal(class);
    }
    if let Some(offset) = instantiated(node) {
        return Receiver::Instance(offset);
    }
    Receiver::Unknown
}

/// The single expression inside `( ... )`, when that is what the node is.
fn unparenthesised<'pr>(node: &Node<'pr>) -> Option<Node<'pr>> {
    let body = node.as_parentheses_node()?.body()?;
    let mut statements = body.as_statements_node()?.body().iter();
    let only = statements.next()?;
    // `(a; b)` evaluates to `b`, but a receiver written that way is nobody's real code and
    // guessing at it is how a classification starts being wrong.
    statements.next().is_none().then_some(only)
}

fn is_constant(node: &Node<'_>) -> bool {
    node.as_constant_read_node().is_some() || node.as_constant_path_node().is_some()
}

/// The class Ruby gives a literal, or `None` when the node is not one.
///
/// Every entry here is a parser decision being read back, not a guess: there is no program in
/// which `[1, 2]` is anything but an `Array`. The interpolated forms are the same classes —
/// `"a#{b}"` is a `String` however the pieces were assembled.
fn literal_class(node: &Node<'_>) -> Option<&'static str> {
    let class = if node.as_string_node().is_some()
        || node.as_interpolated_string_node().is_some()
        // Backticks run a command and hand back its output.
        || node.as_x_string_node().is_some()
        || node.as_interpolated_x_string_node().is_some()
        || node.as_source_file_node().is_some()
    {
        "String"
    } else if node.as_symbol_node().is_some() || node.as_interpolated_symbol_node().is_some() {
        "Symbol"
    } else if node.as_array_node().is_some() {
        "Array"
    } else if node.as_hash_node().is_some() {
        "Hash"
    } else if node.as_integer_node().is_some() || node.as_source_line_node().is_some() {
        "Integer"
    } else if node.as_float_node().is_some() {
        "Float"
    } else if node.as_rational_node().is_some() {
        "Rational"
    } else if node.as_imaginary_node().is_some() {
        "Complex"
    } else if node.as_regular_expression_node().is_some()
        || node.as_interpolated_regular_expression_node().is_some()
    {
        "Regexp"
    } else if node.as_range_node().is_some() {
        "Range"
    } else if node.as_nil_node().is_some() {
        "NilClass"
    } else if node.as_true_node().is_some() {
        "TrueClass"
    } else if node.as_false_node().is_some() {
        "FalseClass"
    } else if node.as_lambda_node().is_some() {
        "Proc"
    } else if node.as_source_encoding_node().is_some() {
        "Encoding"
    } else {
        return None;
    };
    Some(class)
}

/// `Foo.new` and `Foo::Bar.new`, as an offset into the constant.
///
/// Only the literal message `new`. A class that overrides `new` to return something else is
/// rare enough, and a factory method called anything else would need the return type — which
/// RBS has and we deliberately do not read.
fn instantiated(node: &Node<'_>) -> Option<u32> {
    let call = node.as_call_node()?;
    if call.name().as_slice() != b"new" {
        return None;
    }
    let receiver = call.receiver()?;
    is_constant(&receiver).then(|| receiver.location().end_offset() as u32)
}

/// The span of a local variable read, which is the only receiver whose type can be recovered
/// from somewhere else in the file.
fn local_span(node: &Node<'_>) -> Option<(u32, u32)> {
    let read = node.as_local_variable_read_node()?;
    let location = read.location();
    Some((location.start_offset() as u32, location.end_offset() as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Classify the cursor written as `~` in the fixture, which is removed before parsing.
    fn at_marker(marked: &str) -> Option<(Cursor, String)> {
        let offset = marked.find('~').expect("a ~ marking the cursor") as u32;
        let source = marked.replace('~', "");
        at(&source, offset).map(|cursor| {
            let word = source[cursor.start as usize..cursor.end as usize].to_owned();
            (cursor, word)
        })
    }

    fn context(marked: &str) -> Context {
        at_marker(marked).expect("a cursor").0.context
    }

    fn word(marked: &str) -> String {
        at_marker(marked).expect("a cursor").1
    }

    /// The receiver of the `.` the cursor is completing after.
    fn receiver(marked: &str) -> Receiver {
        match context(marked) {
            Context::MethodCall { receiver } => receiver,
            other => panic!("expected a method call, got {other:?}"),
        }
    }

    #[test]
    fn a_literal_receiver_is_named_by_its_class() {
        for (source, class) in [
            (r#""hello".~"#, "String"),
            (r#""a#{b}c".~"#, "String"),
            ("`ls`.~", "String"),
            (":name.~", "Symbol"),
            ("[1, 2].~", "Array"),
            ("{ a: 1 }.~", "Hash"),
            ("42.~", "Integer"),
            ("4.2.~", "Float"),
            ("3r.~", "Rational"),
            ("3i.~", "Complex"),
            ("/re/.~", "Regexp"),
            ("(1..9).~", "Range"),
            ("nil.~", "NilClass"),
            ("true.~", "TrueClass"),
            ("false.~", "FalseClass"),
            ("-> { }.~", "Proc"),
            ("__FILE__.~", "String"),
            ("__LINE__.~", "Integer"),
        ] {
            assert_eq!(
                receiver(source),
                Receiver::Literal(class),
                "classifying {source:?}"
            );
        }
    }

    #[test]
    fn a_decimal_point_is_not_a_method_call() {
        // `4.2` is one literal, and the cursor after it completes on a Float — not on an
        // Integer `4` with a message. This is the case the backwards text scan gets wrong.
        assert_eq!(receiver("4.2.~"), Receiver::Literal("Float"));
    }

    #[test]
    fn new_gives_an_instance_rather_than_the_class() {
        // The offset points inside the constant, where the graph files the resolved reference —
        // the same convention `Receiver::Constant` uses.
        assert_eq!(receiver("Person.new.~"), Receiver::Instance(6));
        assert_eq!(receiver("HR::Person.new.~"), Receiver::Instance(10));
        assert_eq!(receiver("Person.new(1, 2).~"), Receiver::Instance(6));
        // And the class itself is still the class.
        assert_eq!(receiver("Person.~"), Receiver::Constant(6));
    }

    #[test]
    fn only_new_makes_an_instance() {
        // Any other factory would need the method's return type, which is exactly the line
        // this milestone does not cross.
        assert_eq!(receiver("Person.build.~"), Receiver::Unknown);
        assert_eq!(receiver("person.new.~"), Receiver::Unknown);
    }

    #[test]
    fn a_local_takes_the_type_of_what_was_assigned_to_it() {
        assert_eq!(
            receiver("name = \"ada\"\nname.~\n"),
            Receiver::Literal("String")
        );
        assert_eq!(
            receiver("person = Person.new\nperson.~\n"),
            Receiver::Instance(15)
        );
    }

    #[test]
    fn the_nearest_preceding_assignment_wins() {
        assert_eq!(
            receiver("x = \"a\"\nx = [1]\nx.~\n"),
            Receiver::Literal("Array")
        );
        // An assignment *after* the cursor is not in scope yet, whatever the parser saw.
        assert_eq!(
            receiver("x = \"a\"\nx.~\nx = [1]\n"),
            Receiver::Literal("String")
        );
    }

    #[test]
    fn a_local_assigned_something_untypable_stays_unknown() {
        assert_eq!(receiver("x = compute\nx.~\n"), Receiver::Unknown);
        assert_eq!(receiver("x.~\n"), Receiver::Unknown);
        // `x = x.` must not type `x` from the half-written statement it is part of.
        assert_eq!(receiver("x = x.~\n"), Receiver::Unknown);
    }

    #[test]
    fn an_instance_variable_is_still_unknown() {
        // Nothing here tracks where an ivar was written, and pretending otherwise would be a
        // wrong answer rather than a missing one.
        assert_eq!(receiver("@name = \"ada\"\n@name.~\n"), Receiver::Unknown);
    }

    #[test]
    fn a_literal_is_not_a_namespace() {
        assert_eq!(
            context(r#""a"::~"#),
            Context::NamespaceAccess {
                receiver: Receiver::Literal("String")
            }
        );
    }

    #[test]
    fn a_bare_word_is_an_expression() {
        assert_eq!(
            context("class Person\n  def shout\n    na~\n  end\nend\n"),
            Context::Expression
        );
        assert_eq!(
            word("class Person\n  def shout\n    na~\n  end\nend\n"),
            "na"
        );
    }

    #[test]
    fn an_empty_line_is_an_expression_with_nothing_typed() {
        let (cursor, word) = at_marker("class Person\n  def shout\n    ~\n  end\nend\n").unwrap();
        assert_eq!(cursor.context, Context::Expression);
        assert_eq!(word, "");
        assert_eq!(cursor.start, cursor.end);
    }

    #[test]
    fn the_scope_operator_is_a_namespace_access() {
        // The half-written form is the one that matters: `HR::` does not parse, and Prism's
        // recovery is what puts a zero-width name exactly at the cursor.
        assert_eq!(
            context("module HR\nend\nHR::~\n"),
            Context::NamespaceAccess {
                receiver: Receiver::Constant(16)
            }
        );
        assert_eq!(
            context("HR::Per~\n"),
            Context::NamespaceAccess {
                receiver: Receiver::Constant(2)
            }
        );
        assert_eq!(word("HR::Per~\n"), "Per");
    }

    #[test]
    fn a_dot_is_a_method_call_and_the_receiver_is_where_it_is_written() {
        assert_eq!(
            context("Person.~\n"),
            Context::MethodCall {
                receiver: Receiver::Constant(6)
            }
        );
        assert_eq!(
            context("HR::Person.bui~\n"),
            Context::MethodCall {
                receiver: Receiver::Constant(10)
            }
        );
        assert_eq!(word("HR::Person.bui~\n"), "bui");
    }

    #[test]
    fn a_receiver_with_no_type_says_so_rather_than_guessing() {
        // A method's return value: RBS has the type and reading it is `Chain#infer`.
        assert_eq!(
            context("p = build_person\np.~\n"),
            Context::MethodCall {
                receiver: Receiver::Unknown
            }
        );
        assert_eq!(
            context("@person.sh~\n"),
            Context::MethodCall {
                receiver: Receiver::Unknown
            }
        );
        assert_eq!(
            context("self.~\n"),
            Context::MethodCall {
                receiver: Receiver::SelfObject
            }
        );
    }

    #[test]
    fn safe_navigation_is_still_a_method_call() {
        assert_eq!(
            context("Person&.~\n"),
            Context::MethodCall {
                receiver: Receiver::Constant(6)
            }
        );
    }

    #[test]
    fn a_trailing_dot_above_an_end_is_still_a_method_call() {
        // Ruby continues an expression across a trailing `.`, so Prism reads the `end` below as
        // the method name and the cursor lands *before* the message rather than inside it.
        assert_eq!(
            context("items.each do |i|\n  i.~\nend\n"),
            Context::MethodCall {
                receiver: Receiver::Unknown
            }
        );
    }

    #[test]
    fn an_argument_list_is_its_own_context() {
        assert_eq!(context("build(~\n"), Context::Argument { name: 0 });
        assert_eq!(context("build(na~\n"), Context::Argument { name: 0 });
        assert_eq!(context("Person.build(~\n"), Context::Argument { name: 7 });
    }

    #[test]
    fn the_whitespace_after_a_comma_is_still_the_argument_list() {
        // Prism closes an unterminated call at the last token it read, which is the comma, so
        // the cursor is past the node unless the region steps over the separator.
        assert_eq!(context("build(1, ~\n"), Context::Argument { name: 0 });
        assert_eq!(
            context("def go\n  build(1, ~\nend\n"),
            Context::Argument { name: 9 }
        );
    }

    #[test]
    fn a_paren_less_call_still_offers_its_keywords() {
        assert_eq!(context("link_to \"x\", ~\n"), Context::Argument { name: 0 });
    }

    #[test]
    fn an_operator_beats_the_argument_list_it_is_written_in() {
        // Both contain the cursor; what is being completed is the receiver's methods.
        assert_eq!(
            context("build(Person.~\n"),
            Context::MethodCall {
                receiver: Receiver::Constant(12)
            }
        );
    }

    #[test]
    fn a_comment_completes_nothing() {
        assert!(at_marker("# na~\n").is_none());
        assert!(at_marker("x = 1 # na~\n").is_none());
        assert!(at_marker("=begin\nna~\n=end\n").is_none());
    }

    #[test]
    fn a_literal_completes_nothing() {
        assert!(at_marker("\"na~\"\n").is_none());
        assert!(at_marker(":na~\n").is_none());
        assert!(at_marker("/na~/\n").is_none());
        // But the code around it does: this is where the next `.` gets typed, and by then the
        // literal has a class.
        assert_eq!(
            context("\"text\".~\n"),
            Context::MethodCall {
                receiver: Receiver::Literal("String")
            }
        );
    }

    #[test]
    fn interpolation_is_code_even_though_the_string_is_not() {
        assert_eq!(
            context("\"hello #{Person.~}\"\n"),
            Context::MethodCall {
                receiver: Receiver::Constant(15)
            }
        );
        assert!(at_marker("\"hello ~#{x}\"\n").is_none());
    }

    #[test]
    fn a_predicate_replaces_its_question_mark_but_a_ternary_does_not() {
        assert_eq!(word("x.empty?~\n"), "empty?");
        assert_eq!(word("y = a ?~ b : c\n"), "");
    }

    #[test]
    fn a_sigil_is_part_of_the_word() {
        assert_eq!(word("@nam~\n"), "@nam");
        assert_eq!(word("@@co~\n"), "@@co");
        assert_eq!(word("$gl~\n"), "$gl");
        assert_eq!(word("@~\n"), "@");
    }

    #[test]
    fn a_leading_scope_operator_means_the_top_level() {
        // `::Foo` is how a Rails codebase says "the outer one", and it is the only receiver
        // that is absent on purpose rather than unknown.
        assert_eq!(
            context("::~\n"),
            Context::NamespaceAccess {
                receiver: Receiver::TopLevel
            }
        );
        assert_eq!(word("::Us~\n"), "Us");
        // A parent that happens to be top-level is still an ordinary path.
        assert_eq!(
            context("::Foo::Ba~\n"),
            Context::NamespaceAccess {
                receiver: Receiver::Constant(5)
            }
        );
    }

    #[test]
    fn an_unparseable_file_does_not_panic() {
        assert!(at("class Broken\n  def foo\n", 5).is_some());
        assert!(at("", 0).is_some());
    }
}
