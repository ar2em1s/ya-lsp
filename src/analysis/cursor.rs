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
//! cursor is somewhere Ruby code can be written at all.
//!
//! # Why this module has no graph
//!
//! What the receiver *is* takes a graph; where the receiver is *written* does not. Keeping the
//! split here means the classification can be tested against nothing but a string, which is the
//! only way the awkward cases (a trailing `.` on the line above an `end`, a cursor in the
//! whitespace after a comma) are cheap enough to enumerate.

use std::borrow::Cow;

use ruby_prism::{
    CallNode, ConstantPathNode, InstanceVariableAndWriteNode, InstanceVariableOrWriteNode,
    InstanceVariableWriteNode, LocalVariableWriteNode, Location, MatchLastLineNode, Node,
    ParseResult, RegularExpressionNode, StringNode, SymbolNode, Visit, XStringNode,
};

use super::position::Rebase;
use super::scopes;

/// What the cursor is positioned to complete.
///
/// Not `Copy`: a receiver can be a chain, which owns the method names in it.
#[derive(Debug, Clone, PartialEq, Eq)]
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

impl Context {
    /// Whether Ruby would let a *private* method be written where the cursor is.
    ///
    /// It permits one with an implicit receiver, and since 2.7 with a receiver spelled `self` —
    /// through `.` and through `::` alike; both were checked against a real interpreter, as was
    /// the fact that `other.secret` still raises from inside the class that declares `secret`.
    ///
    /// Which of those the cursor sits in is a question about the syntax and nothing else, so it
    /// is answered here rather than where the graph is. It is deliberately stricter than
    /// rubydex, whose own check passes a private method whenever the caller's `self` is the same
    /// class as the receiver — Ruby's exemption is for the receiver being *written* `self`, not
    /// for it happening to be the same class.
    #[must_use]
    pub fn allows_private(&self) -> bool {
        match self {
            // No receiver written at all, so the call has one implicitly.
            Context::Expression | Context::Argument { .. } => true,
            Context::MethodCall { receiver } | Context::NamespaceAccess { receiver } => {
                matches!(*receiver, Receiver::SelfObject)
            }
        }
    }
}

impl Context {
    /// Move this context's offsets out of the buffer's coordinates and into the graph's.
    ///
    /// **The boundary a deferred index makes necessary.** Everything in this module reads the buffer, and
    /// everything that consumes a `Receiver` — `constant_at`, `precise_call` — keys the *graph*
    /// with what it finds. The two coordinate systems are one string wherever the graph holds
    /// what the buffer holds, and this is then the identity; between a keystroke and the settle
    /// that indexes it they are not.
    ///
    /// `None` means the cursor's receiver is written in text the graph has never been given, so
    /// no lookup on it can be trusted and the caller must index before answering.
    #[must_use]
    pub fn rebased(&self, rebase: &Rebase) -> Option<Self> {
        Some(match self {
            Context::Expression => Context::Expression,
            Context::Argument { name } => Context::Argument {
                name: rebase.to_graph(*name)?,
            },
            Context::NamespaceAccess { receiver } => Context::NamespaceAccess {
                receiver: receiver.rebased(rebase)?,
            },
            Context::MethodCall { receiver } => Context::MethodCall {
                receiver: receiver.rebased(rebase)?,
            },
        })
    }
}

impl Receiver {
    /// The same translation, down the receiver tree.
    ///
    /// **`Assigned { at }` is deliberately left alone and it is the trap in this enum.** Every
    /// other offset here is a graph key; that one is provenance — `hover` renders it with
    /// `text.position_at(at)` against the *buffer* to name a line — so translating it would move
    /// the line every card prints. A blanket "rebase every `u32` in `Receiver`" is wrong.
    #[must_use]
    pub fn rebased(&self, rebase: &Rebase) -> Option<Self> {
        Some(match self {
            Receiver::Constant(offset) => Receiver::Constant(rebase.to_graph(*offset)?),
            Receiver::Instance(offset) => Receiver::Instance(rebase.to_graph(*offset)?),
            Receiver::Assigned { at, was } => Receiver::Assigned {
                at: *at,
                was: Box::new(was.rebased(rebase)?),
            },
            Receiver::Returned {
                on,
                method,
                block,
                arity,
            } => Receiver::Returned {
                on: Box::new(on.rebased(rebase)?),
                method: method.clone(),
                block: *block,
                arity: *arity,
            },
            Receiver::Yielded { on, method, index } => Receiver::Yielded {
                on: Box::new(on.rebased(rebase)?),
                method: method.clone(),
                index: *index,
            },
            Receiver::Spelled { was, name } => Receiver::Spelled {
                was: Box::new(was.rebased(rebase)?),
                name: name.clone(),
            },
            // Nothing to move: a name, a literal, `self`, `::` and the two dead ends.
            Receiver::Literal(_)
            | Receiver::SelfObject
            | Receiver::TopLevel
            | Receiver::Named(_)
            | Receiver::Unknown => self.clone(),
        })
    }
}

/// The thing to the left of the `.` or the `::`.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// The value a call hands back: what the call was written on, and the name it called.
    ///
    /// This is a *shape*, not a type. Turning it into one is a lookup in
    /// [`types`](super::types), which is the only thing that knows `String#upcase` returns a
    /// `String` — and which needs a graph, so it happens on the other side of this module's
    /// line. Nothing here has followed anything: `"x".upcase` is recorded as "the result of
    /// calling `upcase` on a `String` literal" and no more.
    ///
    /// One `Unknown` anywhere ends the chain rather than being carried, because a step that
    /// cannot be looked up makes every step above it unanswerable too.
    Returned {
        on: Box<Receiver>,
        /// The method's name, as the source spells it. Carried rather than an offset because
        /// resolving it needs no reference the graph recorded — a half-written chain has
        /// none — and the name is the only part of it this module can read.
        method: String,
        /// Whether the call was written with a block.
        ///
        /// Syntax, and the reason `"x".bytes.` can be answered at all: RBS declares `bytes` one
        /// way with a block and another without, and which arm applies is decided here rather
        /// than guessed at. `&:upcase` and a forwarded `&blk` count — Ruby passes a block either
        /// way, so the signature's block arm is the one that applies.
        block: bool,
        /// How many positional arguments the call wrote.
        ///
        /// The other half of the same fact, and read for the same reason: RBS declares
        /// `Float#round` one way with a digit count and another without, and which arm applies
        /// is syntax rather than inference. Counted here because counting it is a question
        /// about the text; what the count *means* is [`types`](super::types)'.
        arity: Arity,
    },
    /// A block parameter, typed by what the method the block was passed to says it yields.
    ///
    /// The other half of `Returned`, and it reads the same declaration from the other end: RBS
    /// writes `def each: () { (Story) -> void } -> Story::Relation`, and reading only the
    /// return type leaves `Story.where(...).each do |instance|` typing `instance` as nothing at
    /// all, while the same block written `do |story|` types it by *guessing from the word* —
    /// which looks like it works and stops working the moment the variable is renamed.
    ///
    /// A **shape** like `Returned`: nothing here has resolved anything. `on` and `method` are
    /// the call the block was written on, and `index` is which of the block's parameters this
    /// is; what the signature says they are is [`types`](super::types)'.
    ///
    /// There is no `block` field and no `arity`, and that is not an omission. A call that
    /// reaches this **wrote** a block, so the block arm is the one that applies; and what a
    /// block is handed is a property of the signature rather than of how many arguments the
    /// call wrote, so the arity partition has nothing to say about it.
    Yielded {
        on: Box<Receiver>,
        method: String,
        index: usize,
    },
    /// An instance variable, typed by an assignment somewhere else in its class.
    ///
    /// Wrapped rather than replaced, because *where the type came from* is half the answer: the
    /// assignment can be in another method, twenty lines away, in a branch that never runs.
    /// A local is not wrapped — `person = Person.new` is a line the reader can see from where
    /// they are standing, and it is answered exactly.
    Assigned {
        /// The offset of the `@name` being assigned, for a card to name the line.
        at: u32,
        was: Box<Receiver>,
    },
    /// A variable whose assignment produced a shape, carrying the name it is written as.
    ///
    /// The fall-through, and it is a *step* rather than a sixth rung. `story =
    /// Story.where(...).first` produces a chain nothing declares a return type for, and without
    /// this the answer is nothing — while a `story` with no assignment at all reaches the name
    /// rung and answers `Story`. Writing the assignment made the answer *worse*, which is the
    /// wrong shape for a system whose whole argument is that its rungs are ordered.
    ///
    /// The order itself does not change. [`types`](super::types) asks `was` first and reaches
    /// `name` only when that came back empty, so a chain that resolves can never be displaced
    /// by a guess — and what `name` reaches is the same last rung a bare name reaches, wearing
    /// the same label and turned off by the same setting.
    Spelled {
        was: Box<Receiver>,
        /// What the source spells the variable, sigils and all: exactly what
        /// [`Receiver::Named`] would have carried had there been no assignment to follow.
        name: String,
    },
    /// A bare name nothing in this file could type: an instance variable with no assignment
    /// worth following, a local likewise, a receiverless call. What the source spells it,
    /// sigils and all.
    ///
    /// Not a type and not a shape — a *name*, and it is here because two rungs below the graph
    /// can still do something with one. A template's instance variables are assigned by the
    /// controller its path names, which is a question for [`types`](super::types) because it
    /// needs a graph and another file. And a name that looks like a class is a guess worth
    /// making once it is labelled as one. Both live on the other side of this module's line;
    /// what belongs here is only that the name survived.
    ///
    /// It is deliberately *not* a fifth thing a caller has to handle: everything that treated
    /// [`Receiver::Unknown`] as "nothing exact can be said" still answers the same way when the
    /// rungs below come back empty.
    Named(String),
    /// A method's return value that nothing declares, an expression whose shape says nothing:
    /// a type it would take real inference to know, with not even a name left to try.
    Unknown,
}

/// How many positional arguments a call wrote.
///
/// Positional ones only: a keyword hash is not one of them, and RBS counts the two apart too.
/// An arity that cannot be counted is a value rather than an absence, because the two answer
/// differently — a call with no arguments reaches only the arms that take none, and one whose
/// arguments cannot be counted reaches whatever every arm agrees on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    /// Exactly this many, every one of them written out.
    Exactly(u32),
    /// A splat or an argument forwarding: `foo(*args)` writes some number of arguments this
    /// side cannot know, and neither can any amount of inference below it.
    Unknown,
}

/// How many links of a chain are followed before the answer becomes `Unknown`.
///
/// A chain is not a fixpoint and must not become one: this runs on the analysis thread, on a
/// keystroke, and a pathological expression — a deep chain, or `x = x.foo` — must not be able
/// to make it think. Eight is past anything anybody writes; the limit is a bound and not a
/// budget.
const MAX_CHAIN: usize = 8;

/// The call whose argument list the cursor is inside, and which argument that is.
///
/// This is what `textDocument/signatureHelp` asks about, and it is deliberately *not* the same
/// question `Context::Argument` answers. Two differences, each of them a case where the popup
/// has to stay up while completion has nothing to say: a `.` written inside the parentheses
/// (`puts(person.`) is still a call the user is passing arguments to, and so is a string
/// argument being typed (`puts("hel`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// An offset inside the called method's name, for the caller to resolve. The same
    /// convention `Context::Argument` uses.
    pub name: u32,
    pub active: Active,
}

/// Which of a method's parameters the cursor is writing an argument for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Active {
    /// The nth argument, counting from zero — the number of arguments that end before the
    /// cursor. A keyword hash counts as its own elements rather than as one argument, so
    /// `f(1, a: 2, ` is the third parameter and not the second.
    Nth(u32),
    /// A keyword argument, named. Keywords may be written in any order, so where one sits in
    /// the call says nothing about which parameter it is: `f(b: 1, a: ` is `a`, not the second.
    Keyword(String),
    /// A keyword argument that has not been named yet — the cursor is past one keyword and has
    /// not begun the next. Which one it will be is unknowable; *that* it is a keyword is not,
    /// because Ruby forbids a positional argument after one. Counting instead would answer with
    /// a parameter this call can no longer reach.
    AnyKeyword,
}

/// A classified cursor, and the half-typed word it sits at the end of.
#[derive(Debug, Clone, PartialEq, Eq)]
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

    let mut finder = Finder::new(source, offset);
    finder.visit(&result.node());
    if finder.in_literal {
        return None;
    }

    // An operator wins over the argument list it is written inside: in `foo(bar.` the cursor is
    // in both, and what it is completing is `bar`'s methods.
    let context = match (&finder.operator, &finder.arguments) {
        (Some(pending), _) => finder.classify(pending),
        (None, Some(call)) => Context::Argument { name: call.name },
        (None, None) => Context::Expression,
    };

    let (start, end) = word_at(source, offset);
    Some(Cursor {
        context,
        start,
        end,
    })
}

/// The innermost call whose argument list the cursor sits in, and which argument that is.
///
/// Unlike [`at`], neither a comment nor a literal ends the answer, and an operator written
/// inside the parentheses does not take it over. All three are places where there is nothing to
/// complete and still a call being written: an editor keeps the signature on screen through
/// `puts("hel`, through `puts(person.` and through a comment between two arguments, and a
/// server that answers `null` for those makes it flicker on every keystroke.
///
/// `None` when the cursor is not inside an argument list at all, or when the call has no name
/// to resolve — `foo.()` is `foo.call()` written with none.
#[must_use]
pub fn call_at(source: &str, offset: u32) -> Option<Call> {
    let result = ruby_prism::parse(source.as_bytes());
    let mut finder = Finder::new(source, offset);
    finder.visit(&result.node());
    finder.arguments
}

/// Every assignment to `@name` on an instance of the class written as `path`, in file order.
///
/// The syntax half of the view↔controller convention, and the one entry point that reads a
/// file the cursor is not in. A template has no enclosing class, so
/// [`Finder::type_the_instance_variable`] has nothing to walk; the assignments that type
/// `@story` are in `StoriesController`, and [`types`](super::types) is what knows which file
/// that is. This is the same walk, entered by name.
///
/// Which `@story` counts stays [`scopes`]'s question, asked through [`scopes::writes_to`] —
/// a `def self.` and a `class << self` hold a different variable of the same name, exactly as
/// they do for a cursor.
///
/// **All of them, in order, rather than the last one that produced a shape.** The caller has a
/// graph and this does not, and "produced a type" is a question only the graph can answer: an
/// assignment whose class nothing declares has to fall through to the one written above it,
/// which cannot be decided here. Two parses, like the cursor path, and for the same reason.
#[must_use]
pub fn assignments_in(source: &str, path: &str, name: &str) -> Vec<(u32, Receiver)> {
    let writes = scopes::writes_to(source, path, name);
    if writes.is_empty() {
        return Vec::new();
    }
    let result = ruby_prism::parse(source.as_bytes());
    // No cursor in this file: `u32::MAX` is past every offset in it, so nothing is "being
    // typed" and the walk collects assignments and nothing else.
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    writes
        .iter()
        .filter_map(|occurrence| {
            let write = finder
                .instance_writes
                .iter()
                .find(|write| write.name == (occurrence.start, occurrence.end))?;
            let receiver = finder.receiver_of(Some(&write.value), 0);
            (!matches!(receiver, Receiver::Unknown)).then_some((occurrence.start, receiver))
        })
        .collect()
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

struct Finder<'s, 'pr> {
    offset: u32,
    source: &'s str,
    /// Set when the cursor is inside a literal with no code in it.
    in_literal: bool,
    /// The innermost `::` or `.` the cursor is completing after, still unclassified.
    ///
    /// The *node* rather than the answer, and it has to be. Classifying
    /// a receiver can need an assignment that the walk has not reached yet — `x` typed from
    /// `x = Foo.new` is the old case, and a chain through a local is the new one — so the whole
    /// classification waits until the walk is over and every assignment is in. Holding one node
    /// is also how the rule "one `Unknown` ends the chain" stays in a single place instead of
    /// being re-established per link.
    operator: Option<Pending<'pr>>,
    /// The innermost call whose argument list holds the cursor.
    arguments: Option<Call>,
    /// Every completed assignment to a local that ends before the cursor.
    locals: Vec<LocalWrite<'pr>>,
    /// Every block parameter in the file, with the call its block was written on.
    ///
    /// Collected on the same walk as the writes and for the same reason: the question is asked
    /// about a span, and answering it needs the whole file rather than the node under the
    /// cursor.
    yielded: Vec<BlockParameter<'pr>>,
    /// Every assignment to an instance variable anywhere in the file, keyed by the span of the
    /// `@name` it writes — which is the span [`scopes`] reports occurrences under, and the only
    /// thing the two walks have to agree about.
    instance_writes: Vec<InstanceWrite<'pr>>,
    /// The half-typed operator the cursor is completing after, as a span to blank out.
    ///
    /// See [`Finder::without_the_half_typed_call`]. Recorded here because the call node it
    /// comes from is gone by the time the question is asked.
    repair: Option<(u32, u32)>,
}

/// One `@x = <something>`.
///
/// Unlike a local's, these are collected from the whole file rather than from before the cursor.
/// An instance variable is assigned in `initialize` and read in every other method, and half of
/// those methods are written above it.
struct InstanceWrite<'pr> {
    name: (u32, u32),
    /// The span of the assigned value, so that a write cannot answer for the read inside it.
    value_span: (u32, u32),
    value: Node<'pr>,
}

/// An operator the cursor is completing after, before its receiver has been looked at.
enum Pending<'pr> {
    /// After a `.` or `&.`. `None` where no receiver was written, which is a call on an
    /// implicit `self` and not something this module can name.
    MethodCall(Option<Node<'pr>>),
    /// After a `::`. `None` is the leading-`::` form, the one place a missing receiver means
    /// something specific rather than something unknown.
    NamespaceAccess(Option<Node<'pr>>),
}

/// One `x = <something>`, kept as a span rather than a name so matching costs no allocation.
/// Whether this shape is a block parameter, reached directly or through an assignment.
///
/// The first of the two precedence questions the assignment loops ask, and it is about
/// *precedence* rather than about the shape being wrong. `Receiver::Yielded` answers only where
/// the callee's signature says what its block receives; where it does not — which is every call
/// on a receiver this pass cannot type — it falls through `Receiver::Spelled` to the name,
/// exactly as a bare local read does. So a write that produced one must not
/// displace a write that produced a real type, and `Spelled` is unwrapped because a local read
/// is always wrapped in one.
fn relays_a_block_parameter(receiver: &Receiver) -> bool {
    match receiver {
        Receiver::Yielded { .. } => true,
        Receiver::Spelled { was, .. } => relays_a_block_parameter(was),
        _ => false,
    }
}

/// Whether this chain is rooted in `self` — the third assignment slot.
///
/// The same argument one step weaker. `Foo.bar` names a class the file names and a receiver
/// every reader can check; `self` in an RSpec block, a rake task or a top-level script is
/// `Object`, and `create(:story, title: "…")` resolves against it to nothing. Read as solid it
/// displaced `s = Story.find(s.id)` written ten lines above and took **62 lobsters positions**
/// down with it, every one in a spec.
///
/// **Through a call and never through a variable**, and both halves of that were measured rather
/// than reasoned. chatwoot writes `tokens = user_tokens(account, …) + contact_tokens(…)`, a call
/// on a call on `self`, which asked only about its last link looks as solid as `Foo.bar.baz` —
/// so the walk goes down the `Returned`s. It stops at a `Spelled`, because lobsters writes
/// `link = c.links.last` where `c` is itself a call on `self`: that chain is **`c`'s** problem,
/// `c` already carries its own name rung, and treating `link` as weak let a String literal in
/// another `it` block take it — two positions, in the other direction.
///
/// A **written** `self.foo` is treated the same as an implicit one, because it is the same call.
fn rooted_in_self(receiver: &Receiver) -> bool {
    // A **bare** receiverless call carries its own name rung in a `Spelled` — the wrapper item
    // 41 adds around the lookup — and that wrapper is this same call rather than a variable in
    // the middle of a chain, so it is unwrapped once here and never followed again below.
    let receiver = match receiver {
        Receiver::Spelled { was, .. } => was.as_ref(),
        other => other,
    };
    rooted_in_a_call(receiver)
}

fn rooted_in_a_call(receiver: &Receiver) -> bool {
    match receiver {
        Receiver::SelfObject => true,
        Receiver::Returned { on, .. } => rooted_in_a_call(on),
        _ => false,
    }
}

/// One parameter of one block, and the call the block was written on.
struct BlockParameter<'pr> {
    name: (u32, u32),
    /// Which positional parameter it is, which is the index RBS lists the block's own by.
    index: usize,
    /// The block's whole span, so a read of that name outside it is not this parameter.
    body: (u32, u32),
    /// The call, unclassified — held for [`LocalWrite::value`]'s reason: what it is is a
    /// question for `receiver_of`, which cannot run during the walk that collects it.
    call: Node<'pr>,
}

struct LocalWrite<'pr> {
    name: (u32, u32),
    /// The end of the assigned *value*, which is what "before the cursor" has to mean. Using
    /// the name would let `x = x.` type `x` by the half-written statement it is part of.
    at: u32,
    /// The value, unclassified. Held rather than typed at this point so that an assignment
    /// whose own right-hand side is another local (`b = a.foo`) can be answered whichever
    /// order the walk reached the two in.
    value: Node<'pr>,
}

impl<'pr> Visit<'pr> for Finder<'_, 'pr> {
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
            self.repair = Some((
                operator.start_offset() as u32,
                // The message is normally empty and exactly at the cursor. Where it is not, it
                // is either the half-typed word — which blanks with the operator — or, for a
                // dangling `.` on the line above an `end`, the `end` keyword itself, which must
                // survive: blanking that is what would break the structure this is repairing.
                if message.end_offset() as u32 <= self.offset {
                    message.end_offset() as u32
                } else {
                    operator.end_offset() as u32
                },
            ));
            self.operator = Some(Pending::MethodCall(node.receiver()));
        }

        if let Some(region) = self.argument_region(node)
            && region.0 <= self.offset
            && self.offset <= region.1
            && let Some(message) = node.message_loc()
        {
            self.arguments = Some(Call {
                name: message.start_offset() as u32,
                active: self.active_argument(node),
            });
        }

        // A block's parameters, and the call they are handed by. Read here rather
        // than in `visit_block_node` because the two are needed together — the parameter's name
        // and the call whose signature says what it receives — and only the call node holds
        // both.
        if let Some(block) = node.block().and_then(|block| block.as_block_node())
            && let Some(parameters) = block
                .parameters()
                .and_then(|it| it.as_block_parameters_node())
                .and_then(|it| it.parameters())
        {
            let span = block.location();
            for (index, parameter) in parameters.requireds().iter().enumerate() {
                let Some(required) = parameter.as_required_parameter_node() else {
                    continue;
                };
                let name = required.location();
                self.yielded.push(BlockParameter {
                    name: (name.start_offset() as u32, name.end_offset() as u32),
                    index,
                    body: (span.start_offset() as u32, span.end_offset() as u32),
                    call: node.as_node(),
                });
            }
        }

        ruby_prism::visit_call_node(self, node);
    }

    fn visit_constant_path_node(&mut self, node: &ConstantPathNode<'pr>) {
        let delimiter = node.delimiter_loc();
        let name = node.name_loc();
        if delimiter.end_offset() as u32 <= self.offset && self.offset <= name.end_offset() as u32 {
            self.operator = Some(Pending::NamespaceAccess(node.parent()));
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
                value,
            });
        }
        ruby_prism::visit_local_variable_write_node(self, node);
    }

    fn visit_instance_variable_write_node(&mut self, node: &InstanceVariableWriteNode<'pr>) {
        self.note_instance_write(&node.name_loc(), node.value());
        ruby_prism::visit_instance_variable_write_node(self, node);
    }

    // `@cache ||= build` is how Ruby spells memoisation and is as much an assignment as `=`.
    // `@n += 1` is not: an operator write says what happens to a value, not what it is.
    fn visit_instance_variable_or_write_node(&mut self, node: &InstanceVariableOrWriteNode<'pr>) {
        self.note_instance_write(&node.name_loc(), node.value());
        ruby_prism::visit_instance_variable_or_write_node(self, node);
    }

    fn visit_instance_variable_and_write_node(&mut self, node: &InstanceVariableAndWriteNode<'pr>) {
        self.note_instance_write(&node.name_loc(), node.value());
        ruby_prism::visit_instance_variable_and_write_node(self, node);
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

impl<'s, 'pr> Finder<'s, 'pr> {
    fn new(source: &'s str, offset: u32) -> Self {
        Self {
            offset,
            source,
            in_literal: false,
            operator: None,
            arguments: None,
            locals: Vec::new(),
            yielded: Vec::new(),
            instance_writes: Vec::new(),
            repair: None,
        }
    }

    /// The file with the half-typed call blanked out, byte for byte.
    ///
    /// Completion fires on text that is, by definition, not valid Ruby, and Prism's recovery
    /// for a dangling `.` is to read whatever follows as the method name — which for
    /// `@name.` on the line above an `end` means the `end` is consumed and **everything below
    /// it is reparented**. Measured: the `@name = "ada"` in an `initialize` written under the
    /// method being typed in lands in a `def` nested inside it, one singleton step away, and is
    /// correctly reported as a different variable.
    ///
    /// So the scope question is asked about the file as it would be without the operator the
    /// user is in the middle of typing. Every byte but the newlines becomes a space, exactly as
    /// [`signatures`](super::signatures) does it, so every offset and every line in the answer
    /// is the one the caller asked about.
    fn without_the_half_typed_call(&self) -> Cow<'s, str> {
        let Some((start, end)) = self.repair else {
            return Cow::Borrowed(self.source);
        };
        let mut bytes = self.source.as_bytes().to_vec();
        let Some(slice) = bytes.get_mut(start as usize..end as usize) else {
            return Cow::Borrowed(self.source);
        };
        for byte in slice {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
        // Whole characters were replaced by ASCII, so this holds; a wrong answer here would be
        // a scope question asked about mangled text rather than a panic.
        String::from_utf8(bytes).map_or(Cow::Borrowed(self.source), Cow::Owned)
    }

    fn note_instance_write(&mut self, name: &Location<'_>, value: Node<'pr>) {
        let span = value.location();
        self.instance_writes.push(InstanceWrite {
            name: (name.start_offset() as u32, name.end_offset() as u32),
            value_span: (span.start_offset() as u32, span.end_offset() as u32),
            value,
        });
    }

    /// What the held operator is completing after, now that the whole file has been walked.
    fn classify(&self, pending: &Pending<'_>) -> Context {
        match pending {
            Pending::MethodCall(receiver) => Context::MethodCall {
                receiver: self.receiver_of(receiver.as_ref(), 0),
            },
            Pending::NamespaceAccess(parent) => Context::NamespaceAccess {
                receiver: match parent {
                    Some(parent) => self.receiver_of(Some(parent), 0),
                    None => Receiver::TopLevel,
                },
            },
        }
    }

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

    /// Which of the callee's parameters the cursor is writing an argument for.
    ///
    /// The count of arguments that *end* before the cursor is the whole rule for positional
    /// ones — `f(1, ` has finished one, `f(1` has finished none, `f(` none either — with two
    /// things folded into "an argument". A keyword hash is spread into its elements, because
    /// `f(a: 1, b: 2` is one Prism node and two arguments written; and a keyword the cursor is
    /// *inside* beats the count outright, since keywords may be written in any order and the
    /// position of one then says nothing about which parameter it is.
    fn active_argument(&self, node: &CallNode<'_>) -> Active {
        let Some(arguments) = node.arguments() else {
            return Active::Nth(0);
        };

        let mut elements: Vec<Node<'_>> = Vec::new();
        for argument in arguments.arguments().iter() {
            // Only a hash Prism itself says is keywords. `f("a" => 1, "b" => 2)` is one
            // argument however many pairs are in it, and spreading it would count two.
            match argument
                .as_keyword_hash_node()
                .filter(ruby_prism::KeywordHashNode::is_symbol_keys)
            {
                Some(hash) => elements.extend(hash.elements().iter()),
                None => elements.push(argument),
            }
        }

        let mut written = 0_u32;
        let mut keywords_began = false;
        for element in &elements {
            let location = element.location();
            let (start, end) = (location.start_offset() as u32, location.end_offset() as u32);
            if self.holds_cursor(start, end) {
                return keyword_name(self.source, element)
                    .map_or(Active::Nth(written), Active::Keyword);
            }
            if end < self.offset {
                written += 1;
                keywords_began |= element.as_assoc_node().is_some();
            }
        }
        if keywords_began {
            return Active::AnyKeyword;
        }
        Active::Nth(written)
    }

    /// Whether the cursor belongs to this argument rather than to the next one.
    ///
    /// Inside its span, plainly — and also *past* it, up to the comma that ends it, because
    /// that gap is where the cursor spends most of its time: `create(name: ` has written the
    /// keyword and not yet its value, and Prism recovers the pair as ending at the colon. The
    /// comma is what says the user has moved on, so `create(name: "ada", ` belongs to the
    /// argument after `name` rather than to `name`.
    ///
    /// There is no upper bound to check. The caller has already established that the cursor is
    /// inside the argument list, and no argument's span reaches past it — a heredoc looks as
    /// though it should and does not: Prism scopes the node to the `<<~SQL` marker and holds
    /// the body separately, so `execute(<<~SQL, id)` needs no special case.
    fn holds_cursor(&self, start: u32, end: u32) -> bool {
        if self.offset < start {
            return false;
        }
        self.offset <= end || !self.source[end as usize..self.offset as usize].contains(',')
    }

    /// What a receiver node is, as far as syntax can say.
    ///
    /// `depth` bounds the recursion at [`MAX_CHAIN`]: the three rules below all recurse, and two
    /// of them can be made to recurse for ever by legal Ruby (`(((x)))`, `x = x.foo`).
    fn receiver_of(&self, node: Option<&Node<'_>>, depth: usize) -> Receiver {
        let Some(node) = node else {
            return Receiver::Unknown;
        };
        if depth >= MAX_CHAIN {
            return Receiver::Unknown;
        }
        // `(1..9).each` — parentheses are how a range or a ternary gets a receiver at all, so
        // seeing through a single-statement one is not an optimisation, it is the common
        // spelling.
        if let Some(inner) = unparenthesised(node) {
            return self.receiver_of(Some(&inner), depth + 1);
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
        if let Some(span) = local_span(node) {
            return self.type_the_local(span, depth);
        }
        if let Some(span) = instance_span(node) {
            return self.type_the_instance_variable(span, depth);
        }
        self.returned_by(node, depth)
    }

    /// Give a local the type of the assignment it came from.
    ///
    /// The nearest preceding assignment *that produced a type* wins, and that is the whole of
    /// the analysis. A variable reassigned inside a branch, or in a block that never runs, will
    /// be answered by whichever assignment is textually last — which is a *wrong* answer rather
    /// than an absent one, and the only place in this module where that is true. The
    /// provenance is what makes it possible to see.
    ///
    /// `depth` is Prism's and is deliberately ignored: a block's `x` and the outer `x` are
    /// treated as one variable, which is what they usually are.
    ///
    /// Only assignments whose value ends before the *read* are candidates, which is stricter
    /// than "before the cursor" and is what makes `x = x.foo` terminate on its own rather than
    /// on [`MAX_CHAIN`]: the read inside the value cannot be answered by the write it is part
    /// of.
    ///
    /// The answer carries the variable's own spelling alongside the assignment's shape. An
    /// assignment that produces a shape nothing can type must not leave the variable worse off
    /// than one with no assignment at all — see [`Receiver::Spelled`].
    fn type_the_local(&self, span: (u32, u32), depth: usize) -> Receiver {
        let name = &self.source[span.0 as usize..span.1 as usize];
        // The latest write that produced a shape, and the latest that produced one only by
        // *relaying a block parameter* — `max_distance_color = color` inside
        // `palette.each do |color|`. They are two slots rather than one `max_by_key` because a
        // relayed shape is a fallback wearing a shape's clothes: `Receiver::Yielded` ends at
        // the name when the callee's signature says nothing about its block, which is the
        // ordinary case for a receiver this pass cannot type. Letting it win on position alone
        // is how `max_distance_color = nil` above it stopped answering `NilClass` — two
        // mastodon positions, and the only two the block half made worse anywhere.
        let mut solid: Option<(u32, Receiver)> = None;
        let mut relayed: Option<(u32, Receiver)> = None;
        // The third slot, and it is **below** the block parameter below: a write in another
        // method whose value is a call on `self` must not take `uploader` away from the
        // `SubforemImageUploader.new.tap do |uploader|` the cursor is standing inside. One
        // forem position, and the only one this rung makes worse anywhere.
        let mut rooted: Option<(u32, Receiver)> = None;
        for write in self.locals.iter().filter(|write| {
            write.at <= span.0 && self.source[write.name.0 as usize..write.name.1 as usize] == *name
        }) {
            let receiver = self.receiver_of(Some(&write.value), depth + 1);
            // A `Receiver::Named` counts as nothing here, and that is the whole of how the
            // last rung stays last: `x = Person.new` above `x = whatever` must keep
            // answering `Person`, rather than being displaced by a name that will be
            // guessed from. The guess is made from *this* variable's own spelling, below.
            if matches!(receiver, Receiver::Unknown | Receiver::Named(_)) {
                continue;
            }
            let slot = if relays_a_block_parameter(&receiver) {
                &mut relayed
            } else if rooted_in_self(&receiver) {
                &mut rooted
            } else {
                &mut solid
            };
            if slot.as_ref().is_none_or(|(at, _)| write.at >= *at) {
                *slot = Some((write.at, receiver));
            }
        }
        solid
            .or(relayed)
            .map(|(_, receiver)| receiver)
            // No assignment produced a type, so the variable may be a **block parameter**, and
            // what the block was handed is written down in the signature of the method it was
            // passed to. Asked after the writes and never before them, so no answer this
            // already gave can be displaced by it — the same order `Receiver::Spelled` keeps
            // between an assignment and a name.
            .or_else(|| self.yielded_to(span, name, depth))
            // And only then a write rooted in a call on `self`, which may resolve to nothing at
            // all — see [`rooted_in_self`]. It is still a *precedence* and not a refusal: with
            // no other write and no block around it, the chain is what is taken.
            .or_else(|| rooted.map(|(_, receiver)| receiver))
            .map_or_else(
                || Receiver::Named(name.to_owned()),
                // The shape *and* the spelling, because the shape can still fail to type: a
                // chain through a method nothing declares has to end where a bare `story` ends
                // rather than below it. See `Receiver::Spelled`.
                |receiver| Receiver::Spelled {
                    was: Box::new(receiver),
                    name: name.to_owned(),
                },
            )
    }

    /// Give a block parameter the type the called method says its block receives.
    ///
    /// The **innermost** enclosing block wins, which is the one place this walk has to care
    /// about nesting: `stories.each { |s| s.tags.each { |s| ... } }` is legal, and the inner
    /// `s` is the one a read inside the inner block means. Blocks that do not contain the read
    /// are not candidates at all, which is what makes this narrower than
    /// [`Finder::type_the_local`] — a write anywhere above the cursor counts there, and a
    /// parameter of a block the cursor is not inside means nothing here.
    fn yielded_to(&self, span: (u32, u32), name: &str, depth: usize) -> Option<Receiver> {
        let parameter = self
            .yielded
            .iter()
            .filter(|parameter| {
                parameter.body.0 <= span.0
                    && span.1 <= parameter.body.1
                    && self.source[parameter.name.0 as usize..parameter.name.1 as usize] == *name
            })
            // Innermost: the shortest span that still contains the read.
            .min_by_key(|parameter| parameter.body.1 - parameter.body.0)?;
        let Receiver::Returned { on, method, .. } =
            self.receiver_of(Some(&parameter.call), depth + 1)
        else {
            // A call with no receiver written is an implicit `self` and reaches `Unknown`, so
            // there is no signature to ask: `each { |x| }` in a model body is a `yield`, and
            // what a `yield` hands over is the body of the method rather than a declaration.
            return None;
        };
        Some(Receiver::Yielded {
            on,
            method,
            index: parameter.index,
        })
    }

    /// Give an instance variable the type of an assignment that shares its `self`.
    ///
    /// **Which `@foo` this is is not a syntactic question**, and that is why the answer comes
    /// from [`scopes`] rather than from this walk. `@v` in `def a` and `@v` in `def self.b` are
    /// two variables; a `def c` inside `class << self` shares the second. That algebra was
    /// written for `documentHighlight` and is asked here unchanged — a second copy of
    /// it in this module is the one thing certain to drift, and a highlight and a completion
    /// disagreeing about which `@foo` is which would be invisible in both.
    ///
    /// The price is a second parse of the file, on this path only. It buys the guarantee that
    /// the two walks cannot disagree, which is worth more than the microsecond.
    ///
    /// The textually last assignment that produced a type wins — *last*, not last-before-the-
    /// cursor, because `initialize` is as often below the method reading `@foo` as above it.
    /// Bounded to the file: a class reopened elsewhere is a second question, and the honest
    /// first answer is not to look.
    fn type_the_instance_variable(&self, span: (u32, u32), depth: usize) -> Receiver {
        // The name is what is left when no assignment answers, and the two rungs below the
        // graph both work from it. `@` included, because that is how it is written and how
        // `scopes` spans it.
        let spelling = &self.source[span.0 as usize..span.1 as usize];
        let named = || Receiver::Named(spelling.to_owned());
        let repaired = self.without_the_half_typed_call();
        let Some((_, occurrences)) = scopes::variable(&repaired, span.0) else {
            return named();
        };
        // Two slots and the same rule `type_the_local` keeps, which is a rule this loop had
        // been missing rather than one invented for it: a write whose value can still end
        // up a guess is taken only where no write produced a shape that cannot.
        let mut solid: Option<(u32, Receiver)> = None;
        let mut relayed: Option<(u32, Receiver)> = None;
        let mut rooted: Option<(u32, Receiver)> = None;
        for occurrence in occurrences.iter().filter(|occurrence| occurrence.write) {
            let Some(write) = self
                .instance_writes
                .iter()
                .find(|write| write.name == (occurrence.start, occurrence.end))
            else {
                continue;
            };
            // `@foo = @foo.bar` reads the variable inside the write that assigns it, and the
            // write cannot be the answer for that read. Same rule as a local's, expressed
            // against the value's span because an instance variable has no "before".
            if write.value_span.0 <= span.0 && span.1 <= write.value_span.1 {
                continue;
            }
            // And it cannot be the answer for **any** read of that variable, not only for one
            // written inside it: the value's type is the question being asked. `type_the_local`
            // gets this rule from position — its candidates are the writes *before* the read,
            // which strictly shrinks at every hop — and an instance variable, which has no
            // "before" because a write in another method is a legitimate answer, has to state
            // it.
            //
            // Stating it is also what stops the walk exploding. `MAX_CHAIN` bounds how *deep*
            // the recursion goes and says nothing about how *wide* it is: every write of `@x`
            // is a candidate for every read of it, so typing one read visits them all, and
            // each `@x = @x.foo` asks the same question again. discourse's
            // `lib/topics_filter.rb` assigns `@scope` sixty times, thirty-nine of them from
            // itself — 59^8 paths under a depth bound of eight. Measured there: **35 s for one
            // `textDocument/definition`**, against 114 ms for the next slowest request on that
            // corpus, and 30 ms once this arm is taken.
            if occurrences.iter().any(|read| {
                !read.write && write.value_span.0 <= read.start && read.end <= write.value_span.1
            }) {
                continue;
            }
            let receiver = self.receiver_of(Some(&write.value), depth + 1);
            // See `type_the_local`: a name is not a type, and letting one win here would lose
            // an exact assignment written above it.
            if matches!(receiver, Receiver::Unknown | Receiver::Named(_)) {
                continue;
            }
            let slot = if relays_a_block_parameter(&receiver) {
                &mut relayed
            } else if rooted_in_self(&receiver) {
                &mut rooted
            } else {
                &mut solid
            };
            *slot = Some((occurrence.start, receiver));
        }
        let typed = solid.or(relayed).or(rooted);
        // The spelling survives an assignment here for the reason it does for a local, and the
        // shape it wraps is the whole `Assigned` — so a chain that types keeps the note naming
        // the line it was assigned on, and one that does not falls to the rungs a bare `@story`
        // would have reached.
        typed.map_or_else(named, |(at, was)| Receiver::Spelled {
            was: Box::new(Receiver::Assigned {
                at,
                was: Box::new(was),
            }),
            name: spelling.to_owned(),
        })
    }

    /// A call's return value, as the shape a lookup can be made from.
    ///
    /// Nothing is followed here and no type is decided: this records that a name was called on
    /// something, and [`types`](super::types) is where RBS is asked what that returns. A call
    /// with no receiver written **is** one of these, on [`Receiver::SelfObject`] — which is what
    /// Ruby says it is, and a question about the graph rather than about the text.
    fn returned_by(&self, node: &Node<'_>, depth: usize) -> Receiver {
        let Some(call) = node.as_call_node() else {
            return Receiver::Unknown;
        };
        let method = call
            .message_loc()
            .map(|message| &self.source[message.start_offset()..message.end_offset()])
            .unwrap_or_default();
        // `foo.()` is `foo.call()` written with no name at all, and a call Prism recovered with
        // no message span at all is the same fact spelled differently. One check, because to
        // everything below this line they are one thing: there is no name here.
        if method.is_empty() {
            return Receiver::Unknown;
        }
        let Some(receiver) = call.receiver() else {
            // No receiver written is an implicit `self`, and this is that sentence taken
            // literally. Until it, this returned `Receiver::Named(method)` — the *name* rung,
            // two below the graph — so `api_key_scopes.first` on a model guessed at a class
            // called `ApiKeyScopes` and then offered 824 possible definitions, while
            // `self.api_key_scopes.first` one character longer resolved exactly. Everything
            // needed to answer the first was already built and proven by the second.
            //
            // Nothing new is declared and no rung is added: `SelfObject` is the variant a
            // *written* `self` already produces, and `types::method_receiver` has typed it as
            // the enclosing class.
            let returned = Receiver::Returned {
                on: Box::new(Receiver::SelfObject),
                method: method.to_owned(),
                block: call.block().is_some(),
                arity: arity_of(&call),
            };
            // The name rung is kept **below** the lookup rather than beside it, which is
            // `Receiver::Spelled`'s whole reason to exist: `types` asks the shape first and
            // reaches the name only where that answered nothing, so a chain that resolves can
            // never be displaced by a guess and a guess is never deleted by a chain that failed.
            //
            // Only a bare name reaches it, and the guard is the one that was here before —
            // narrowed to what it was always a bound on. `find(id).title` and `each { }.first`
            // are expressions whose *spelling* says nothing about what they return, so no guess
            // may be made from them; asking the graph what `self.find(id)` returns is not a
            // guess at all, and refusing to ask was conservative about the wrong half.
            return if call.arguments().is_none() && call.block().is_none() {
                Receiver::Spelled {
                    was: Box::new(returned),
                    name: method.to_owned(),
                }
            } else {
                returned
            };
        };
        let on = self.receiver_of(Some(&receiver), depth + 1);
        // One `Unknown` ends the chain rather than being carried up it: with nothing to look
        // the method up *on*, every link above this one is unanswerable too, and a `Returned`
        // wrapped around an `Unknown` would only make the graph side rediscover that.
        if matches!(on, Receiver::Unknown) {
            return Receiver::Unknown;
        }
        Receiver::Returned {
            on: Box::new(on),
            method: method.to_owned(),
            block: call.block().is_some(),
            arity: arity_of(&call),
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

/// The single expression inside `( ... )`, when that is what the node is.
fn unparenthesised<'pr>(node: &Node<'pr>) -> Option<Node<'pr>> {
    let body = node.as_parentheses_node()?.body()?;
    let mut statements = body.as_statements_node()?.body().iter();
    let only = statements.next()?;
    // `(a; b)` evaluates to `b`, but a receiver written that way is nobody's real code and
    // guessing at it is how a classification starts being wrong.
    statements.next().is_none().then_some(only)
}

/// How many positional arguments a call wrote, as [`Arity`] spells it.
///
/// A keyword hash is not a positional argument and is not counted, on either side of the
/// question: `3.7.round(half: :up)` writes none, and RBS's `(?half: :up | :down | :even)` takes
/// none. Prism reads a bare `k => v` tail as one, `**opts` included, which is Ruby 3's own rule;
/// braces make it a `Hash` somebody passed positionally and it counts again.
///
/// A splat gives up on the count rather than guessing at it, and so does `...`. Guessing low
/// would silently pick the arm with the fewest parameters, which is exactly the "falls to the
/// nearest one" this partition exists to refuse.
fn arity_of(call: &CallNode<'_>) -> Arity {
    let Some(arguments) = call.arguments() else {
        return Arity::Exactly(0);
    };
    let mut written = 0_u32;
    for argument in arguments.arguments().iter() {
        if argument.as_splat_node().is_some() || argument.as_forwarding_arguments_node().is_some() {
            return Arity::Unknown;
        }
        if argument.as_keyword_hash_node().is_none() {
            written += 1;
        }
    }
    Arity::Exactly(written)
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

/// The name a keyword argument is written under, when the node is one.
///
/// Both of Ruby's spellings, because Ruby accepts both: `f(name: "ada")` and `f(:name => "ada")`
/// pass the same keyword, and `def f(name:)` is satisfied by either. `value_loc` is the name
/// without whichever colon it was written with. A key that is not a symbol — `f("name" => 1)` —
/// is a hash entry rather than a keyword, and has no name to give.
fn keyword_name(source: &str, element: &Node<'_>) -> Option<String> {
    let key = element.as_assoc_node()?.key();
    let name = key.as_symbol_node()?.value_loc()?;
    Some(source[name.start_offset()..name.end_offset()].to_owned())
}

/// The span of a local variable read, which is one of the two receivers whose type can be
/// recovered from somewhere else in the file.
fn local_span(node: &Node<'_>) -> Option<(u32, u32)> {
    let read = node.as_local_variable_read_node()?;
    let location = read.location();
    Some((location.start_offset() as u32, location.end_offset() as u32))
}

/// The span of an instance variable read, `@` included — which is how [`scopes`] spans one.
fn instance_span(node: &Node<'_>) -> Option<(u32, u32)> {
    let read = node.as_instance_variable_read_node()?;
    let location = read.location();
    Some((location.start_offset() as u32, location.end_offset() as u32))
}

#[cfg_attr(coverage_nightly, coverage(off))]
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

    /// The call the `~` is passing an argument to.
    fn call(marked: &str) -> Option<Call> {
        let offset = marked.find('~').expect("a ~ marking the cursor") as u32;
        call_at(&marked.replace('~', ""), offset)
    }

    /// Which argument the `~` is writing, as `Nth` or the keyword's name.
    fn active(marked: &str) -> Active {
        call(marked).expect("a call around the cursor").active
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

    /// [`self_call`]'s shape with an argument count, for the widened-guard half.
    fn self_call_with(arity: u32, name: &str) -> Receiver {
        Receiver::Returned {
            on: Box::new(Receiver::SelfObject),
            method: name.to_owned(),
            block: false,
            arity: Arity::Exactly(arity),
        }
    }

    /// The shape for a bare `name` written with no receiver: the lookup on `self`, with
    /// the method's own spelling kept under it for the rung below.
    ///
    /// The same name one rung further up, with the name-based answer still beneath it.
    fn self_call(name: &str) -> Receiver {
        Receiver::Spelled {
            was: Box::new(Receiver::Returned {
                on: Box::new(Receiver::SelfObject),
                method: name.to_owned(),
                block: false,
                arity: Arity::Exactly(0),
            }),
            name: name.to_owned(),
        }
    }

    /// The shape a variable's assignment produced, with the fall-through spelling peeled off.
    ///
    /// Every variable an assignment types is a [`Receiver::Spelled`] — the shape, and the name
    /// to try if nothing can be made of it. Almost every test here is about the shape alone,
    /// and the wrapper is pinned once by
    /// `a_typed_variable_still_carries_the_name_it_is_written_as` rather than repeated into
    /// assertions that would then each be about two things.
    fn typed(marked: &str) -> Receiver {
        match receiver(marked) {
            Receiver::Spelled { was, .. } => *was,
            other => panic!("expected a variable an assignment typed, got {other:?}"),
        }
    }

    #[test]
    fn nothing_written_after_the_cursor_is_something_the_cursor_is_inside() {
        // Every span this module tests is a pair of bounds, and a fixture written *around* the
        // cursor only ever exercises the upper one. A comment, a `::` and a literal that all
        // begin after the offset have to leave the classification alone — miss the lower bound
        // and completion goes silent on the first line of a file that has a string later in it.
        assert!(
            matches!(
                context("~\n# a note\nHR::Person\n\"later\"\n"),
                Context::Expression
            ),
            "a comment, a namespace and a string that all start later are not where the cursor is"
        );
    }

    #[test]
    fn a_call_with_no_name_in_it_is_not_somewhere_to_complete() {
        // `foo.()` is `foo.call()` written with no name at all, and it is the only shape Prism
        // gives a call operator and no message. Neither half of `visit_call_node` may claim it:
        // there is no name for a completion to replace, and no signature for the parentheses to
        // be the arguments of.
        assert!(
            matches!(context("foo.(~)"), Context::Expression),
            "the parentheses of a `.()` call are not an argument list we know the callee of"
        );
    }

    #[test]
    fn a_comment_ends_at_its_line_and_code_after_it_is_code() {
        // Both bounds of the same test. Completion must not fire inside a comment, and must
        // fire again on the line below one — the check is inclusive of the end because a
        // comment's span stops at its last character, and the cursor parks past it.
        assert!(at_marker("# a note ~").is_none(), "inside the comment");
        assert!(at_marker("# a note~").is_none(), "at its last character");
        assert!(
            matches!(context("# a note\n\"x\".~"), Context::MethodCall { .. }),
            "the line below a comment is code"
        );
        assert!(
            matches!(context("x = 1 # why\n\"y\".~"), Context::MethodCall { .. }),
            "and so is the line below a trailing comment"
        );
    }

    #[test]
    fn a_local_takes_the_type_of_the_last_assignment_that_had_one() {
        // The one place this module can be confidently wrong, so the rule is stated: the
        // textually last preceding assignment whose value ends before the cursor — and
        // assignments whose value has no knowable type are passed over rather than taken.
        assert_eq!(typed("x = 1\nx.~"), Receiver::Literal("Integer"));
        assert_eq!(
            typed("x = whatever\nx = \"s\"\nx.~"),
            Receiver::Literal("String"),
            "an untypeable assignment is not the answer when a typed one exists"
        );
        assert_eq!(
            typed("x = 1\nother = \"s\"\nx.~"),
            Receiver::Literal("Integer"),
            "and neither is a later assignment to a different name"
        );
        assert_eq!(
            typed("x = \"s\"\nx = whatever\nx.~"),
            Receiver::Literal("String"),
            "and it does not erase one either"
        );
        assert_eq!(
            receiver("x = whatever\nx.~"),
            Receiver::Spelled {
                was: Box::new(self_call("whatever")),
                name: "x".to_owned(),
            },
            "with nothing declared anywhere, what is left is two names one below the other — \
             the assignment's, because `whatever` is a call on `self`, and then the \
             *local's*, which is what the writer called it and the one a guess is still made \
             from last"
        );
    }

    #[test]
    fn a_cursor_past_a_literal_is_out_of_it_again() {
        // The bound that lets `"foo".` complete at all: the closing quote is where the next
        // `.` gets typed, and everything after the literal is ordinary code.
        assert!(at_marker("\"foo~\"").is_none(), "inside the string");
        assert!(
            matches!(context("[\"foo\", ~]"), Context::Expression),
            "past the string, inside the array"
        );
        assert!(
            matches!(context(":foo\nbar~"), Context::Expression),
            "the line after a symbol"
        );
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
            // The interpolated forms are the same classes: `"a#{b}"` is a String however the
            // pieces were assembled, and Prism gives each of them a node of its own.
            ("`ls #{dir}`.~", "String"),
            (r#":"a#{b}".~"#, "Symbol"),
            ("/re#{x}/.~", "Regexp"),
            ("__ENCODING__.~", "Encoding"),
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
    fn only_new_makes_an_instance_and_every_other_call_is_a_chain() {
        // `new` is the one factory this module can name on its own, and that has not changed.
        // What happens to the others: instead of `Unknown` they
        // become the *shape* of a lookup, which the graph side answers or does not. Nothing
        // here has decided that `Person.build` returns a `Person` — only that whether it does
        // is a question about `Person`'s singleton.
        assert_eq!(
            receiver("Person.build.~"),
            Receiver::Returned {
                on: Box::new(Receiver::Constant(6)),
                method: "build".to_owned(),
                block: false,
                arity: Arity::Exactly(0),
            }
        );
        // `person` is a bare name nothing assigned, which Ruby reads as `self.person` — so
        // the chain runs on through it, and the name it would otherwise be answered by
        // sits underneath as the rung to fall to. Nothing here has decided anything about
        // either.
        assert_eq!(
            receiver("person.new.~"),
            Receiver::Returned {
                on: Box::new(self_call("person")),
                method: "new".to_owned(),
                block: false,
                arity: Arity::Exactly(0),
            }
        );
    }

    #[test]
    fn a_chain_carries_how_many_positional_arguments_the_call_wrote() {
        // The other half of the fact `block` already carries. Nothing here has decided that
        // `first(3)` returns an `Array` — only that whoever asks is asking about a call with
        // one argument, which is a different question from one with none.
        let arity = |source: &str| match receiver(source) {
            Receiver::Returned { arity, .. } => arity,
            other => panic!("{other:?}"),
        };
        assert_eq!(arity("[1, 2].first.~"), Arity::Exactly(0));
        assert_eq!(arity("[1, 2].first(3).~"), Arity::Exactly(1));
        assert_eq!(arity("\"x\".sub(\"a\", \"b\").~"), Arity::Exactly(2));
        // Keywords are not positional arguments, and RBS counts them apart too: `3.7.round`
        // and `3.7.round(half: :up)` reach the same arm and it is the zero-argument one.
        assert_eq!(arity("3.7.round(half: :up).~"), Arity::Exactly(0));
        assert_eq!(arity("3.7.round(1, half: :up).~"), Arity::Exactly(1));
        // `**opts` is the same fact spelled differently, and Prism reads a bare `k => v` tail
        // as keywords whatever the keys are — Ruby 3's own rule. Braces make it a `Hash`
        // somebody passed positionally, and that counts.
        assert_eq!(arity("f.g(**opts).~"), Arity::Exactly(0));
        assert_eq!(arity("f.g(\"a\" => 1).~"), Arity::Exactly(0));
        assert_eq!(arity("f.g({ \"a\" => 1 }).~"), Arity::Exactly(1));
        // A block argument is a block and not an argument, which is what `block` already said.
        assert_eq!(arity("f.map(&:upcase).~"), Arity::Exactly(0));
        // And a count that cannot be taken is said rather than guessed at. Guessing low would
        // pick the arm with the fewest parameters, which is the "falls to the nearest one" the
        // partition exists to refuse.
        assert_eq!(arity("f.g(*args).~"), Arity::Unknown);
        assert_eq!(arity("f.g(1, *rest).~"), Arity::Unknown);
        assert_eq!(arity("def wrap(...)\n  f.g(...).~\nend\n"), Arity::Unknown);
    }

    #[test]
    fn a_chain_is_the_receiver_it_was_written_on_and_the_name_it_called() {
        assert_eq!(
            receiver("\"hi\".upcase.~"),
            Receiver::Returned {
                on: Box::new(Receiver::Literal("String")),
                method: "upcase".to_owned(),
                block: false,
                arity: Arity::Exactly(0),
            }
        );
        // Chains compose, innermost first, and the shape says which order to resolve them in.
        assert_eq!(
            receiver("\"hi\".upcase.strip.~"),
            Receiver::Returned {
                on: Box::new(Receiver::Returned {
                    on: Box::new(Receiver::Literal("String")),
                    method: "upcase".to_owned(),
                    block: false,
                    arity: Arity::Exactly(0),
                }),
                method: "strip".to_owned(),
                block: false,
                arity: Arity::Exactly(0),
            }
        );
    }

    #[test]
    fn one_unknown_ends_the_chain_rather_than_being_carried_up_it() {
        // `x.()` is `x.call` written with no name at all, so there is no message to look
        // anything up by — and a `Returned` wrapped around an `Unknown` would only make the
        // graph side rediscover that.
        assert_eq!(receiver("x.().foo.~"), Receiver::Unknown);
        // Two shapes are deliberately off that list. `thing(1)` and `thing { }` are calls on an
        // implicit `self` exactly as a bare `thing` is, and asking the graph what one returns
        // is not a guess; what their *spelling* cannot support is the rung below, which is all
        // the guard here was ever a bound on. So the chain carries the lookup, with no name
        // under it.
        assert_eq!(
            receiver("thing(1).foo.~"),
            Receiver::Returned {
                on: Box::new(Receiver::Returned {
                    on: Box::new(Receiver::SelfObject),
                    method: "thing".to_owned(),
                    block: false,
                    arity: Arity::Exactly(1),
                }),
                method: "foo".to_owned(),
                block: false,
                arity: Arity::Exactly(0),
            }
        );
        let Receiver::Returned { on, .. } = receiver("thing { }.foo.~") else {
            panic!("a chain");
        };
        assert_eq!(
            *on,
            Receiver::Returned {
                on: Box::new(Receiver::SelfObject),
                method: "thing".to_owned(),
                block: true,
                arity: Arity::Exactly(0),
            },
            "and the block the call wrote is carried, because RBS tells the arms apart by it"
        );
        // A *bare* call keeps both: the lookup, and the name below it that the two rungs under
        // the graph read. `@user.name.` is exactly the expression this is for.
        assert_eq!(
            receiver("thing.foo.bar.~"),
            Receiver::Returned {
                on: Box::new(Receiver::Returned {
                    on: Box::new(self_call("thing")),
                    method: "foo".to_owned(),
                    block: false,
                    arity: Arity::Exactly(0),
                }),
                method: "bar".to_owned(),
                block: false,
                arity: Arity::Exactly(0),
            }
        );
        assert_eq!(receiver("build.~"), self_call("build"));
    }

    #[test]
    fn a_local_assigned_a_call_carries_the_call() {
        // `type_the_local` does not stop at literals and `.new`: it hands back whatever shape
        // the assigned expression has.
        assert_eq!(
            typed("shouted = \"hi\".upcase\nshouted.~\n"),
            Receiver::Returned {
                on: Box::new(Receiver::Literal("String")),
                method: "upcase".to_owned(),
                block: false,
                arity: Arity::Exactly(0),
            }
        );
    }

    #[test]
    fn a_local_assigned_from_itself_terminates() {
        // `x = x.foo` reads a local inside the write that declares it. Only assignments whose
        // value ends before the *read* are candidates, so the write cannot answer for the read
        // inside it — and the recursion ends on that rather than on `MAX_CHAIN`. What it ends
        // *at* is the name, which is one link and not a fixpoint.
        assert_eq!(
            typed("x = x.foo\nx.~\n"),
            Receiver::Returned {
                on: Box::new(Receiver::Named("x".to_owned())),
                method: "foo".to_owned(),
                block: false,
                arity: Arity::Exactly(0),
            }
        );
        // The inner `x` is itself a variable an assignment typed, so it arrives wrapped: the
        // fall-through is per-variable and nests exactly where variables do.
        assert_eq!(
            typed("x = \"hi\"\nx = x.upcase\nx.~\n"),
            Receiver::Returned {
                on: Box::new(Receiver::Spelled {
                    was: Box::new(Receiver::Literal("String")),
                    name: "x".to_owned(),
                }),
                method: "upcase".to_owned(),
                block: false,
                arity: Arity::Exactly(0),
            }
        );
    }

    #[test]
    fn a_chain_is_bounded_rather_than_followed_to_the_end() {
        // A chain is not a fixpoint: this runs on the analysis thread on a keystroke. Past
        // `MAX_CHAIN` links the answer is `Unknown`, which is what every other unanswerable
        // receiver gets.
        let long = format!("\"hi\"{}.~", ".upcase".repeat(MAX_CHAIN + 2));
        let mut links = 0;
        let mut at = &receiver(&long);
        while let Receiver::Returned { on, .. } = at {
            links += 1;
            at = on;
        }
        assert_eq!(*at, Receiver::Unknown);
        assert!(links < MAX_CHAIN, "{links} links");
    }

    #[test]
    fn a_local_takes_the_type_of_what_was_assigned_to_it() {
        assert_eq!(
            typed("name = \"ada\"\nname.~\n"),
            Receiver::Literal("String")
        );
        assert_eq!(
            typed("person = Person.new\nperson.~\n"),
            Receiver::Instance(15)
        );
    }

    #[test]
    fn the_nearest_preceding_assignment_wins() {
        assert_eq!(
            typed("x = \"a\"\nx = [1]\nx.~\n"),
            Receiver::Literal("Array")
        );
        // An assignment *after* the cursor is not in scope yet, whatever the parser saw.
        assert_eq!(
            typed("x = \"a\"\nx.~\nx = [1]\n"),
            Receiver::Literal("String")
        );
    }

    #[test]
    fn a_local_assigned_something_untypable_keeps_only_its_name() {
        // Two names, one below the other, and both survive the `self` rung. The assignment's value is
        // now a lookup — `self.compute` — and the local's own spelling is still the last thing
        // tried, so a workspace that declares no `compute` answers exactly what it answered
        // before.
        assert_eq!(
            receiver("x = compute\nx.~\n"),
            Receiver::Spelled {
                was: Box::new(self_call("compute")),
                name: "x".to_owned(),
            }
        );
        // Never assigned at all: a bare word, which Prism reads as a receiverless call — so it
        // is the *call's* shape, with the same spelling under it.
        assert_eq!(receiver("x.~\n"), self_call("x"));
        // `x = x.` must not type `x` from the half-written statement it is part of. It stays a
        // bare `Named` and the `self` rung does not reach it: the assignment above makes `x` a *local*
        // to Prism, so this is the local's own fall-through and never a call on `self`.
        assert_eq!(receiver("x = x.~\n"), Receiver::Named("x".to_owned()));
    }

    #[test]
    fn a_typed_variable_still_carries_the_name_it_is_written_as() {
        // The wrapper every other test here peels, pinned once. Both halves have to be present
        // at the same time: the shape, so a chain that types is answered from the code; and the
        // spelling, so a chain that does not is answered by the same rung a bare name reaches
        // rather than by nothing at all.
        assert_eq!(
            receiver("story = Story.where(x).first\nstory.~\n"),
            Receiver::Spelled {
                was: Box::new(Receiver::Returned {
                    on: Box::new(Receiver::Returned {
                        on: Box::new(Receiver::Constant(13)),
                        method: "where".to_owned(),
                        block: false,
                        arity: Arity::Exactly(1),
                    }),
                    method: "first".to_owned(),
                    block: false,
                    arity: Arity::Exactly(0),
                }),
                name: "story".to_owned(),
            }
        );
        // An instance variable wraps the *whole* `Assigned`, so a chain that types keeps the
        // note naming the line it was assigned on and only a chain that fails reaches the name.
        assert_eq!(
            receiver(
                "class C\n  def a\n    @story = fetch.first\n  end\n  def b\n    @story.~\n  end\nend\n"
            ),
            Receiver::Spelled {
                was: Box::new(Receiver::Assigned {
                    at: 20,
                    was: Box::new(Receiver::Returned {
                        on: Box::new(self_call("fetch")),
                        method: "first".to_owned(),
                        block: false,
                        arity: Arity::Exactly(0),
                    }),
                }),
                name: "@story".to_owned(),
            }
        );
    }

    /// The assignment an instance variable was typed from, and what it assigned.
    fn assigned(marked: &str) -> (u32, Receiver) {
        match typed(marked) {
            Receiver::Assigned { at, was } => (at, *was),
            other => panic!("expected an assignment, got {other:?}"),
        }
    }

    #[test]
    fn an_instance_variable_takes_the_type_of_an_assignment_in_its_class() {
        // An instance variable is typed from an assignment in its own class, and what makes
        // that cheap is that `scopes` already decides which `@foo` is which.
        assert_eq!(
            assigned(
                "class Person\n  def initialize\n    @name = \"ada\"\n  end\n\n  def shout\n    @name.~\n  end\nend\n"
            ),
            (34, Receiver::Literal("String"))
        );
        // And the assignment can be written *below* the method that reads it, which is why
        // "textually last" here is not "textually last before the cursor" — and why the
        // half-typed `.` has to be blanked before the question is asked. Without that repair
        // Prism reads the `end` below the cursor as the method name, the rest of the class is
        // reparented into a nested `def`, and this `@name` is correctly reported as a different
        // variable belonging to a different `self`.
        assert_eq!(
            assigned("class Person\n  def shout\n    @name.~\n  end\n\n  def initialize\n    @name = \"ada\"\n  end\nend\n").1,
            Receiver::Literal("String")
        );
        // The other shape of half-typed call: a word already begun. Here the message blanks
        // with the operator, because it ends at the cursor rather than past it.
        assert_eq!(
            assigned("class Person\n  def shout\n    @name.up~\n  end\n\n  def initialize\n    @name = \"ada\"\n  end\nend\n").1,
            Receiver::Literal("String")
        );
    }

    #[test]
    fn an_instance_variable_in_another_self_is_a_different_variable() {
        // `@v` in `def a` and `@v` in `def self.b` belong to two different objects. Joining
        // them would be a confidently wrong answer, which is worse than the `Unknown` this
        // shipped with — and the rule is `scopes`'s, asked rather than re-derived.
        assert_eq!(
            receiver(
                "class Person\n  def self.build\n    @seed = \"x\"\n  end\n\n  def shout\n    @seed.~\n  end\nend\n"
            ),
            Receiver::Named("@seed".to_owned()),
            "the singleton's assignment is not this variable's, so nothing but the name is left"
        );
    }

    #[test]
    fn two_assignments_of_different_classes_answer_the_last_one() {
        // The same caveat `type_the_local` carries, extended to a second shape: a wrong answer
        // rather than an absent one, and the reason the provenance footnote exists.
        let (_, was) = assigned(
            "class Person\n  def a\n    @v = \"s\"\n  end\n  def b\n    @v = 1\n  end\n  def c\n    @v.~\n  end\nend\n",
        );
        assert_eq!(was, Receiver::Literal("Integer"));
    }

    #[test]
    fn a_memoised_instance_variable_is_an_assignment() {
        // `@cache ||= …` is how Ruby spells memoisation, and it is as much an assignment as
        // `=`. `@n += 1` is not, and stays unknown: an operator write says what happens to a
        // value rather than what it is.
        assert_eq!(
            assigned("class C\n  def cache\n    @cache ||= \"x\"\n  end\n  def use\n    @cache.~\n  end\nend\n").1,
            Receiver::Literal("String")
        );
        assert_eq!(
            receiver("class C\n  def bump\n    @n += 1\n  end\n  def use\n    @n.~\n  end\nend\n"),
            Receiver::Named("@n".to_owned())
        );
    }

    #[test]
    fn an_instance_variable_assigned_from_itself_terminates() {
        // `@v = @v.foo` reads the variable inside the write that assigns it, and that write
        // cannot be the answer for that read.
        assert_eq!(
            receiver("class C\n  def a\n    @v = @v.~\n  end\nend\n"),
            Receiver::Named("@v".to_owned())
        );
    }

    #[test]
    fn an_instance_variable_assigned_from_itself_many_times_is_typed_by_the_one_that_can() {
        // The wide case, which the single-assignment test above does not reach. Every write of
        // `@v` is a candidate for every read of it, so typing one read visits them all, and
        // each `@v = @v.foo` reads `@v` again — `MAX_CHAIN` bounds that at depth eight and
        // says nothing about the breadth. discourse's `lib/topics_filter.rb` assigns `@scope`
        // sixty times, thirty-nine of them from itself: measured there before the guard,
        // **35 s for one `textDocument/definition`**, against 114 ms for the next slowest
        // request on that corpus, and 30 ms after it.
        //
        // Without the guard this test does not fail, it **hangs** — which is the honest shape
        // of the defect and the reason the assertion below is about the answer rather than
        // about a duration. The answer is the point too: refusing re-entry must not cost the
        // one write that can type the variable.
        let mut source = String::from("class Person\n  def initialize\n    @name = \"ada\"\n");
        for _ in 0..12 {
            source.push_str("    @name = @name.strip\n");
        }
        source.push_str("  end\n\n  def shout\n    @name.~\n  end\nend\n");
        assert_eq!(assigned(&source).1, Receiver::Literal("String"));
    }

    #[test]
    fn an_instance_variable_nothing_assigned_keeps_only_its_name() {
        assert_eq!(
            receiver("class C\n  def a\n    @v.~\n  end\nend\n"),
            Receiver::Named("@v".to_owned())
        );
        // Assigned a receiverless call is `self.compute`, and every rung this
        // answered by before is still under it in order: the assignment's own name, then — when
        // `method_receiver` returns nothing for the whole `Assigned` — the variable's, which is
        // the one a guess is made from.
        assert_eq!(
            receiver("class C\n  def a\n    @v = compute\n  end\n  def b\n    @v.~\n  end\nend\n"),
            Receiver::Spelled {
                was: Box::new(Receiver::Assigned {
                    at: 20,
                    was: Box::new(self_call("compute")),
                }),
                name: "@v".to_owned(),
            }
        );
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
        // This module still guesses nothing, and that is what the two `Spelled` names are: the
        // spelling, not a type. Whether a `p` can be a `P` is a question for the graph, one
        // rung further down, and a setting can turn the answer off — none of which is decided
        // here. The `self` rung adds a *lookup* above them and no guess of its own.
        assert_eq!(
            context("p = build_person\np.~\n"),
            Context::MethodCall {
                receiver: Receiver::Spelled {
                    was: Box::new(self_call("build_person")),
                    name: "p".to_owned(),
                }
            }
        );
        assert_eq!(
            context("@person.sh~\n"),
            Context::MethodCall {
                receiver: Receiver::Named("@person".to_owned())
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
    fn a_block_parameter_carries_the_call_that_hands_it_over() {
        // The block-parameter shape, and nothing here has resolved anything: the receiver records *which
        // call* the block was written on and *which* of its parameters this is, and what the
        // signature says they are is `types`'.
        assert_eq!(
            context("Story.where(id: 1).each do |story|\n  story.~\nend\n"),
            Context::MethodCall {
                receiver: Receiver::Spelled {
                    was: Box::new(Receiver::Yielded {
                        on: Box::new(Receiver::Returned {
                            on: Box::new(Receiver::Constant(5)),
                            method: "where".to_owned(),
                            block: false,
                            arity: Arity::Exactly(0),
                        }),
                        method: "each".to_owned(),
                        index: 0,
                    }),
                    name: "story".to_owned(),
                }
            }
        );
    }

    #[test]
    fn the_innermost_block_a_name_is_a_parameter_of_wins() {
        // The one place this walk has to care about nesting. Shadowing a block parameter with
        // another of the same name is legal, and a read inside the inner block means the inner
        // one — where `type_the_local` deliberately treats a block's `x` and the outer `x` as
        // one variable, because they usually are.
        let Context::MethodCall {
            receiver: Receiver::Spelled { was, .. },
        } = context("a.each do |x|\n  b.map do |x|\n    x.~\n  end\nend\n")
        else {
            panic!("a method call on a block parameter");
        };
        let Receiver::Yielded { on, method, .. } = *was else {
            panic!("a yielded receiver");
        };
        assert_eq!(method, "map");
        assert_eq!(*on, self_call("b"));
    }

    #[test]
    fn a_block_on_a_call_with_no_receiver_is_asked_of_self() {
        // `each { |x| }` written with no receiver is an implicit `self`, and that
        // is a question rather than a dead end: the block's parameter is whatever `self.each`
        // says it yields. Where nothing declares it — a `yield` in the method's own body is not
        // a declaration anything can be asked about — the `Spelled` under it is the name rung,
        // which is where this answered before.
        assert_eq!(
            context("each do |story|\n  story.~\nend\n"),
            Context::MethodCall {
                receiver: Receiver::Spelled {
                    was: Box::new(Receiver::Yielded {
                        on: Box::new(Receiver::SelfObject),
                        method: "each".to_owned(),
                        index: 0,
                    }),
                    name: "story".to_owned(),
                }
            }
        );
    }

    #[test]
    fn an_assignment_from_a_block_parameter_does_not_displace_one_that_typed() {
        // Mastodon's `lib/paperclip/color_extractor.rb`, and the only two positions the block
        // half made worse anywhere. `max_distance_color = nil` above the loop typed it, and
        // `max_distance_color = color` inside the loop is the *later* write — so on position
        // alone it wins, and it answers nothing at all, because nothing declares what
        // `palette.each` hands its block. A relayed block parameter is a fallback wearing a
        // shape's clothes and is taken only when no other write produced one.
        let Context::MethodCall {
            receiver: Receiver::Spelled { was, .. },
        } = context("best = nil\npalette.each do |color|\n  best = color\nend\nbest.~\n")
        else {
            panic!("a method call on a local");
        };
        assert!(
            matches!(*was, Receiver::Literal(_)),
            "the `nil` above the loop lost to a block parameter that types nothing: {was:?}"
        );
    }

    #[test]
    fn an_assignment_rooted_in_a_call_on_self_never_displaces_one_that_typed() {
        // The precedence arm, and the second half of it was found by a corpus rather than
        // by reasoning. A receiverless call may resolve to nothing — `self` in a spec file, a
        // rake task or a top-level script is `Object` — so a write whose value is one must not
        // take the answer away from a write that produced a type.
        assert_eq!(
            typed("x = \"s\"\nx = whatever\nx.~"),
            Receiver::Literal("String"),
            "a bare call on `self`"
        );
        assert_eq!(
            typed("x = \"s\"\nx = whatever(1)\nx.~"),
            Receiver::Literal("String"),
            "and one that wrote an argument, which carries no name under it at all"
        );
        // **Through the whole chain.** chatwoot writes `tokens = user_tokens(a) + contact(b)`,
        // which is a call on a call on `self`: asked only about its last link it looks as solid
        // as `Foo.bar.baz`, and it displaced the `tokens = [x]` in the method above it. One
        // position in five corpora, and the only one this rung makes worse.
        assert_eq!(
            typed("x = [1]\nx = one(2) + two(3)\nx.~"),
            Receiver::Literal("Array"),
            "a chain rooted in a call on `self` is rooted in one however long it is"
        );
        // And it stays a *precedence*: with no other write, the chain is still what is taken.
        assert!(
            matches!(typed("x = whatever(1)\nx.~"), Receiver::Returned { .. }),
            "the only write there is has to be taken"
        );
        // Through a call and never through a **variable**: `link = c.links.last` where `c` is
        // itself a call on `self` is `c`'s problem, and `c` already carries its own name rung.
        // Read as rooted, a String literal in another `it` block took `link` away from it.
        assert_eq!(
            typed("l = \"s\"\nc = make(1)\nl = c.rows.last\nl.~"),
            Receiver::Returned {
                on: Box::new(Receiver::Returned {
                    on: Box::new(Receiver::Spelled {
                        was: Box::new(self_call_with(1, "make")),
                        name: "c".to_owned(),
                    }),
                    method: "rows".to_owned(),
                    block: false,
                    arity: Arity::Exactly(0),
                }),
                method: "last".to_owned(),
                block: false,
                arity: Arity::Exactly(0),
            }
        );
        // Below the block parameter as well as below a solid write, which is the third slot's
        // whole reason to exist: forem writes `uploader = upload_subforem_image(a, b)` in one
        // method and `Uploader.new.tap do |uploader|` in the next, and `type_the_local`
        // deliberately treats the two names as one variable.
        let Context::MethodCall {
            receiver: Receiver::Spelled { was, .. },
        } = context("u = make(1)\nItem.new.tap do |u|\n  u.~\nend\n")
        else {
            panic!("a method call on a local");
        };
        assert!(
            matches!(*was, Receiver::Yielded { .. }),
            "the block the cursor is inside beats a call on `self` in another method: {was:?}"
        );
    }

    #[test]
    fn a_block_parameter_relayed_through_an_assignment_is_still_taken_when_it_is_all_there_is() {
        // The other half of the same rule: it is a *precedence* and not a refusal, so a
        // variable whose only write is a block parameter still carries the shape.
        let Context::MethodCall {
            receiver: Receiver::Spelled { was, .. },
        } = context("stories.each do |story|\n  row = story\n  row.~\nend\n")
        else {
            panic!("a method call on a local");
        };
        assert!(
            relays_a_block_parameter(&was),
            "the only write there is has to be taken: {was:?}"
        );
    }

    #[test]
    fn a_name_read_outside_the_block_it_is_a_parameter_of_is_not_one() {
        // The bound that makes this narrower than an assignment: a write anywhere above the
        // cursor counts for `type_the_local`, and a parameter of a block the cursor is not
        // inside means nothing at all.
        assert_eq!(
            context("a.each do |story|\n  story\nend\nstory.~\n"),
            Context::MethodCall {
                receiver: self_call("story")
            }
        );
    }

    #[test]
    fn an_assignment_is_still_asked_before_the_block_it_is_written_in() {
        // The order is `Receiver::Spelled`'s and it is unchanged: a shape an assignment
        // produced can never be displaced by this, so every answer the crate already gave
        // survives the block-parameter rung by construction.
        let Context::MethodCall {
            receiver: Receiver::Spelled { was, .. },
        } = context("a.each do |story|\n  story = Person.new\n  story.~\nend\n")
        else {
            panic!("a method call on a local");
        };
        assert!(
            matches!(*was, Receiver::Instance(_)),
            "the assignment lost to the block parameter: {was:?}"
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
        //
        // `i` is a block parameter, so the reader wraps the name in the shape that asks what
        // `items.each` says it hands its block — and `Spelled` is what keeps the name rung
        // underneath, for the case where nothing declares one.
        assert_eq!(
            context("items.each do |i|\n  i.~\nend\n"),
            Context::MethodCall {
                receiver: Receiver::Spelled {
                    was: Box::new(Receiver::Yielded {
                        on: Box::new(self_call("items")),
                        method: "each".to_owned(),
                        index: 0,
                    }),
                    name: "i".to_owned(),
                }
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
    fn the_active_argument_is_how_many_have_been_finished() {
        assert_eq!(active("f(~"), Active::Nth(0), "nothing written yet");
        assert_eq!(active("f(1~"), Active::Nth(0), "still inside the first");
        assert_eq!(active("f(1,~"), Active::Nth(1), "the comma finished it");
        assert_eq!(active("f(1, ~"), Active::Nth(1), "and the space after it");
        assert_eq!(active("f(1, 2~)"), Active::Nth(1), "inside the second");
        assert_eq!(active("f(1, 2, ~)"), Active::Nth(2));
        assert_eq!(active("f(~1, 2)"), Active::Nth(0), "back at the first");
        assert_eq!(
            active("f(1, ~2, 3)"),
            Active::Nth(1),
            "at the start of the second"
        );
        // A block argument is an argument, and a `do ... end` block is not: it is written
        // outside the parentheses and the cursor in it is outside the call's arguments.
        assert_eq!(active("f(1, &blk~)"), Active::Nth(1));
        assert_eq!(call("f(1) do |x|\n  ~\nend\n"), None);
    }

    #[test]
    fn a_paren_less_call_still_says_which_argument_it_is_on() {
        assert_eq!(active("link_to \"x\", ~"), Active::Nth(1));
        assert_eq!(active("puts ~1, 2"), Active::Nth(0));
    }

    #[test]
    fn the_innermost_call_is_the_one_the_cursor_is_passing_to() {
        // The nested case, and the reason it needs no special handling: the walk is pre-order,
        // so the innermost call is the last to claim the cursor.
        let inner = call("outer(1, inner(2, ~))").expect("the inner call");
        assert_eq!(inner.name, 9, "`inner`, not `outer`");
        assert_eq!(inner.active, Active::Nth(1));

        let outer = call("outer(1, inner(2, 3), ~)").expect("the outer call");
        assert_eq!(outer.name, 0);
        assert_eq!(outer.active, Active::Nth(2));
    }

    #[test]
    fn a_keyword_argument_is_named_rather_than_counted() {
        // Keywords may be written in any order, so counting them answers the wrong parameter
        // the moment anybody does.
        assert_eq!(active("f(name: ~)"), Active::Keyword("name".to_owned()));
        assert_eq!(
            active("f(name: \"ada~\")"),
            Active::Keyword("name".to_owned())
        );
        assert_eq!(
            active("f(1, name: \"ada\", age: ~)"),
            Active::Keyword("age".to_owned())
        );
        assert_eq!(
            active("f(age: 1, name: ~)"),
            Active::Keyword("name".to_owned()),
            "written second, and still `name`"
        );
        // Ruby accepts both spellings for the same keyword, so both are named.
        assert_eq!(active("f(:name => ~)"), Active::Keyword("name".to_owned()));
        // A string key is a hash entry rather than a keyword, and a hash of them is one
        // argument however many pairs it holds.
        assert_eq!(active("f(\"name\" => ~)"), Active::Nth(0));
        assert_eq!(active("f(\"a\" => 1, \"b\" => 2, ~)"), Active::Nth(1));
    }

    #[test]
    fn a_keyword_hash_counts_as_its_own_elements() {
        // One Prism node, two arguments written. Without spreading it, every keyword after a
        // positional one answers the same parameter.
        assert_eq!(active("f(1, a: 2, b: ~)"), Active::Keyword("b".to_owned()));
        // Past a finished keyword and before the next: which one it will be is unknowable,
        // that it is a keyword is not — Ruby forbids a positional argument after one, so a
        // count here would answer with a parameter the call can no longer reach.
        assert_eq!(active("f(a: 1, ~)"), Active::AnyKeyword);
        assert_eq!(active("f(1, a: 2, ~)"), Active::AnyKeyword);
        assert_eq!(active("f(a: 1, b: 2, ~)"), Active::AnyKeyword);
    }

    #[test]
    fn a_heredoc_argument_ends_at_its_marker_and_not_at_its_body() {
        // `execute(<<~SQL, user_id)` is how anybody writes SQL, and the argument after the
        // heredoc is written three lines above the end of it. The counting rule holds anyway,
        // because Prism scopes the node to the opening marker and keeps the body separately.
        // Worth a test rather than a comment: the obvious reading of "where the node ends"
        // would put every later argument inside the first one. (`<<-` rather than `<<~` only
        // because a squiggly heredoc and this module's cursor marker are the same character.)
        for body in ["  body\n", "  body #{x}\n"] {
            let marked = format!("f(<<-TEXT, ~)\n{body}  TEXT\n");
            assert_eq!(active(&marked), Active::Nth(1), "{body:?}");
        }
        // And the cursor on the marker itself is still the first argument.
        assert_eq!(active("f(<<-TEXT~, 2)\n  body\n  TEXT\n"), Active::Nth(0));
    }

    #[test]
    fn a_call_with_nothing_to_resolve_is_not_a_call() {
        // `foo.()` is `foo.call()` written with no name at all: there is no callee to look up
        // and so no signature to show. The same shape `a_call_with_no_name_in_it` pins for
        // completion, from the other side.
        assert_eq!(call("foo.(~)"), None);
        assert_eq!(call("~"), None, "nowhere near a call");
        assert_eq!(call("f(1) ~"), None, "past the closing paren");
    }

    #[test]
    fn a_literal_and_a_comment_end_completion_and_not_the_signature() {
        // The two places `at` deliberately gives up. An editor keeps the signature popup on
        // screen through both, and a `null` makes it flicker on every keystroke.
        assert!(
            at_marker("f(\"hel~\")").is_none(),
            "nothing to complete in a string"
        );
        assert_eq!(
            active("f(\"hel~\")"),
            Active::Nth(0),
            "and still the first argument"
        );

        assert!(at_marker("f(1, # note~\n  2)").is_none());
        assert_eq!(active("f(1, # note~\n  2)"), Active::Nth(1));
    }

    #[test]
    fn an_operator_inside_the_parentheses_does_not_take_the_call_away() {
        // `an_operator_beats_the_argument_list_it_is_written_in` is the completion half of
        // this: what the cursor completes is `Person`'s methods, and what it is passing an
        // argument to is still `build`.
        let found = call("build(Person.~").expect("the enclosing call");
        assert_eq!(found.name, 0);
        assert_eq!(found.active, Active::Nth(0));
    }

    #[test]
    fn an_unparseable_file_does_not_panic() {
        assert!(at("class Broken\n  def foo\n", 5).is_some());
        assert!(at("", 0).is_some());
    }
}
