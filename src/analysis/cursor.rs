//! What the cursor is in the middle of typing.
//!
//! # Why this parses instead of reading the graph
//!
//! - **Completion fires on half-written Ruby.** The graph records finished text; `Foo::` and `foo.`
//!   are syntax errors.
//! - **Prism's error recovery classifies the cursor.** `Foo::` becomes a `ConstantPathNode` with an
//!   empty name span exactly at the cursor, and `foo.` a `CallNode` with an empty message span. The
//!   shapes below are read off that recovery, not rebuilt from raw text.
//! - **Scanning backwards through the text would fire inside comments, strings, and on a decimal's
//!   `.`.** It is used for one thing only, finding where the half-typed word starts, and only after
//!   Prism says the cursor is somewhere Ruby can be written.
//!
//! # Why this module has no graph
//!
//! What the receiver *is* needs a graph; where it is *written* does not. Keeping them apart lets
//! the classification be tested against a bare string, which makes the awkward cases (a trailing
//! `.` above an `end`, a cursor in the whitespace after a comma) cheap to enumerate.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;

use ruby_prism::{
    AssocNode, BlockNode, CallNode, ClassNode, ConstantOrWriteNode, ConstantPathNode,
    ConstantPathOrWriteNode, ConstantPathWriteNode, ConstantReadNode, ConstantWriteNode, DefNode,
    InstanceVariableAndWriteNode, InstanceVariableOrWriteNode, InstanceVariableWriteNode,
    ItLocalVariableReadNode, LambdaNode, LocalVariableReadNode, LocalVariableWriteNode, Location,
    MatchLastLineNode, ModuleNode, MultiWriteNode, Node, ParseResult, RegularExpressionNode,
    SingletonClassNode, StatementsNode, StringNode, SymbolNode, Visit, XStringNode,
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
    /// Inside an argument list, as in `foo(` or `bar(1, `. Everything an expression offers, plus
    /// the called method's keyword parameters, so it carries an offset inside that method's name
    /// for the caller to resolve.
    Argument { name: u32 },
}

impl Context {
    /// Whether Ruby allows a *private* method to be written where the cursor is.
    ///
    /// - **Ruby allows one with an implicit receiver**, and since 2.7 with a receiver spelled
    ///   `self`, through `.` and `::` alike. `other.secret` still raises inside the class that
    ///   declares `secret`. All checked against a real interpreter.
    /// - **Answered here, from syntax alone.**
    /// - **Stricter than rubydex on purpose.** rubydex passes a private method whenever the
    ///   caller's `self` is the receiver's class. Ruby's exemption is for a receiver *written*
    ///   `self`, not one that happens to be the same class.
    #[must_use]
    pub fn allows_private(&self) -> bool {
        match self {
            // No receiver written at all, so the call has one implicitly.
            Context::Expression | Context::Argument { .. } => true,
            Context::MethodCall { receiver } | Context::NamespaceAccess { receiver } => {
                matches!(*receiver, Receiver::SelfObject(_))
            }
        }
    }
}

impl Context {
    /// Move this context's offsets from the buffer's coordinates into the graph's.
    ///
    /// **Needed because indexing is deferred.** This module reads the buffer, while everything that
    /// consumes a `Receiver` (`constant_at`, `precise_call`) keys the *graph* with what it finds.
    /// The two agree wherever the graph holds what the buffer holds, and then this is the identity.
    /// Between a keystroke and the settle that indexes it, they differ.
    ///
    /// `None` means the receiver is written in text the graph has never seen, so no lookup on it
    /// can be trusted and the caller must index first.
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
    /// **The trap: `Assigned { at }` is left alone on purpose.** Every other offset here is a graph
    /// key. That one is provenance: `hover` renders it with `text.position_at(at)` against the
    /// *buffer* to name a line, so translating it would move the line every card prints. Do not
    /// "rebase every `u32` in `Receiver`".
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
                arguments,
            } => Receiver::Returned {
                on: Box::new(on.rebased(rebase)?),
                method: method.clone(),
                block: block.rebased(rebase)?,
                arity: *arity,
                // **All or none, never a `?`.** One missing argument would miscount every position
                // after it. The list is an optimisation on top of the chain, so dropping it only
                // costs the arm pick.
                arguments: arguments
                    .iter()
                    .map(|written| written.rebased(rebase))
                    .collect::<Option<Vec<_>>>()
                    .unwrap_or_default(),
            },
            Receiver::Yielded { on, method, index } => Receiver::Yielded {
                on: Box::new(on.rebased(rebase)?),
                method: method.clone(),
                index: *index,
            },
            // The index is a position on the *left* of an assignment, not in any text, so it
            // travels unchanged. The value it indexes is an ordinary shape and walks.
            Receiver::Destructured { of, index } => Receiver::Destructured {
                of: Box::new(of.rebased(rebase)?),
                index: *index,
            },
            Receiver::Spelled { was, name } => Receiver::Spelled {
                was: Box::new(was.rebased(rebase)?),
                name: name.clone(),
            },
            // Two nested shapes and a flag, with no offset of its own. Both sides must translate,
            // because the operator is evaluated against what they resolve to.
            Receiver::Negated(on) => Receiver::Negated(Box::new(on.rebased(rebase)?)),
            Receiver::Shortcut { left, right, and } => Receiver::Shortcut {
                left: Box::new(left.rebased(rebase)?),
                right: Box::new(right.rebased(rebase)?),
                and: *and,
            },
            // The body this offset falls in decides the answer, so it is a position in the graph's
            // text like `Constant`'s. A `self` captured above the cursor is exactly what a
            // keystroke moves.
            Receiver::SelfObject(offset) => Receiver::SelfObject(rebase.to_graph(*offset)?),
            // The offset moves for [`Receiver::SelfObject`]'s reason, and the default is a shape in
            // this same buffer, so it walks like any other.
            Receiver::Parameter {
                at,
                method,
                slot,
                default,
            } => Receiver::Parameter {
                at: rebase.to_graph(*at)?,
                method: method.clone(),
                slot: slot.clone(),
                default: match default {
                    Some(written) => Some(Box::new(written.rebased(rebase)?)),
                    None => None,
                },
            },
            // The offset moves for [`Receiver::SelfObject`]'s reason: the body it falls in says
            // what `super` means, and both halves (which class, which method) are read from the
            // graph at this position.
            Receiver::Super {
                at,
                method,
                block,
                arity,
            } => Receiver::Super {
                at: rebase.to_graph(*at)?,
                method: method.clone(),
                block: *block,
                arity: *arity,
            },
            // Nothing to move: a name, a literal, `::` and the two dead ends.
            Receiver::Literal { .. }
            | Receiver::TopLevel
            | Receiver::Named(_)
            | Receiver::Unknown => self.clone(),
        })
    }
}

impl Receiver {
    /// A literal that holds nothing this module can name.
    ///
    /// Every literal but an array, a hash and a range (their classes are generic over nothing),
    /// plus those three wherever their contents are not one written class. One spelling for both,
    /// because downstream they are the same absence: no argument at any position.
    #[must_use]
    pub fn literal(class: &'static str) -> Self {
        Self::Literal {
            class,
            arguments: Vec::new(),
        }
    }
}

/// Which parameter of a `def` a name means: counted from the left, or called by keyword.
///
/// - **Two spellings, not one index.** Ruby binds positionals by *where* they sit and keywords by
///   *name*, and so does every signature. One index would let `def f(a, b:)` answer `b` with
///   whatever was declared at position one.
/// - **A parameter after a `*rest` gets no slot**, for [`Receiver::Destructured`]'s reason: its
///   position depends on how many arguments the call wrote, which nothing can know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParameterSlot {
    /// Counted from the left, over required then optional positionals.
    Positional(usize),
    /// Called by name, without its colon, as RBS spells it.
    Keyword(String),
}

/// The thing to the left of the `.` or the `::`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Receiver {
    /// A constant path. The offset is inside its last segment, where the graph files the resolved
    /// reference: `HR::Person.` points into `Person`, not `HR`.
    Constant(u32),
    /// An *instance* of a constant: `Foo.new.`, or a local holding one. The offset means what it
    /// does for `Constant`; only the side of the class differs.
    Instance(u32),
    /// One target of a **multiple assignment**: the `write_io` of `read_io, write_io = IO.pipe`.
    ///
    /// - **Why a position is a receiver shape.** Every other receiver here is one thing with one
    ///   class. `IO.pipe` is declared `-> [IO, IO]`, and which `IO` a name gets depends on *where
    ///   it sits on the left*. So the index travels, and [`types`](super::types) reads the tuple
    ///   element at it.
    /// - **`of` is the whole right-hand side, unresolved**, as [`Returned`](Self::Returned) carries
    ///   its call.
    /// - **A written list never reaches here.** In `a, b = foo, bar` each target gets its own
    ///   element, exactly, with no signature. This variant is for one value spread across several
    ///   names, where only a declaration says how.
    Destructured {
        of: Box<Receiver>,
        /// Which target this is, counted from the left. Only names *before* a `*rest` can be
        /// counted, so the walk refuses a multiple assignment that has one.
        index: u32,
    },
    /// A literal, named by the class Ruby gives it, and for the three generic ones, by what it
    /// holds. Not inference: the parser already decided `"x"` is a `String` and `[1]` an `Array`,
    /// and reading the node kind reads that back.
    Literal {
        class: &'static str,
        /// What the literal holds, by **the position of its class's type parameter**: one for
        /// `Array[E]` and `Range[Elem]`, two for `Hash[K, V]`, none for any other literal.
        ///
        /// - **Why only a literal carries this.** A class does not reveal its type argument:
        ///   `Array[Integer]` and `Array[String]` reach the same declarations, so
        ///   [`types`](super::types) stops at the head. A literal writes its element in the source
        ///   (`[1, 2].each { |n| ... }`), so it is read here, not inferred.
        /// - **`None` at a position** means the contents are not one written class: `[a, b]`,
        ///   `[1, "x"]`, `[]`, and every configuration hash's values. The slot stays so later
        ///   positions line up, as a signature's block half does.
        arguments: Vec<Option<&'static str>>,
    },
    /// A literal `self`, or the implicit one a receiverless call has, **and where it was written**.
    ///
    /// - **This offset is not a graph key.** It is the position whose *enclosing body* decides what
    ///   `self` means, which [`types::method_receiver`](super::types::method_receiver) answers
    ///   against the graph.
    /// - **A `self` is not always read where it is written.** `held = self` above a
    ///   `Class.new(base) do … end` block, and `held.` inside it, are two `self`s: rubydex records
    ///   the block body as an anonymous class. Resolving against the *cursor's* scope would give
    ///   the anonymous class, with none of the instance's members and no name
    ///   [`locator::missed`](super::locator) can print.
    /// - **Usually the identity.** A plain block rebinds nothing, and neither does a `def` inside
    ///   one, so the offset and the cursor land in the same body in every other case.
    SelfObject(u32),
    /// Nothing at all, as in `::Foo`: the receiver is the top-level scope.
    TopLevel,
    /// The value a call returns: what the call was written on, and the name it called.
    ///
    /// - **A shape, not a type.** Only [`types`](super::types) knows `String#upcase` returns a
    ///   `String`, and it needs a graph, on the other side of this module's line. `"x".upcase` is
    ///   recorded as "the result of calling `upcase` on a `String` literal", nothing more.
    /// - **One `Unknown` anywhere ends the chain.** A step that cannot be looked up makes every
    ///   step above it unanswerable.
    Returned {
        on: Box<Receiver>,
        /// The method's name as the source spells it. A name, not an offset: a half-written chain
        /// has no reference the graph recorded, and the name is all this module can read.
        method: String,
        /// The block the call was written with, and what it returns.
        ///
        /// Syntax, and why `"x".bytes.` can be answered: RBS declares `bytes` one way with a block
        /// and another without, and this picks the arm. `&:upcase` and a forwarded `&blk` count,
        /// since Ruby passes a block either way. What the block *returns* rides along for methods
        /// whose answer is that; see [`Block`].
        block: Block,
        /// How many positional arguments the call wrote.
        ///
        /// The block's companion fact: RBS declares `Float#round` one way with a digit count and
        /// another without, and the count picks the arm. Counted here because counting is a
        /// question about text; what it *means* is [`types`](super::types)' question.
        arity: Arity,
        /// The **shape of each positional argument**, in written order.
        ///
        /// - **Why a count is not enough.** `Integer#+: (Integer) -> Integer` and bigdecimal's
        ///   `(BigDecimal) -> BigDecimal` have one arity and two answers. The argument, written
        ///   three characters away, tells them apart.
        /// - **Shapes, unresolved.** [`types::pick_by_argument`](super::types) types them against
        ///   the graph.
        /// - **Empty means *no claim*, never "no arguments".** A call with none is
        ///   `Arity::Exactly(0)` with an empty list, and so is one whose arguments could not be
        ///   read: a splat, a spent width budget, or more arguments than [`MAX_WIDTH`]. The
        ///   consumer requires one shape per counted argument, so an empty list can only cost an
        ///   answer, never invent one. A keyword hash is skipped, as it is not counted.
        arguments: Vec<Receiver>,
    },
    /// A block parameter, typed by what the method the block was passed to says it yields.
    ///
    /// - **The other half of `Returned`**, reading the same declaration from the other end. RBS
    ///   writes `def each: () { (Story) -> void } -> Story::Relation`. Reading only the return
    ///   leaves `Story.where(...).each do |instance|` untyped, and `do |story|` typed only by a
    ///   *guess* from the word, which breaks when the variable is renamed.
    /// - **A shape.** `on` and `method` are the call the block was written on, `index` which block
    ///   parameter this is. What the signature says is [`types`](super::types)' question.
    /// - **No `block` and no `arity` field, on purpose.** A call reaching this *wrote* a block, so
    ///   the block arm applies. What a block is handed depends on the signature, not on the call's
    ///   arity.
    Yielded {
        on: Box<Receiver>,
        method: String,
        index: usize,
    },
    /// An instance variable, typed by an assignment elsewhere in its class.
    ///
    /// Wrapped, not replaced, because *where the type came from* is half the answer: the assignment
    /// may be in another method, twenty lines away, in a branch that never runs. A local is not
    /// wrapped: `person = Person.new` is visible from where the reader stands.
    Assigned {
        /// The offset of the `@name` being assigned, for a card to name the line.
        at: u32,
        was: Box<Receiver>,
    },
    /// A variable whose assignment produced a shape, carrying the name it is written as.
    ///
    /// - **The fall-through: a step, not a sixth rung.** `story = Story.where(...).first` is a
    ///   chain nothing declares. Without this it would answer nothing, while a `story` with no
    ///   assignment reaches the name rung and answers `Story`. Writing the assignment must not make
    ///   the answer worse.
    /// - **The rung order is unchanged.** [`types`](super::types) asks `was` first and reaches
    ///   `name` only when that is empty, so a guess never displaces a resolving chain. `name`
    ///   reaches the same last rung as a bare name, with the same label and the same off switch.
    Spelled {
        was: Box<Receiver>,
        /// The variable as the source spells it, sigils included: exactly what [`Receiver::Named`]
        /// would carry with no assignment to follow.
        name: String,
    },
    /// What `super` returns: the same-named method on whatever is above this one.
    ///
    /// - **A shape like [`Receiver::Returned`], seen from another side.** There the call names its
    ///   receiver; here the receiver is `self` and the name is the enclosing `def`'s. Which
    ///   ancestor declares it needs a graph and Ruby's linearization, so that is
    ///   [`types`](super::types)' question.
    /// - **`arity` and `block` mean what they mean on `Returned`.** `super(a, b)` writes two
    ///   arguments and reaches only two-argument arms. A **bare** `super` forwards whatever the
    ///   caller got, which this file cannot count, so it is [`Arity::Unknown`], like a splat.
    Super {
        /// Where the keyword was written, which decides the rest.
        ///
        /// An offset for [`Receiver::SelfObject`]'s reason: the enclosing body says which class
        /// `super` climbs out of. Reading the cursor's scope instead would answer about a different
        /// method.
        at: u32,
        /// The enclosing `def`'s name, as the source spells it.
        method: String,
        block: bool,
        arity: Arity,
    },
    /// What a `&&` or `||` returns: both operands, and which operator joined them.
    ///
    /// A shape like [`Receiver::Returned`], for a sharper reason. `a && b` **is** `a` or **is**
    /// `b`, decided by whether `a` is falsy. Only `nil` and `false` are, so `a`'s **class**
    /// decides, and classes live on the other side of this module's line. So both operands travel
    /// as nested shapes, and `types::shortcut` evaluates the operator.
    Shortcut {
        left: Box<Receiver>,
        right: Box<Receiver>,
        /// `&&`/`and` against `||`/`or`. One flag, not two variants: they differ only in which side
        /// the falsy case takes.
        and: bool,
    },
    /// What a unary `!` returns: the operand, whose class decides which half.
    ///
    /// - **[`Self::Shortcut`] with one operand.** `!x` is `false` when `x` is truthy and `true`
    ///   when falsy, so `x`'s **class** decides. `types::negated` evaluates it.
    /// - **The third case matters most.** `&&` needs typed operands; `!` needs none, because Ruby
    ///   returns one of the two regardless. So an untypable operand still yields `bool`. That types
    ///   `Object#blank?` (`respond_to?(:empty?) ? !!empty? : false`) and
    ///   `def valid?; !errors.any?; end`.
    /// - **Why not the member.** `!` is a real method and rubydex finds it. But `vendor/rbs`
    ///   declares `TrueClass#!: () -> false`, and an unresolved `bool` carries `TrueClass`, so the
    ///   lookup would answer a confident `false` for an undetermined value. Every `!` in
    ///   `vendor/rbs` returns `true`, `false`, `bool` or `untyped`, so nothing is lost. A class
    ///   redefining `!` to return a non-boolean is deliberately not modelled.
    Negated(Box<Receiver>),
    /// A **method parameter**, typed by the enclosing `def`'s own signature, or else by the default
    /// value written beside it.
    ///
    /// - **Why a parameter is a shape.** `Finder::locals` is filled by local writes and multiple
    ///   writes, and neither sees a parameter. Without this variant a parameter reaches
    ///   [`Receiver::Named`], typed only by its spelling.
    /// - **A shape like [`Receiver::Yielded`], from the third side.** `Yielded` asks what a
    ///   signature hands a method's *block*; this asks what it says the method's own parameters
    ///   are. Both need a graph, so both stop at the name and position.
    /// - **`at`** is [`Receiver::Super`]'s offset, for its reason: the enclosing body decides which
    ///   class this `def` hangs off. **`method`** is the `def`'s name.
    /// - **`default`** is the other rung, carried here so the order is decided in one place: **a
    ///   declared type beats a default**. A signature covers every call; a default only covers a
    ///   call that passed nothing.
    Parameter {
        /// Where the `def` was written; see [`Receiver::Super::at`].
        at: u32,
        /// The enclosing `def`'s name, as the source spells it.
        method: String,
        /// Which parameter of it this is.
        slot: ParameterSlot,
        /// The value written after the `=`, as a shape, if any.
        ///
        /// `None` covers both "no default" and a refused default; see [`Finder::parameter_of`] for
        /// the one refused literal and why.
        default: Option<Box<Receiver>>,
    },
    /// A bare name nothing in this file could type: an instance variable or local with no
    /// assignment worth following, or a receiverless call. As the source spells it, sigils
    /// included.
    ///
    /// - **A name, not a type or a shape.** Two rungs below the graph can still use one: a
    ///   template's instance variables are assigned by the controller its path names (needs a graph
    ///   and another file), and a class-like name is worth a labelled guess. Both are
    ///   [`types`](super::types)' work; this only keeps the name alive.
    /// - **Not a new case for callers.** Anything that treated [`Receiver::Unknown`] as "nothing
    ///   exact" answers the same when those rungs come back empty.
    Named(String),
    /// A return nothing declares, or an expression whose shape says nothing: knowing its type would
    /// take real inference, and there is not even a name left to try.
    Unknown,
}

/// What a call was written with in its block slot, and what that block returns.
///
/// Two questions are asked of this:
///
/// 1. **Was a block written?** That picks the overload arm: `String#bytes` is declared one way with
///    a block and another without.
/// 2. **What does the block return?** RBS declares `map` as `[U] () { (Elem) -> U } -> Array[U]`,
///    so the element is the block's own return type, and only the block's body says what that is.
///
/// - **The exits are shapes**, like everything in this enum: the block's last expression, expanded
///   through a tail-position conditional as for a `def` ([`Exits::tail`]), nothing resolved.
/// - **An empty list and a list holding [`Receiver::Unknown`] differ.** Empty is a block with no
///   body in this file (`&:upcase`, a forwarded `&blk`). A held `Unknown` is a body that *was* read
///   but whose value cannot be named. [`returns_of`] draws the same line for a `def`; dropping
///   unreadable exits is how the rung would lie.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Block {
    /// No block written, in any spelling.
    #[default]
    None,
    /// One written, and the shapes its body hands back.
    Written(Box<[Receiver]>),
}

impl Block {
    /// Whether a block was written at all: the arity-like fact, and all the overload table reads.
    #[must_use]
    pub fn written(&self) -> bool {
        matches!(self, Self::Written(_))
    }

    /// The shapes the block returns; empty where none was written or none could be read.
    #[must_use]
    pub fn exits(&self) -> &[Receiver] {
        match self {
            Self::Written(exits) => exits,
            Self::None => &[],
        }
    }

    /// [`Receiver::rebased`], down the exits.
    ///
    /// **An exit that will not translate becomes [`Receiver::Unknown`] instead of refusing the
    /// whole receiver**, unlike every other arm of that walk. Refusing would take the chain's
    /// *head* down with it, to say nothing about an element. An `Unknown` exit says exactly what is
    /// true: the block's value cannot be named here.
    fn rebased(&self, rebase: &Rebase) -> Option<Self> {
        Some(match self {
            Self::None => Self::None,
            Self::Written(exits) => Self::Written(
                exits
                    .iter()
                    .map(|exit| exit.rebased(rebase).unwrap_or(Receiver::Unknown))
                    .collect(),
            ),
        })
    }
}

/// How many positional arguments a call wrote.
///
/// Positional only: a keyword hash is not counted, and RBS counts the two apart too. An uncountable
/// arity is a value, not an absence, because they answer differently: a call with no arguments
/// reaches only arms that take none, while an uncountable one reaches what every arm agrees on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    /// Exactly this many, every one of them written out.
    Exactly(u32),
    /// A splat or argument forwarding: `foo(*args)` writes a number of arguments nothing can know.
    Unknown,
}

/// How wide a receiver walk may go, on either axis: the chain's *width*.
///
/// **One number for two axes:**
///
/// - **Links.** `Post.where(x).order(y).first.title` is one question per `.`, and each costs the
///   same, so this can be generous. Twenty is past anything hand-written but not past what Rails'
///   query interface produces. Larger values were measured over six corpora and answered nothing
///   more.
/// - **Fan-out.** Typing a local or instance variable visits **every write of that name**, and each
///   write may ask the question again, so unchecked, cost multiplies per level. discourse's
///   `lib/topics_filter.rb` assigns `@scope` sixty times; see the self-referential-write cut in
///   [`Finder::type_the_instance_variable`].
///
/// **Both axes cost about the same, so one number bounds both.** Two exact rules keep it true:
///
/// 1. The assignment loops take candidates newest first and stop at the first that answers, so a
///    write is recursed into only if an older one could still be the answer.
/// 2. [`Finder::memo`] answers each (span, budget) once, turning a count of paths into a product of
///    the two bounds.
///
/// [`Budget`] keeps separate counters because they count different things; only the limit is
/// shared.
const MAX_WIDTH: usize = 20;

/// How far a receiver walk may still go, on each of its two axes.
///
/// Two counters, because the axes grow differently: a deeper search is not a longer chain, and one
/// shared counter made fan-out pay for chain length.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
struct Budget {
    /// Links of a chain followed, bounded by [`MAX_WIDTH`].
    links: usize,
    /// Assignments visited, bounded by [`MAX_WIDTH`].
    fanout: usize,
}

impl Budget {
    /// One more link of a chain: a `.`, a set of parentheses, or the call a block was passed to.
    fn linked(self) -> Self {
        Self {
            links: self.links + 1,
            ..self
        }
    }

    /// One more *branching* step: the walk is about to visit every write of a name.
    fn spread(self) -> Self {
        Self {
            fanout: self.fanout + 1,
            ..self
        }
    }

    /// Whether either axis is spent: the one place both are read together.
    fn spent(self) -> bool {
        self.links >= MAX_WIDTH || self.fanout >= MAX_WIDTH
    }
}

/// The call whose argument list the cursor is inside, and which argument that is.
///
/// What `textDocument/signatureHelp` asks, and deliberately *not* `Context::Argument`'s question.
/// The popup must stay up in two places where completion has nothing to say: a `.` inside the
/// parentheses (`puts(person.`), and a string argument being typed (`puts("hel`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// An offset inside the called method's name, for the caller to resolve; the same convention as
    /// `Context::Argument`.
    pub name: u32,
    pub active: Active,
    /// Whether Ruby allows a **private** method to be written as this call.
    ///
    /// [`Context::allows_private`]'s rule, read from the `CallNode` this finder already holds
    /// rather than a second parse. `signatureHelp` needs it: a signature for a method the
    /// interpreter refuses would disagree with the jump at the same cursor, which
    /// [`locator::precise_call`](super::locator) exists to prevent.
    pub allows_private: bool,
}

/// Which of a method's parameters the cursor is writing an argument for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Active {
    /// The nth argument, from zero: the number of arguments that end before the cursor. A keyword
    /// hash counts as its elements, so `f(1, a: 2, ` is the third parameter, not the second.
    Nth(u32),
    /// A named keyword argument. Keywords may come in any order, so position says nothing:
    /// `f(b: 1, a: ` is `a`, not the second.
    Keyword(String),
    /// A keyword argument not yet named: the cursor is past one keyword and has not started the
    /// next. Which one is unknowable, but *that* it is a keyword is certain, since Ruby forbids a
    /// positional after a keyword. Counting would answer with a parameter this call can no longer
    /// reach.
    AnyKeyword,
}

/// A classified cursor, and the half-typed word it sits at the end of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub context: Context,
    /// The span a completion replaces. Empty when the cursor is not inside a word, as right after
    /// typing `.`.
    pub start: u32,
    pub end: u32,
    /// Whether a **bare word** here is inside a block written straight into a class or module body:
    /// the one place `self` may not be what the file says. See [`closure_in_a_body`] for what that
    /// means and why it is only half the evidence.
    ///
    /// **Computed on the cursor, not where it is needed**, because both readers
    /// ([`locator`](super::locator) for its rung, [`completion`](super::completion) for its list)
    /// already parsed this buffer. Asking separately would parse the file again per keystroke. It
    /// is `false` on any cursor with a written receiver, where the walk would be wasted.
    pub in_a_closure: bool,
}

/// What the cursor at `offset` is completing.
///
/// `None` where Ruby cannot be written: inside a comment, or a string, symbol or regexp literal.
/// Constants in the middle of an error message are worse than nothing, and the server, unlike a
/// client word list, can tell.
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

    // An operator beats the argument list around it: in `foo(bar.` the cursor is in both, and it is
    // completing `bar`'s methods.
    let context = match (&finder.operator, &finder.arguments) {
        (Some(pending), _) => finder.classify(pending),
        (None, Some(call)) => Context::Argument { name: call.name },
        (None, None) => Context::Expression,
    };

    let (start, end) = word_at(source, offset);
    Some(Cursor {
        // Gated on the context, because both readers are in one arm: a written receiver says what
        // `self` is whatever block it sits in, so `person.` and `Foo::` never pay for the walk.
        in_a_closure: matches!(context, Context::Expression | Context::Argument { .. })
            && closure_in_a_body(&result.node(), offset),
        context,
        start,
        end,
    })
}

/// The innermost call whose argument list the cursor is in, and which argument that is.
///
/// - **Unlike [`at`]**, a comment or literal does not end the answer, and an operator inside the
///   parentheses does not take it over. Editors keep the signature up through `puts("hel`,
///   `puts(person.` and a comment between arguments; answering `null` there makes the popup flicker
///   per keystroke.
/// - **`None`** when the cursor is in no argument list, or the call has no name to resolve
///   (`foo.()` is `foo.call()` with none).
#[must_use]
pub fn call_at(source: &str, offset: u32) -> Option<Call> {
    let result = ruby_prism::parse(source.as_bytes());
    let mut finder = Finder::new(source, offset);
    finder.visit(&result.node());
    finder.arguments
}

/// Every assignment to `@name` on an instance of the class written as `path`, in file order.
///
/// The syntax half of the view↔renderer convention, and one of two entry points that read a file
/// the cursor is not in. A template has no enclosing class, so
/// [`Finder::type_the_instance_variable`] has nothing to walk. The assignments typing `@story` are
/// in `StoriesController`, and [`types`](super::types) knows which file that is. Same walk, entered
/// by name.
///
/// - **[`scopes::writes_to`] decides which `@story` counts.** A `def self.` and a `class << self`
///   hold a different variable of the same name, as for a cursor.
/// - **All of them, in order, not just the last shaped one.** "Produced a type" needs a graph: an
///   assignment naming an undeclared class must fall through to the one above, which only the
///   caller can decide.
#[must_use]
pub fn assignments_in(source: &str, path: &str, name: &str) -> Vec<(u32, Receiver)> {
    let writes = scopes::writes_to(source, path, name);
    if writes.is_empty() {
        return Vec::new();
    }
    let result = ruby_prism::parse(source.as_bytes());
    // No cursor in this file: `u32::MAX` is past every offset, so nothing is "being typed" and the
    // walk only collects assignments.
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    writes
        .iter()
        .filter_map(|occurrence| {
            let write = finder
                .instance_writes
                .iter()
                .find(|write| write.name == (occurrence.start, occurrence.end))?;
            let receiver = finder.receiver_of(Some(&write.value), Budget::default());
            (!matches!(receiver, Receiver::Unknown)).then_some((occurrence.start, receiver))
        })
        .collect()
}

/// What the constant whose name is written at `name` was assigned, as a shape.
///
/// The syntax half of the constant-assignment rung, and the other entry point that reads a file the
/// cursor is not in. An application's config constant is typed by `CONFIG = Settings.new` in an
/// initializer, and [`types`](super::types) knows which file.
///
/// - **`name` is a span, not a name**, which makes this exact. rubydex files a
///   `Definition::Constant` under the span of its last segment (`Foo::BAR = x` at the `BAR`), and
///   the caller takes the span from that definition. Same-spelled constants in two namespaces are
///   two spans, and a mere mention is never a recorded span.
/// - **`None`** where the span names no assignment in this text, or the assignment is a shape
///   nothing could come of (as [`assignments_in`] treats it). The first is normal for a buffer
///   edited since indexing: the span no longer names the same bytes, and this refuses rather than
///   reading whatever constant is there now.
#[must_use]
pub fn constant_assignment(source: &str, name: (u32, u32)) -> Option<Receiver> {
    let result = ruby_prism::parse(source.as_bytes());
    // No cursor in this file, as in `assignments_in`: `u32::MAX` is past every offset, so nothing
    // is being typed and the walk only collects assignments.
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    let write = finder
        .constant_writes
        .iter()
        .find(|write| write.name == name)?;
    let receiver = finder.receiver_of(Some(&write.value), Budget::default());
    (!matches!(receiver, Receiver::Unknown)).then_some(receiver)
}

/// The instance variable spanned by `span`, as a shape a type can be looked up from.
///
/// - **[`at`] reaches an instance variable only as the thing left of a `.`.** This asks about the
///   variable itself (the cursor on `@story`, nothing after it) and returns the same [`Receiver`],
///   so cards on `@story` and `@story.title` cannot disagree on the type or its rung.
/// - **`span` is the name span [`scopes`] returns, `@` included.** Which `@foo` this is is not a
///   syntax question (see [`Finder::type_the_instance_variable`]);
///   [`locator::variable_at`](super::locator::variable_at) has already placed the cursor.
#[must_use]
pub fn instance_variable(source: &str, span: (u32, u32)) -> Receiver {
    let result = ruby_prism::parse(source.as_bytes());
    // Nothing is being typed: this answers a hover over settled text, so the half-typed-call repair
    // has nothing to blank. `u32::MAX` is past every offset, as in `assignments_in`.
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    finder.type_the_instance_variable(span, Budget::default())
}

/// Every shape a method body returns, for the rung that reads a body.
///
/// The syntax half of "what does this `def` return" where no signature says; the depth axis is
/// bounded by `types::BODY_HOPS` (see [`types::from_body`](super::types)).
///
/// - **`span` is the whole `def ... end`**: the offset rubydex files a `Definition::Method` under,
///   and a goto's `targetRange`.
/// - **The exits are every `return` in the body, plus the last statement.** A `return` inside a
///   **nested** `def` belongs to that method, so the walk stops at the first nested `def`.
/// - **All of them, not the last.** Whether two exits name one type needs a graph, and the caller
///   has one. Their agreement is what makes the rung safe, as with an overloaded signature's arms.
/// - **A [`Receiver::Unknown`] in the list is an answer, not a gap.** Dropping unreadable exits is
///   how this rung would lie: `is_flaggable?` is a `return false` guard over an `if` whose branches
///   are the real answer, and filtering the `if` leaves a confident wrong `false`. So an unreadable
///   exit comes back `Unknown`, and the caller declines the method.
#[must_use]
pub fn returns_of(source: &str, span: (u32, u32)) -> Vec<Receiver> {
    returns_in(source).remove(&span).unwrap_or_default()
}

/// Every `def` in a document and the shapes its body returns, from **one** parse.
///
/// - **[`returns_of`] asked about every `def` at once.** An `inlayHint` over a file reads the rung
///   once per `def`; asking [`returns_of`] each time would re-parse and re-walk the whole source
///   per method.
/// - **A `def` with no readable exit still gets an entry**, so a caller can tell *read, and none
///   found* from *not read*, which is the difference between declining one method and declining a
///   file.
#[must_use]
pub fn returns_in(source: &str) -> HashMap<(u32, u32), Vec<Receiver>> {
    let result = ruby_prism::parse(source.as_bytes());
    // No cursor in this file, as in `assignments_in`: `u32::MAX` is past every offset, so nothing
    // is being typed.
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    let mut exits = Exits {
        open: Vec::new(),
        found: HashMap::new(),
        lambdas: 0,
    };
    exits.visit(&result.node());
    exits
        .found
        .iter()
        .map(|(span, nodes)| {
            (
                *span,
                nodes
                    .iter()
                    .map(|exit| match exit {
                        Exit::Written(node) => finder.receiver_of(Some(node), Budget::default()),
                        // The class of a `nil` the parser never saw, named as `literal_class` names
                        // one it did: a written `nil` and an unwritten branch are the same value.
                        Exit::Nil => Receiver::literal("NilClass"),
                        Exit::Unknown => Receiver::Unknown,
                    })
                    .collect(),
            )
        })
        .collect()
}

/// One thing a body returns.
///
/// Three kinds, not two, because an unwritten branch returns `nil`: an exit with a class but no
/// node. [`Exit::Unknown`] would decline the method and [`Exit::Written`] has nothing to point at,
/// so the `nil` travels as itself (see [`Exits::tail`]).
enum Exit<'pr> {
    /// An expression, whose type [`Finder::receiver_of`] reads off the node.
    Written(Node<'pr>),
    /// The `nil` Ruby returns where nothing else was written.
    Nil,
    /// An exit this cannot read, kept rather than dropped; see [`returns_of`].
    Unknown,
}

/// The expressions one `def` hands back, found by the span the graph filed it under.
struct Exits<'pr> {
    /// The `def`s the walk is inside, innermost last.
    ///
    /// A stack because [`returns_in`] reads every `def` in a document from one parse. An exit
    /// belongs to the innermost open `def`, so a `return` in a nested `def` is that method's, never
    /// the outer one's.
    open: Vec<(u32, u32)>,
    found: HashMap<(u32, u32), Vec<Exit<'pr>>>,
    /// How many `->` lambdas the walk is inside.
    ///
    /// - **A `return` inside a lambda returns from the lambda**, so filing it as the method's exit
    ///   would decline a method that answers fine. Solidus has
    ///   `def code_column; { header: :code, data: ->(x) do return if x.code.blank?; … end }; end`:
    ///   the method returns a `Hash`, and the `return` never leaves the proc.
    /// - **A counter, not a flag**, because lambdas nest. Zeroed across a `def`, which owns its
    ///   `return`s again.
    /// - **Only `->` is fenced.** `proc`, `Proc.new` and ordinary blocks keep the method's `return`
    ///   (that is the semantic difference). `lambda { … }` is a receiverless call this cannot tell
    ///   from another method named `lambda` without resolving it, so it is left alone: a wrongly
    ///   kept exit only declines a method, the safe direction.
    lambdas: usize,
}

impl<'pr> Visit<'pr> for Exits<'pr> {
    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        let here = (
            node.location().start_offset() as u32,
            node.location().end_offset() as u32,
        );
        // Opened before the body is read, so every exit in the body lands on this `def`, not on the
        // one around it.
        self.found.entry(here).or_default();
        self.open.push(here);
        // A `def` inside a lambda owns its `return`s, so the fence is lifted for this body and
        // restored after.
        let fenced = std::mem::take(&mut self.lambdas);
        // The body's exits. An endless `def` is a `StatementsNode` like any other; a `def` with
        // `rescue`, `else` or `ensure` is a `BeginNode`, handled by [`Self::rescued`].
        match node.body() {
            Some(body) => match body.as_begin_node() {
                Some(found) => self.rescued(&found, 0),
                None => self.statements(body.as_statements_node().as_ref(), 0),
            },
            // **An unwritten body is an unwritten branch.** `def edit; end` returns `nil` as
            // `if x then end` does, and is filed the same way (see [`Self::branch`]). So a Rails
            // action whose template does the work answers `nil`, as Ruby does.
            None => self.push(Exit::Nil),
        }
        ruby_prism::visit_def_node(self, node);
        self.lambdas = fenced;
        self.open.pop();
    }

    fn visit_lambda_node(&mut self, node: &ruby_prism::LambdaNode<'pr>) {
        // Everything inside is the lambda's exit, not this method's; see [`Exits::lambdas`]. The
        // lambda itself is still a value, so a `def` ending in one reaches [`Exits::tail`] and
        // answers `Proc`. This fences the `return` walk, not the tail read.
        self.lambdas += 1;
        ruby_prism::visit_lambda_node(self, node);
        self.lambdas -= 1;
    }

    fn visit_return_node(&mut self, node: &ruby_prism::ReturnNode<'pr>) {
        // - **`return a, b` is an `Array`**, but saying so needs a node this module has none of, so
        //   it is an unreadable exit, not the first value.
        // - **A bare `return` is an unwritten branch in another spelling.**
        //   `return if query.blank?` over an `each` returns `nil` on the guard's path, so it files
        //   a `nil`, as [`Exits::branch`] does for a missing `else`.
        self.push(match node.arguments() {
            None => Exit::Nil,
            Some(written) => match exactly_one(&written) {
                Some(one) => Exit::Written(one),
                None => Exit::Unknown,
            },
        });
        ruby_prism::visit_return_node(self, node);
    }
}

/// How many conditionals deep an exit may be expanded.
///
/// A bound, not a budget, on a third axis ([`MAX_WIDTH`]'s argument): an `if` inside an `if` inside
/// a `case` is ordinary; a whole file of them is not worth walking on a keystroke.
const MAX_BRANCHES: usize = 4;

impl<'pr> Exits<'pr> {
    /// One exit, filed under the innermost open `def`.
    ///
    /// A `return` outside every `def` (a file's top level, a `class` body) is not a method's exit
    /// and is dropped, not filed against the last `def` passed.
    fn push(&mut self, exit: Exit<'pr>) {
        if self.lambdas > 0 {
            return;
        }
        if let Some(open) = self.open.last() {
            self.found.entry(*open).or_default().push(exit);
        }
    }

    /// One tail-position statement, expanded into the exits it really is.
    ///
    /// - **A conditional in tail position is one exit per branch.** That is the difference between
    ///   `is_flaggable?` declining and wrongly answering `false`.
    /// - **An unwritten branch is an exit whose value is `nil`**, and filing it keeps this rung
    ///   honest. `def find_user; if found? then User.first end; end` returns `nil` on the common
    ///   path; skipping the missing `else` would draw `-> User`. The `nil` exit disagrees with the
    ///   written branch, so the method declines and the margin stays silent.
    /// - **Where every branch is missing**, `NilClass` is the answer.
    fn tail(&mut self, node: Node<'pr>, depth: usize) {
        if depth >= MAX_BRANCHES {
            self.push(Exit::Unknown);
            return;
        }
        if let Some(found) = node.as_if_node() {
            self.branch(found.statements().as_ref(), depth);
            match found.subsequent() {
                Some(otherwise) => self.tail(otherwise, depth + 1),
                // No `else`, including the modifier form (`x if y` and `if y then x end` are one
                // node). Ruby returns `nil` when the condition is false. A branchless conditional
                // is one of the two shapes that say so; a bare `return` is the other, filed by
                // [`Visit::visit_return_node`].
                None => self.push(Exit::Nil),
            }
            return;
        }
        if let Some(found) = node.as_unless_node() {
            self.branch(found.statements().as_ref(), depth);
            self.branch(
                found
                    .else_clause()
                    .and_then(|otherwise| otherwise.statements())
                    .as_ref(),
                depth,
            );
            return;
        }
        if let Some(found) = node.as_case_node() {
            for condition in found.conditions().iter() {
                match condition.as_when_node() {
                    Some(when) => self.branch(when.statements().as_ref(), depth),
                    // `in`: a pattern-match arm, which `as_when_node` does not answer for.
                    None => self.push(Exit::Unknown),
                }
            }
            self.branch(
                found
                    .else_clause()
                    .and_then(|otherwise| otherwise.statements())
                    .as_ref(),
                depth,
            );
            return;
        }
        if let Some(found) = node.as_else_node() {
            self.branch(found.statements().as_ref(), depth);
            return;
        }
        // A `begin`/`rescue` as the last statement has the same shape as one on the `def` itself,
        // and is read the same way instead of pushed whole as an unreadable exit.
        if let Some(found) = node.as_begin_node() {
            self.rescued(&found, depth + 1);
            return;
        }
        // A tail-position `return` is **already** this method's exit: [`Visit::visit_return_node`]
        // pushes its value. Pushing the `return` node too would add an unreadable exit, and the
        // agreement rule would decline `def title; return "x"; end` while `def title; "x"; end`
        // answers `String`.
        if node.as_return_node().is_some() {
            return;
        }
        self.push(Exit::Written(node));
    }

    /// Every exit of a `begin`/`rescue`/`else`/`ensure`, which is what a `def` with a `rescue`
    /// clause has for a body.
    ///
    /// 1. **The statements** return their last value.
    /// 2. **An `else`** *replaces* that value when nothing was raised, so only one of the two is
    ///    read.
    /// 3. **Each `rescue`** returns its own last value; a chain of them is a chain of exits.
    /// 4. **An `ensure`** runs for effect and never decides the return, so it is not read:
    ///    `def f; A; ensure; log; end` must not answer with `log`.
    ///
    /// This usually declines, and that is right: a `rescue` ending in `errors.add` and a body
    /// ending in a comparison name two classes, a union. Without reading the rescue exits and the
    /// tail, a method would be answered from whatever `return` guards it happened to contain.
    fn rescued(&mut self, node: &ruby_prism::BeginNode<'pr>, depth: usize) {
        match node.else_clause() {
            // `begin A rescue B else C end` returns `C` when nothing was raised; `A`'s value is
            // discarded, so reading both would invent an exit.
            Some(otherwise) => self.branch(otherwise.statements().as_ref(), depth),
            None => self.statements(node.statements().as_ref(), depth),
        }
        let mut rescued = node.rescue_clause();
        while let Some(clause) = rescued {
            self.branch(clause.statements().as_ref(), depth);
            rescued = clause.subsequent();
        }
    }

    /// A statement list's last statement, expanded, at the same depth.
    ///
    /// [`Self::branch`] is this plus one step of depth: a *branch* costs depth, a body does not. A
    /// one-statement `def` has taken no conditional.
    fn statements(&mut self, statements: Option<&StatementsNode<'pr>>, depth: usize) {
        if let Some(last) = statements.and_then(|found| found.body().iter().last()) {
            self.tail(last, depth);
        }
    }

    /// One branch's last statement, expanded in turn.
    ///
    /// **An unwritten or empty branch is the `nil` exit.** `unless` and `case` reach their missing
    /// `else` through here, and `if x then end` returns `nil` as `if x then nil end` does.
    fn branch(&mut self, statements: Option<&StatementsNode<'pr>>, depth: usize) {
        match statements.and_then(|found| found.body().iter().last()) {
            Some(last) => self.tail(last, depth + 1),
            None => self.push(Exit::Nil),
        }
    }
}

/// Whether a block's body holds a `next` or `break`: the two exits [`Exits`] does not see.
///
/// **This block's own only; the fence keeps the walk linear.** A `next` in a nested block is that
/// block's `return`, and a `break` in one abandons what *it* was passed to, so neither is this
/// block's exit. Descending into them would make every enclosing block re-walk the same subtree: in
/// a file of `describe do … it do … end … end`, the whole file once per level.
fn escapes(block: &BlockNode<'_>) -> bool {
    let Some(body) = block.body() else {
        return false;
    };
    let mut walk = Escapes(false);
    walk.visit(&body);
    walk.0
}

/// [`escapes`]' walk. Once the flag is set the walk stops descending (nothing more to learn), and
/// the three constructs that own their `next` and `break` are never entered.
struct Escapes(bool);

impl<'pr> Visit<'pr> for Escapes {
    fn visit_next_node(&mut self, _: &ruby_prism::NextNode<'pr>) {
        self.0 = true;
    }

    fn visit_break_node(&mut self, _: &ruby_prism::BreakNode<'pr>) {
        self.0 = true;
    }

    fn visit_block_node(&mut self, _: &BlockNode<'pr>) {}

    fn visit_lambda_node(&mut self, _: &ruby_prism::LambdaNode<'pr>) {}

    fn visit_def_node(&mut self, _: &DefNode<'pr>) {}
}

/// The one value a `return` hands back, or `None` where it hands back several.
fn exactly_one<'pr>(written: &ruby_prism::ArgumentsNode<'pr>) -> Option<Node<'pr>> {
    let mut arguments = written.arguments().iter();
    let first = arguments.next()?;
    arguments.next().is_none().then_some(first)
}

/// What introduced a name whose type its line does not spell out.
///
/// The two shapes a *binding* can have: the question an inlay hint asks, and no other caller.
/// Everything else here starts at a cursor and asks what one name means; this starts at the file
/// and asks which names have a type worth saying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    /// A block's parameter: the `story` in `stories.each do |story|`.
    BlockParameter,
    /// A local being assigned: the `author` in `author = story.author`.
    Local,
}

/// One binding, and the shape that would type it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    pub binding: Binding,
    /// The span of the name, which is what a label is drawn after.
    pub name: (u32, u32),
    /// What it was bound to, as a shape. [`types::method_receiver`](super::types::method_receiver)
    /// turns it into a type; a **shape** is all that can be said without a graph.
    pub was: Receiver,
}

/// Every binding whose name starts inside `within`, in source order.
///
/// The third cursor-less walk, like [`assignments_in`] and [`instance_variable`], and the one that
/// reports rather than resolves: `u32::MAX` again means the whole file is collected.
///
/// - **`within` is not an afterwards filter.** Classifying a shape walks a chain and every
///   assignment above it, and the caller asks about the range on screen. Applying the range here
///   stops classification from running at all; in the caller it would only discard answers.
///   Collecting candidates is one parse either way.
/// - **It tests overlap, not containment, and an empty range is legal.** A name half scrolled off
///   the top is still a binding the visible half wants labelled, and a caller asking about one
///   label passes both ends equal.
/// - **An unshapeable binding is dropped**, not reported as [`Receiver::Unknown`], as in
///   [`assignments_in`].
#[must_use]
pub fn bindings_in(source: &str, within: (u32, u32)) -> Vec<Bound> {
    let result = ruby_prism::parse(source.as_bytes());
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    finder.bindings(within)
}

impl Finder<'_, '_> {
    /// [`bindings_in`]'s answer, taken from a walk that has already happened.
    ///
    /// Split out for [`type_of`], which asks this and a second question of the same tree: a cursor
    /// is on a binding or on a read, and parsing twice to learn which would double the request's
    /// cost.
    fn bindings(&self, within: (u32, u32)) -> Vec<Bound> {
        let finder = self;
        let mut bound: Vec<Bound> = finder
            .yielded
            .iter()
            .filter(|parameter| overlaps(parameter.name, within))
            .filter_map(|parameter| {
                #[cfg(test)]
                classified_one();
                Some(Bound {
                    binding: Binding::BlockParameter,
                    name: parameter.name,
                    was: finder.yielded_shape(parameter, Budget::default())?,
                })
            })
            .chain(
                finder
                    .locals
                    .iter()
                    .filter(|write| overlaps(write.name, within))
                    .filter_map(|write| {
                        #[cfg(test)]
                        classified_one();
                        let was = finder.receiver_of(Some(&write.value), Budget::default());
                        // Refused **before** the wrap, for the reason [`LocalWrite::index`]'s other
                        // reader gives: a destructure of something unanswerable is still
                        // unanswerable, not an index into nothing.
                        if matches!(was, Receiver::Unknown) {
                            return None;
                        }
                        // A multiple-assignment target holds the `index`th element of the value,
                        // not the value, and **this is the second place that must say so.** Without
                        // the wrap the question becomes what the whole call returned, drawn on
                        // every name left of the `=` (`blobs, actions, error = prepare(...)` would
                        // draw `: Array` three times). `locator` applies the same wrap, so the card
                        // and the margin agree.
                        let was = match write.index {
                            Some(index) => Receiver::Destructured {
                                of: Box::new(was),
                                index,
                            },
                            None => was,
                        };
                        Some(Bound {
                            binding: Binding::Local,
                            name: write.name,
                            was,
                        })
                    }),
            )
            .collect();
        bound.sort_by_key(|found| found.name);
        bound
    }
}

/// What the cursor stands on, as the shape whose **type** is the answer.
///
/// [`at`] asks what is being *written* and [`bindings_in`] what a file binds. This asks
/// `textDocument/typeDefinition`'s question: not where the name is declared, but where its value's
/// class is. The answer is a [`Receiver`], resolved by
/// [`types::method_receiver`](super::types::method_receiver), so the jump, the card and the margin
/// share one classification.
///
/// Two roads in:
///
/// 1. **A binding** (the `story` of `story = Story.first` or `stories.each do |story|`) is
///    [`bindings_in`] asked with both range ends equal, the same call the margin makes, so a jump
///    from a name and its label cannot disagree. It is also the only road carrying a destructured
///    target's position: the write's value ends after the cursor, so the walk below cannot see it.
/// 2. **A read** (a local, a call, a constant) is the innermost node around the cursor, passed to
///    [`Finder::receiver_of`]. For a call the cursor must be in the **message name**:
///    `story.author` has three cursors with three answers, and only the one on `author` asks what
///    `author` returns.
///
/// - **Instance variables are not here, on purpose.** `locator::resolve_variable` answers them
///   first, as `definition` asks the scope walk before the graph (see `navigation.md`). Handling
///   `@story` here too could classify it twice, differently.
/// - **A parameter is not answered at its declaration.** The margin does not label `def f(story)`
///   either ([`Binding`] has no `def`-header variant). A *use* inside the body is an ordinary read
///   and reaches [`Finder::parameter_of`].
/// - **`u32::MAX` means settled text.** `typeDefinition` is not one of the three requests answered
///   between a keystroke and the index, so there is no half-typed call to blank.
///   [`instance_variable`] and [`returns_in`] do the same.
#[must_use]
pub fn type_of(source: &str, offset: u32) -> Option<(u32, u32, Receiver)> {
    let result = ruby_prism::parse(source.as_bytes());
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());

    // The binding first. Containment is tested here, not with `overlaps`: touching a name at one
    // end is what a *window* means, while a cursor must be inside it.
    if let Some(bound) = finder
        .bindings((offset, offset))
        .into_iter()
        .find(|bound| bound.name.0 <= offset && offset <= bound.name.1)
    {
        return Some((bound.name.0, bound.name.1, bound.was));
    }

    let mut pointed = Pointed {
        offset,
        found: None,
    };
    pointed.visit(&result.node());
    let (start, end, node) = pointed.found?;
    let receiver = finder.receiver_of(Some(&node), Budget::default());
    // The same refusal as every entry point here: an unshapeable shape is no answer, and saying so
    // lets the caller tell it from a type it declined.
    (!matches!(receiver, Receiver::Unknown)).then_some((start, end, receiver))
}

/// The walk [`type_of`] runs: the innermost read around the cursor.
///
/// - **Recorded on the way in, so the deepest node visited stands**, as in [`BodyClosure`]. Prism
///   walks pre-order, a node not containing the cursor records nothing, and a node that does
///   contains every later candidate. Siblings cannot both match.
/// - **Only the three shapes a *read* can have are recorded.** A string literal is a `String`, but
///   a jump to it would be true and unasked for. The tests below pin the set.
struct Pointed<'pr> {
    offset: u32,
    /// The span an editor underlines, and the node whose shape answers.
    found: Option<(u32, u32, Node<'pr>)>,
}

impl<'pr> Pointed<'pr> {
    /// Whether the cursor is inside `location`, ends included.
    ///
    /// Both ends, because editors put the cursor *after* a word's last character as readily as
    /// inside it, and the answer should not depend on which.
    fn holds(&self, location: &Location<'_>) -> bool {
        location.start_offset() as u32 <= self.offset && self.offset <= location.end_offset() as u32
    }

    fn record(&mut self, location: &Location<'_>, node: Node<'pr>) {
        if self.holds(location) {
            self.found = Some((
                location.start_offset() as u32,
                location.end_offset() as u32,
                node,
            ));
        }
    }
}

impl<'pr> Visit<'pr> for Pointed<'pr> {
    /// A call, where the cursor must be on the **name it calls**.
    ///
    /// `story.author` has three cursors: `story` is the local, `author` is this, and the `.` is
    /// neither. Only the message span is recorded, so a cursor on the receiver falls through to the
    /// receiver's node.
    ///
    /// Operator calls (`a + b`, `list[0]`) have a message too and are deliberately included: what
    /// `+` returns is a type like any other, from the same signature.
    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        if let Some(message) = node.message_loc() {
            self.record(&message, node.as_node());
        }
        ruby_prism::visit_call_node(self, node);
    }

    fn visit_local_variable_read_node(&mut self, node: &LocalVariableReadNode<'pr>) {
        self.record(&node.location(), node.as_node());
        ruby_prism::visit_local_variable_read_node(self, node);
    }

    /// `it`: a read of a local the block declares, reaching the same rungs.
    fn visit_it_local_variable_read_node(&mut self, node: &ItLocalVariableReadNode<'pr>) {
        self.record(&node.location(), node.as_node());
        ruby_prism::visit_it_local_variable_read_node(self, node);
    }

    fn visit_constant_read_node(&mut self, node: &ConstantReadNode<'pr>) {
        self.record(&node.location(), node.as_node());
        ruby_prism::visit_constant_read_node(self, node);
    }

    /// `HR::Person`, recorded whole, then refined by its own parent.
    ///
    /// A cursor on `Person` is this node, resolved as a whole path. A cursor on `HR` is the inner
    /// [`ConstantReadNode`], visited after this one, so it stands: the innermost rule at work, not
    /// a special case.
    fn visit_constant_path_node(&mut self, node: &ConstantPathNode<'pr>) {
        self.record(&node.location(), node.as_node());
        ruby_prism::visit_constant_path_node(self, node);
    }
}

/// Whether two spans share any byte, or touch at an end. See [`bindings_in`].
#[must_use]
pub const fn overlaps(span: (u32, u32), within: (u32, u32)) -> bool {
    span.0 <= within.1 && within.0 <= span.1
}

// How many of [`bindings_in`]'s candidates have been classified since a test last asked.
//
// - **Why a counter.** `within` claims a binding outside the window is **never classified**, and no
//   answer can show that: classifying everything and filtering later returns the same list. Only
//   the work differs, and timing it is flaky on a shared runner. A count is the same claim as a
//   stable integer.
// - **`thread_local`**, for [`REQUESTS_TO_CRASH`](super::REQUESTS_TO_CRASH)'s reason: a parallel
//   test must not count into another's total. `bindings_in` runs on the requesting thread (one
//   request handler, never an indexer worker), so nothing is missed.
#[cfg(test)]
thread_local! {
    static CLASSIFIED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn classified_one() {
    CLASSIFIED.set(CLASSIFIED.get() + 1);
}

/// How many candidates were classified since the last call, and resets it.
///
/// Read-and-reset in one call, so a test cannot read a number another request left behind.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn classifications_taken() -> usize {
    CLASSIFIED.replace(0)
}

/// A symbol literal written as a macro's argument, and the macro that wrote it.
///
/// `before_action :authenticate`, `validates :title`, `belongs_to :user`. rubydex records the
/// *call*, not its arguments (as with [`requires`](super::requires)), so the graph cannot see the
/// symbol. It is the second commonest thing in a Rails file to put a cursor on that resolves to
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroSymbol {
    /// The name, colon excluded: `authenticate`.
    pub name: String,
    /// The call it is an argument to: `before_action`.
    pub macro_name: String,
    /// The span of the name, colon excluded: what the editor underlines.
    pub start: u32,
    pub end: u32,
}

/// The macro argument `offset` is inside, if any.
///
/// 1. **A macro is a receiverless call written straight into a class or module body.** That is what
///    a macro *is* in Ruby, and it is pure syntax. So this module learns no vocabulary:
///    `workspace/rails/` stays the only place that knows `belongs_to`, and a gem's own
///    `acts_as_list :position` reads the same way.
/// 2. **Positional arguments only.** The first `:symbol` is a name the macro is *about*. A symbol
///    in a keyword argument *configures* it and usually means something else:
///    `dependent: :destroy`, `on: :create`, `inclusion: { in: [:draft, :live] }`. `delegate`'s
///    `to: :user` would resolve, but telling it from `dependent:` needs Rails knowledge this module
///    lacks, so it is left out too.
/// 3. **Nothing nested.** `scope :recent, -> { order(created_at: :desc) }` offers `:recent`, not
///    `:desc`; `enum :status, [:draft, :live]` offers `:status`, not its values.
#[must_use]
pub fn macro_symbol(source: &str, offset: u32) -> Option<MacroSymbol> {
    let result = ruby_prism::parse(source.as_bytes());
    let mut finder = MacroSymbols {
        offset,
        body: false,
        found: None,
    };
    finder.visit(&result.node());
    finder.found
}

/// The walk [`macro_symbol`] runs: where `self` is a class body, and which call is under the
/// cursor.
struct MacroSymbols {
    offset: u32,
    /// Whether the visited node is in a class or module body rather than inside a `def`. A block
    /// does not change this (`included do … end` and `scope :recent, -> {}` are still the body), so
    /// it follows `def`, not every Prism nesting.
    body: bool,
    found: Option<MacroSymbol>,
}

impl<'pr> Visit<'pr> for MacroSymbols {
    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        let held = std::mem::replace(&mut self.body, true);
        ruby_prism::visit_class_node(self, node);
        self.body = held;
    }

    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        let held = std::mem::replace(&mut self.body, true);
        ruby_prism::visit_module_node(self, node);
        self.body = held;
    }

    fn visit_singleton_class_node(&mut self, node: &SingletonClassNode<'pr>) {
        let held = std::mem::replace(&mut self.body, true);
        ruby_prism::visit_singleton_class_node(self, node);
        self.body = held;
    }

    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        let held = std::mem::replace(&mut self.body, false);
        ruby_prism::visit_def_node(self, node);
        self.body = held;
    }

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        if self.found.is_none() {
            self.found = self.argument_at(node);
        }
        if self.found.is_none() {
            ruby_prism::visit_call_node(self, node);
        }
    }
}

impl MacroSymbols {
    fn argument_at(&self, node: &CallNode<'_>) -> Option<MacroSymbol> {
        // `Foo.validates :title` is someone's own method on someone's own object, and a call inside
        // a `def` is a call, not a declaration.
        if !self.body || node.receiver().is_some() {
            return None;
        }
        for argument in node.arguments()?.arguments().iter() {
            let Some(symbol) = argument.as_symbol_node() else {
                continue;
            };
            // The hit test uses the whole literal, colon included, so a cursor on the `:` counts.
            // The reported span is the name: what an editor underlines, and what generators record
            // as a declaration's place.
            let literal = symbol.location();
            if self.offset < literal.start_offset() as u32
                || self.offset > literal.end_offset() as u32
            {
                continue;
            }
            // A dynamic symbol (`:"#{prefix}_id"`) has no static value to look up.
            let value = symbol.value_loc()?;
            return Some(MacroSymbol {
                name: String::from_utf8_lossy(symbol.unescaped()).into_owned(),
                macro_name: String::from_utf8_lossy(node.name().as_slice()).into_owned(),
                start: value.start_offset() as u32,
                end: value.end_offset() as u32,
            });
        }
        None
    }
}

/// Whether `offset` is inside a **block written straight into a class or module body**.
///
/// - **The one place `self` may not be what the file says.** Elsewhere the enclosing construct
///   decides `self`, and the graph records it. A block is a *value*, and whoever receives it may
///   run it against anything: `rule(:colon) { str(':') }`, `scope :recent, -> { where(...) }`,
///   `validates :x, if: -> { active? }`. A name there may be on the class object or on an instance.
/// - **This answers only the syntax half:** is there a block between the cursor and its namespace
///   body? The graph supplies the other half. [`locator`](super::locator) checks one name (absent
///   from the class object, present on an instance), and [`completion`](super::completion) applies
///   the same rule to a whole list.
/// - **A `def` ends the question, and a block inside one never starts it.** In
///   `def self.run; [1].each { … }; end`, `self` is the class object whatever `each` does, because
///   the block closes over the method's `self`. [`MacroSymbols::body`] follows the same rule.
/// - **A second walk, not a second parse.** This needs different state from [`Finder`] (a stack of
///   what fixes `self`, not of what the cursor is inside), so one visitor would carry both sets of
///   fields. Over the same tree it is one extra walk; a second parse would read the file twice per
///   keystroke.
#[must_use]
fn closure_in_a_body(node: &Node<'_>, offset: u32) -> bool {
    let mut walk = BodyClosure {
        offset,
        body: false,
        block: false,
        found: false,
    };
    walk.visit(node);
    walk.found
}

/// The walk [`closure_in_a_body`] runs.
///
/// Both flags describe *the visited node*. The answer is read at the innermost construct that
/// contains the cursor, recorded on the way in so the deepest one visited stands. A construct not
/// containing the cursor records nothing, and neither can anything inside it.
struct BodyClosure {
    offset: u32,
    /// Whether this is a class or module body rather than a `def`'s inside:
    /// [`MacroSymbols::body`]'s question, asked at a cursor instead of a call.
    body: bool,
    /// Whether a block or a lambda has been entered since that body began.
    block: bool,
    found: bool,
}

impl BodyClosure {
    /// Enter a construct that fixes what `self` is: a namespace body, or a `def`.
    ///
    /// It closes any block above it: a block *containing* a `class` keyword is not between the
    /// cursor and the cursor's body.
    fn fixes_self(&mut self, location: &Location<'_>, body: bool) -> (bool, bool) {
        let held = (self.body, self.block);
        self.body = body;
        self.block = false;
        self.record(location);
        held
    }

    fn record(&mut self, location: &Location<'_>) {
        if location.start_offset() as u32 <= self.offset
            && self.offset <= location.end_offset() as u32
        {
            self.found = self.body && self.block;
        }
    }
}

impl<'pr> Visit<'pr> for BodyClosure {
    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        let held = self.fixes_self(&node.location(), true);
        ruby_prism::visit_class_node(self, node);
        (self.body, self.block) = held;
    }

    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        let held = self.fixes_self(&node.location(), true);
        ruby_prism::visit_module_node(self, node);
        (self.body, self.block) = held;
    }

    fn visit_singleton_class_node(&mut self, node: &SingletonClassNode<'pr>) {
        let held = self.fixes_self(&node.location(), true);
        ruby_prism::visit_singleton_class_node(self, node);
        (self.body, self.block) = held;
    }

    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        let held = self.fixes_self(&node.location(), false);
        ruby_prism::visit_def_node(self, node);
        (self.body, self.block) = held;
    }

    fn visit_block_node(&mut self, node: &BlockNode<'pr>) {
        let held = std::mem::replace(&mut self.block, true);
        self.record(&node.location());
        ruby_prism::visit_block_node(self, node);
        self.block = held;
    }

    // `-> { }` and `lambda { }` are one construct to Ruby but different Prism nodes, and a Rails
    // model writes `scope :recent, -> { … }`.
    fn visit_lambda_node(&mut self, node: &LambdaNode<'pr>) {
        let held = std::mem::replace(&mut self.block, true);
        self.record(&node.location());
        ruby_prism::visit_lambda_node(self, node);
        self.block = held;
    }
}

/// The half-typed word the cursor is at the end of, as a span.
///
/// Ruby names are `[A-Za-z0-9_]` with three complications, each changing what is replaced: a
/// leading `@`, `@@` or `$` is part of the name, and so is a method's trailing `?` or `!`. Missing
/// the last turns accepting `empty?` into `empty?empty?`.
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

/// Ruby names are ASCII word characters plus anything non-ASCII (`имя` is a legal local). A run of
/// continuation bytes is always whole characters, so this stays on a boundary.
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

fn in_comment(result: &ParseResult<'_>, offset: u32) -> bool {
    result.comments().any(|comment| {
        let location = comment.location();
        // Inclusive of the end: a comment's span stops at the line's last character, and a cursor
        // past it is still in the comment.
        location.start_offset() as u32 <= offset && offset <= location.end_offset() as u32
    })
}

/// What the branching arms answered, keyed by the question: a span and the budget the walk arrived
/// with. See [`Finder::memo`].
type Answered = HashMap<((u32, u32), Budget), Receiver>;

struct Finder<'s, 'pr> {
    offset: u32,
    source: &'s str,
    /// Set when the cursor is inside a literal with no code in it.
    in_literal: bool,
    /// The innermost `::` or `.` the cursor is completing after, not yet classified.
    ///
    /// The *node*, not the answer, because classifying a receiver may need an assignment the walk
    /// has not reached yet (`x` typed from `x = Foo.new`, or a chain through a local). So
    /// classification waits until every assignment is in. Holding one node also keeps "one
    /// `Unknown` ends the chain" in a single place.
    operator: Option<Pending<'pr>>,
    /// The innermost call whose argument list holds the cursor.
    arguments: Option<Call>,
    /// Every completed assignment to a local that ends before the cursor.
    locals: Vec<LocalWrite<'pr>>,
    /// Every block parameter in the file, with the call its block was written on.
    ///
    /// Collected with the writes, for the same reason: the question is about a span, and answering
    /// it needs the whole file, not just the node under the cursor.
    yielded: Vec<BlockParameter<'pr>>,
    /// Every instance-variable assignment in the file, keyed by the span of the `@name` it writes:
    /// the span [`scopes`] reports occurrences under, and the one thing the two walks must agree
    /// on.
    instance_writes: Vec<InstanceWrite<'pr>>,
    /// Every constant assignment in the file, keyed by the span of the name it writes.
    ///
    /// The span is the **last segment alone** (the `BAR` of `Foo::BAR = x`), because rubydex files
    /// its `Definition::Constant` there, and the span is all the graph and this walk must agree on.
    /// See [`constant_assignment`].
    constant_writes: Vec<ConstantWrite<'pr>>,
    /// Every `def` in the file (span, name, parameters), in walk order.
    ///
    /// Two arms ask it *which `def` am I in*: a `super` node carries no name (Ruby takes it from
    /// the enclosing method), and a bare local read has no binding when its parameter is in the
    /// header. The walk is pre-order, so the **last** entry whose span contains the offset is the
    /// innermost, the rule [`Exits::open`] states with a stack.
    defs: Vec<Def<'pr>>,
    /// The half-typed operator the cursor is completing after, as a span to blank out.
    ///
    /// See [`Finder::without_the_half_typed_call`]. Recorded here because its call node is gone by
    /// the time the question is asked.
    repair: Option<(u32, u32)>,
    /// What a span already answered, under one budget.
    ///
    /// - **Only the two branching arms are kept**, because only they get re-solved: every write of
    ///   a name is a candidate for every read, so the same question comes back under each sibling.
    ///   Answering each once turns an exponent into a product of the two bounds.
    /// - **Keyed by span, not node**: in one document a span holds one token, so two reads cannot
    ///   start at the same offset.
    /// - **Keyed by budget too**, because the answer depends on it: a walk with less budget left
    ///   stops sooner, and giving its answer to a walk with more would cost labels, not just time.
    /// - **A `RefCell`** because [`Finder::receiver_of`] takes `&self` all the way down and the
    ///   recursion is what is memoised. Borrows are taken and dropped *around* the recursive call,
    ///   never across it (see [`Finder::memoised`]).
    /// - **Unbounded on purpose.** It holds one entry per question actually asked, the questions
    ///   are bounded by the two counters, and the map dies with the [`Finder`] (one parse, one
    ///   request). Measured on discourse's heaviest files, it stays around a hundred entries.
    memo: RefCell<Answered>,
}

/// One `@x = <something>`.
///
/// Collected from the whole file, not just before the cursor, unlike a local's: an instance
/// variable is assigned in `initialize` and read in every other method, half of them written above
/// it.
struct InstanceWrite<'pr> {
    name: (u32, u32),
    /// The span of the assigned value, so a write cannot answer for a read inside itself.
    value_span: (u32, u32),
    value: Node<'pr>,
}

/// One `CONST = <something>`.
///
/// No `value_span`, unlike the instance variable above: that field stops a *cursor* inside an
/// assignment from being answered by it, and this is only ever read from a file the cursor is not
/// in.
struct ConstantWrite<'pr> {
    name: (u32, u32),
    value: Node<'pr>,
}

/// An operator the cursor is completing after, before its receiver has been looked at.
enum Pending<'pr> {
    /// After a `.` or `&.`. `None` where no receiver was written: a call on an implicit `self`,
    /// which this module cannot name.
    MethodCall(Option<Node<'pr>>),
    /// After a `::`. `None` is the leading-`::` form, where a missing receiver means something
    /// specific, not unknown.
    NamespaceAccess(Option<Node<'pr>>),
}

/// Whether this shape is a **parameter** relayed into a name, directly or through an assignment: a
/// block's, or a `def`'s own.
///
/// The first precedence question the assignment loops ask. The shape is not wrong; it is weak.
///
/// - **A block parameter.** `Receiver::Yielded` answers only where the callee's signature says what
///   its block receives. Otherwise (every call on an untypable receiver) it falls through
///   `Receiver::Spelled` to the name, like a bare local read. So such a write must not displace a
///   write that produced a real type. `Spelled` is unwrapped because a local read is always wrapped
///   in one.
/// - **A `def`'s parameter without a default.** A [`Receiver::Parameter`] answers only where
///   something *declares* its type, which application code rarely does, so it is the same kind of
///   fallback. solidus assigns `preference_store_class = Spree::Config` in one branch and
///   `= prefs_or_conf_class` in the next; trusting the second would lose the first's correct
///   answer.
/// - **A parameter with a default is not here**: the default is a real shape.
fn relays_a_parameter(receiver: &Receiver) -> bool {
    match receiver {
        Receiver::Yielded { .. } | Receiver::Parameter { default: None, .. } => true,
        Receiver::Spelled { was, .. } => relays_a_parameter(was),
        _ => false,
    }
}

/// Whether this shape is `super`, directly or through an assignment.
///
/// [`relays_a_parameter`]'s question, for the other shape that often ends at nothing: `super` in a
/// **module** resolves through the including class's ancestry, which the module lacks, and a
/// `super` nothing above declares answers nothing either.
///
/// `ActionController::Instrumentation#render` writes `render_output = nil` above
/// `render_output = super` inside a block, and every controller action ending in `render` reads its
/// type. Taking the newer write by position would turn `-> nil` into no label. So a `super` write
/// does not displace one with a real type; with no such write above it, the `super` is still taken.
fn reaches_a_super(receiver: &Receiver) -> bool {
    match receiver {
        Receiver::Super { .. } => true,
        Receiver::Spelled { was, .. } => reaches_a_super(was),
        _ => false,
    }
}

/// Whether this chain is rooted in `self`: the third assignment slot.
///
/// - **The same argument, one step weaker.** `Foo.bar` names a class the reader can check; `self`
///   in an RSpec block, a rake task or a top-level script is `Object`, and
///   `create(:story, title: "…")` resolves against it to nothing. Treated as solid, it would
///   displace `s = Story.find(s.id)` written above it.
/// - **Followed through calls, never through a variable.**
///   `tokens = user_tokens(account, …) + contact_tokens(…)` is a call on a call on `self`; asked
///   only about its last link it looks as solid as `Foo.bar.baz`, so the walk goes down the
///   `Returned`s. It stops at a `Spelled`: in `link = c.links.last`, where `c` is itself a call on
///   `self`, the chain is `c`'s problem and `c` carries its own name rung.
/// - **A written `self.foo` counts like an implicit one**: it is the same call.
fn rooted_in_self(receiver: &Receiver) -> bool {
    // A **bare** receiverless call carries its own name rung in a `Spelled` (the wrapper keeping a
    // spelling beside the shape). That wrapper is this same call, not a variable mid-chain, so it
    // is unwrapped once here and never followed again below.
    let receiver = match receiver {
        Receiver::Spelled { was, .. } => was.as_ref(),
        other => other,
    };
    rooted_in_a_call(receiver)
}

fn rooted_in_a_call(receiver: &Receiver) -> bool {
    match receiver {
        Receiver::SelfObject(_) => true,
        Receiver::Returned { on, .. } => rooted_in_a_call(on),
        _ => false,
    }
}

/// The parameters a `def` binds, as the ones a position or keyword can name.
///
/// **What is left out is the care here.** `*rest`, `**rest` and `&block` have container types, not
/// what the caller wrote. A positional *after* a rest has no position countable from the left (as
/// [`Receiver::Destructured`] refuses a splat on an assignment's left). A destructured positional,
/// `def f((a, b))`, binds names this does not track, but still **holds its place**, so the index
/// counts past it.
fn bound_by<'pr>(node: &DefNode<'pr>) -> Vec<DefParameter<'pr>> {
    let Some(written) = node.parameters() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // **One loop over both, with the index from the walk, not a counter.** Ruby numbers required
    // and optional positionals in one sequence, so a destructured parameter holds its place for
    // free: it `continue`s, and the next position is still the next position.
    for (index, parameter) in written
        .requireds()
        .iter()
        .chain(written.optionals().iter())
        .enumerate()
    {
        let (name, default) = match (
            parameter.as_required_parameter_node(),
            parameter.as_optional_parameter_node(),
        ) {
            (Some(required), _) => (required.name(), None),
            (_, Some(optional)) => (optional.name(), Some(optional.value())),
            // `def f((a, b))`: a destructure, whose names are bound one level down.
            _ => continue,
        };
        out.push(DefParameter {
            name: String::from_utf8_lossy(name.as_slice()).into_owned(),
            slot: ParameterSlot::Positional(index),
            default,
        });
    }
    // `posts()` are the positionals after a `*rest`, deliberately not walked.
    for parameter in written.keywords().iter() {
        let (name, default) = if let Some(required) = parameter.as_required_keyword_parameter_node()
        {
            (required.name(), None)
        } else if let Some(optional) = parameter.as_optional_keyword_parameter_node() {
            (optional.name(), Some(optional.value()))
        } else {
            continue;
        };
        out.push(DefParameter {
            name: String::from_utf8_lossy(name.as_slice()).into_owned(),
            slot: ParameterSlot::Keyword(String::from_utf8_lossy(name.as_slice()).into_owned()),
            default,
        });
    }
    out
}

/// One `def`, as the three things a read inside it may ask about.
struct Def<'pr> {
    span: (u32, u32),
    name: String,
    /// The parameters this `def` binds, in countable order.
    ///
    /// Only those that can have a slot: `*rest`, `**rest`, `&block` and every positional after a
    /// rest are left out, not numbered. See [`ParameterSlot`].
    parameters: Vec<DefParameter<'pr>>,
}

/// One parameter of a `def`: what it is called, where it sits, and what it falls back to.
struct DefParameter<'pr> {
    name: String,
    slot: ParameterSlot,
    /// The expression after the `=`, unresolved: a shape, whose class is [`types`](super::types)'
    /// question.
    default: Option<Node<'pr>>,
}

/// One parameter of one block, and the call the block was written on.
struct BlockParameter<'pr> {
    name: (u32, u32),
    /// Which positional parameter it is: the index RBS lists a block's own parameters by.
    index: usize,
    /// The block's whole span, so a read of that name outside it is not this parameter.
    body: (u32, u32),
    /// The call, unclassified, for [`LocalWrite::value`]'s reason: classifying it is
    /// `receiver_of`'s job, which cannot run during the collecting walk.
    call: Node<'pr>,
}

/// One `x = <something>`, kept as a span, not a name, so matching allocates nothing.
struct LocalWrite<'pr> {
    name: (u32, u32),
    /// The end of the assigned *value*: what "before the cursor" must mean. Using the name would
    /// let `x = x.` type `x` by the half-written statement it is part of.
    at: u32,
    /// The value, unclassified, so an assignment whose right side is another local (`b = a.foo`)
    /// can be answered in whichever order the walk reached the two.
    value: Node<'pr>,
    /// Which target of a **multiple assignment** this name is, if any.
    ///
    /// `None` is plain `x = value`, where the value *is* the type. `Some(i)` is `a, b = value`,
    /// where the type is the value's `i`th element: the one question here answered by a position,
    /// not a shape. See [`Receiver::Destructured`].
    index: Option<u32>,
}

impl<'pr> Visit<'pr> for Finder<'_, 'pr> {
    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        // A pre-order walk visits the cursor's ancestors outermost first, and only ancestors
        // contain the cursor, so overwriting on every hit leaves the innermost.
        if let Some(operator) = node.call_operator_loc()
            && let Some(message) = node.message_loc()
            // From just past the operator to the end of the message. The message is usually empty
            // and at the cursor, but a trailing `.` above an `end` makes Prism read the `end` as
            // the method name, with the cursor before it.
            && operator.end_offset() as u32 <= self.offset
            && self.offset <= message.end_offset() as u32
        {
            self.repair = Some((
                operator.start_offset() as u32,
                // The message is usually empty and at the cursor. If not, it is either the
                // half-typed word (blanked with the operator) or, for a dangling `.` above an
                // `end`, the `end` keyword, which must survive: blanking it would break the
                // structure being repaired.
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
                // No receiver written, or one written `self`, through `.` or `::` (one node either
                // way). See `Context::allows_private`.
                allows_private: node
                    .receiver()
                    .is_none_or(|receiver| receiver.as_self_node().is_some()),
            });
        }

        // A block's parameters and the call that hands them over. Read here, not in
        // `visit_block_node`, because both are needed together and only the call node holds both.
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

    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        // An index, not a search (see [`Finder::defs`]). Every other walk in this impl looks for
        // one node; this one builds a table, so nothing stops it.
        self.defs.push(Def {
            span: (
                node.location().start_offset() as u32,
                node.location().end_offset() as u32,
            ),
            name: String::from_utf8_lossy(node.name().as_slice()).into_owned(),
            parameters: bound_by(node),
        });
        ruby_prism::visit_def_node(self, node);
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
                index: None,
            });
        }
        ruby_prism::visit_local_variable_write_node(self, node);
    }

    /// `a, b = value`: every target countable from the left.
    ///
    /// **Refused entirely when a `*rest` is written.** In `a, *b, c = value` only `a` is fixed at
    /// 0; `c`'s position depends on the value's length. Keeping the names before the rest would be
    /// correct but not worth it: splats in destructures are rare, and half an answer reads like a
    /// whole one.
    fn visit_multi_write_node(&mut self, node: &MultiWriteNode<'pr>) {
        let at = node.value().location().end_offset() as u32;
        if at <= self.offset && node.rest().is_none() {
            let targets = node.lefts().iter().count();
            // A written list (`a, b = foo, bar`) gives each name its own element, exactly, with no
            // signature. Anything else is one value spread by a declaration:
            // `Receiver::Destructured`. `Node` is not `Clone`, so the value is re-read from `node`
            // per target.
            let written = node
                .value()
                .as_array_node()
                .filter(|array| !array.is_contains_splat())
                .is_some_and(|array| array.elements().iter().count() == targets);
            for (index, target) in node.lefts().iter().enumerate() {
                let Some(target) = target.as_local_variable_target_node() else {
                    continue;
                };
                let element = written
                    .then(|| {
                        node.value()
                            .as_array_node()
                            .and_then(|array| array.elements().iter().nth(index))
                    })
                    .flatten();
                let Ok(index) = u32::try_from(index) else {
                    continue;
                };
                let name = target.location();
                self.locals.push(LocalWrite {
                    name: (name.start_offset() as u32, name.end_offset() as u32),
                    at,
                    index: element.is_none().then_some(index),
                    value: element.unwrap_or_else(|| node.value()),
                });
            }
        }
        ruby_prism::visit_multi_write_node(self, node);
    }

    fn visit_instance_variable_write_node(&mut self, node: &InstanceVariableWriteNode<'pr>) {
        self.note_instance_write(&node.name_loc(), node.value());
        ruby_prism::visit_instance_variable_write_node(self, node);
    }

    // `@cache ||= build` is Ruby's memoisation and as much an assignment as `=`. `@n += 1` is not:
    // an operator write says what happens to a value, not what it is.
    fn visit_instance_variable_or_write_node(&mut self, node: &InstanceVariableOrWriteNode<'pr>) {
        self.note_instance_write(&node.name_loc(), node.value());
        ruby_prism::visit_instance_variable_or_write_node(self, node);
    }

    fn visit_instance_variable_and_write_node(&mut self, node: &InstanceVariableAndWriteNode<'pr>) {
        self.note_instance_write(&node.name_loc(), node.value());
        ruby_prism::visit_instance_variable_and_write_node(self, node);
    }

    // Exactly the four shapes rubydex files a `Definition::Constant` for. `A &&= v` and `A += v`
    // record only a *reference* upstream, so a definition's span could never name them, and an
    // operator write says what happens to a value, not what it is (the instance-variable visitors'
    // rule).
    fn visit_constant_write_node(&mut self, node: &ConstantWriteNode<'pr>) {
        self.note_constant_write(&node.name_loc(), node.value());
        ruby_prism::visit_constant_write_node(self, node);
    }

    fn visit_constant_or_write_node(&mut self, node: &ConstantOrWriteNode<'pr>) {
        self.note_constant_write(&node.name_loc(), node.value());
        ruby_prism::visit_constant_or_write_node(self, node);
    }

    // `Foo::BAR = x`. The name is the target's **last segment**, which is what rubydex records, so
    // `Foo::BAR` and a plain `BAR` arrive spelled the same.
    fn visit_constant_path_write_node(&mut self, node: &ConstantPathWriteNode<'pr>) {
        self.note_constant_write(&node.target().name_loc(), node.value());
        ruby_prism::visit_constant_path_write_node(self, node);
    }

    fn visit_constant_path_or_write_node(&mut self, node: &ConstantPathOrWriteNode<'pr>) {
        self.note_constant_write(&node.target().name_loc(), node.value());
        ruby_prism::visit_constant_path_or_write_node(self, node);
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
            constant_writes: Vec::new(),
            defs: Vec::new(),
            repair: None,
            memo: RefCell::new(Answered::new()),
        }
    }

    /// `super`, read as the method it really calls: the enclosing `def`'s name.
    ///
    /// - **Two nodes, differing in arity.** `super(a, b)` is a [`ruby_prism::SuperNode`] with its
    ///   own arguments. A bare `super` is a [`ruby_prism::ForwardingSuperNode`] and passes on
    ///   whatever the caller got, which this file cannot count, so it is [`Arity::Unknown`], like a
    ///   splat. Both can carry a block.
    /// - **A `super` outside every `def` answers nothing.** It is legal in a `define_method` block,
    ///   where the name is the macro's symbol, not anything the walk has. Answering from the last
    ///   walked `def` would read an unrelated method.
    fn super_in(&self, node: &Node<'_>) -> Option<Receiver> {
        let (block, arity) = if let Some(found) = node.as_super_node() {
            (
                found.block().is_some(),
                written_arity(found.arguments().as_ref()),
            )
        } else if let Some(found) = node.as_forwarding_super_node() {
            (found.block().is_some(), Arity::Unknown)
        } else {
            return None;
        };
        let at = node.location().start_offset() as u32;
        let method = self.enclosing_def(at).map(|found| found.name.clone())?;
        Some(Receiver::Super {
            at,
            method,
            block,
            arity,
        })
    }

    /// The innermost `def` a byte falls in, or `None` outside all of them.
    ///
    /// The **last** match, because the walk is pre-order: an enclosing `def` is pushed before
    /// anything inside it, so the latest entry still containing the offset is the nearest. `def`
    /// inside `def` is legal Ruby, and this reads it right.
    fn enclosing_def(&self, at: u32) -> Option<&Def<'pr>> {
        self.defs
            .iter()
            .rev()
            .find(|found| found.span.0 <= at && at < found.span.1)
    }

    /// Give a bare name the type its enclosing `def` declares for that parameter.
    ///
    /// - **Asked last, so purely additive.** Every write is asked first and the block parameter
    ///   next, as [`Finder::yielded_to`] is. Only names nothing else could type reach this, so no
    ///   existing answer is displaced; only [`Receiver::Named`] positions move.
    /// - **A same-named block parameter wins** by being asked first: Ruby shadows, and in
    ///   `def f(story); list.each { |story| ... }` the inner `story` is the block's.
    /// - **A `nil` default is refused.** `def f(x = nil)` means *optional, type unstated*; reading
    ///   it as `NilClass` would put a confident wrong class on the commonest optional parameter.
    ///   Every other default is taken as its shape.
    fn parameter_of(&self, span: (u32, u32), name: &str, budget: Budget) -> Option<Receiver> {
        let found = self.enclosing_def(span.0)?;
        let parameter = found
            .parameters
            .iter()
            .find(|parameter| parameter.name == name)?;
        let default = parameter
            .default
            .as_ref()
            .filter(|written| written.as_nil_node().is_none())
            .map(|written| Box::new(self.receiver_of(Some(written), budget.linked())));
        Some(Receiver::Parameter {
            at: found.span.0,
            method: found.name.clone(),
            slot: parameter.slot.clone(),
            default,
        })
    }

    /// One of the two branching arms, walked once per (span, budget); see [`Finder::memo`].
    fn memoised(
        &self,
        span: (u32, u32),
        budget: Budget,
        walk: impl FnOnce() -> Receiver,
    ) -> Receiver {
        if let Some(answered) = self.memo.borrow().get(&(span, budget)) {
            return answered.clone();
        }
        let answer = walk();
        self.memo
            .borrow_mut()
            .insert((span, budget), answer.clone());
        answer
    }

    /// The file with the half-typed call blanked out, byte for byte.
    ///
    /// Completion fires on invalid Ruby, and Prism recovers from a dangling `.` by reading whatever
    /// follows as the method name. For `@name.` above an `end`, that consumes the `end` and
    /// **reparents everything below it**: an `@name = "ada"` in an `initialize` written below lands
    /// inside the method being typed, and reads as a different variable.
    ///
    /// So the scope question is asked about the file without the operator being typed. Every byte
    /// except newlines becomes a space, as [`signatures`](super::signatures) does, so every offset
    /// and line in the answer is the caller's.
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
        // Whole characters were replaced by ASCII, so this holds. If not, the result is a scope
        // question about mangled text, never a panic.
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

    fn note_constant_write(&mut self, name: &Location<'_>, value: Node<'pr>) {
        self.constant_writes.push(ConstantWrite {
            name: (name.start_offset() as u32, name.end_offset() as u32),
            value,
        });
    }

    /// What the held operator completes after, now that the whole file has been walked.
    fn classify(&self, pending: &Pending<'_>) -> Context {
        match pending {
            Pending::MethodCall(receiver) => Context::MethodCall {
                receiver: self.receiver_of(receiver.as_ref(), Budget::default()),
            },
            Pending::NamespaceAccess(parent) => Context::NamespaceAccess {
                receiver: match parent {
                    Some(parent) => self.receiver_of(Some(parent), Budget::default()),
                    None => Receiver::TopLevel,
                },
            },
        }
    }

    /// The span between a call's parentheses, or of its bare argument list.
    ///
    /// Prism puts a synthetic zero-width `)` at the last token it could read, so `foo(1, ` closes
    /// at the comma with the cursor past the end. Stepping over trailing separators recovers that
    /// without running past the end of the line.
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
    /// - **Positional:** the count of arguments that *end* before the cursor. `f(1, ` has finished
    ///   one; `f(1` and `f(` none.
    /// - **A keyword hash is spread into its elements**: `f(a: 1, b: 2` is one Prism node but two
    ///   arguments.
    /// - **A keyword the cursor is *inside* beats the count**, since keywords come in any order and
    ///   position says nothing about which parameter one is.
    fn active_argument(&self, node: &CallNode<'_>) -> Active {
        let Some(arguments) = node.arguments() else {
            return Active::Nth(0);
        };

        let mut elements: Vec<Node<'_>> = Vec::new();
        for argument in arguments.arguments().iter() {
            // Only a hash Prism marks as keywords. `f("a" => 1, "b" => 2)` is one argument however
            // many pairs it has, and spreading it would count two.
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

    /// Whether the cursor belongs to this argument rather than the next.
    ///
    /// - **Inside its span, and also *past* it up to the ending comma.** That gap is where the
    ///   cursor usually is: `create(name: ` has the keyword but no value yet, and Prism ends the
    ///   pair at the colon. The comma means the user has moved on, so `create(name: "ada", `
    ///   belongs to the next argument.
    /// - **No upper bound to check.** The caller already knows the cursor is inside the argument
    ///   list, and no argument's span reaches past it. A heredoc does not either: Prism scopes the
    ///   node to the `<<~SQL` marker and holds the body separately, so `execute(<<~SQL, id)` needs
    ///   no special case.
    fn holds_cursor(&self, start: u32, end: u32) -> bool {
        if self.offset < start {
            return false;
        }
        self.offset <= end || !self.source[end as usize..self.offset as usize].contains(',')
    }

    /// What a receiver node is, as far as syntax can say.
    ///
    /// [`Budget`] bounds the recursion on two axes. Every rule below recurses, legal Ruby can make
    /// two of them recurse forever (`(((x)))`, `x = x.foo`), and two of them **branch**. Which
    /// counter a rule spends is the whole difference; see [`Budget`].
    fn receiver_of(&self, node: Option<&Node<'_>>, budget: Budget) -> Receiver {
        let Some(node) = node else {
            return Receiver::Unknown;
        };
        if budget.spent() {
            return Receiver::Unknown;
        }
        // `(1..9).each`: parentheses are how a range or ternary gets a receiver at all, so seeing
        // through a single-statement one is the common spelling, not an optimisation.
        if let Some(inner) = unparenthesised(node) {
            return self.receiver_of(Some(&inner), budget.linked());
        }
        // An assignment **is** its value, by Ruby's rule (see [`assigned_value`]). A link, not a
        // spread: one question, asked once.
        if let Some(value) = assigned_value(node) {
            return self.receiver_of(Some(&value), budget.linked());
        }
        // `obj&.x = v` is the argument where the receiver is not `nil`, and `nil` where it is: the
        // one union [`attribute_written`] refuses. Stopping here makes that refusal real. Falling
        // through would read the message as a call to the **getter** and answer one side of the
        // union without the other.
        if is_safe_attribute_write(node) {
            return Receiver::Unknown;
        }
        if node.as_self_node().is_some() {
            return Receiver::SelfObject(node.location().start_offset() as u32);
        }
        if is_constant(node) {
            // The end of the path, inside its last segment: `HR::Person` resolves as a whole, and
            // its graph reference ends here too.
            return Receiver::Constant(node.location().end_offset() as u32);
        }
        if let Some(class) = literal_class(node) {
            return Receiver::Literal {
                class,
                arguments: held_by(node, class),
            };
        }
        if let Some(offset) = instantiated(node) {
            return Receiver::Instance(offset);
        }
        if let Some(receiver) = self.super_in(node) {
            return receiver;
        }
        if let Some(span) = local_span(node) {
            return self.memoised(span, budget, || self.type_the_local(span, budget));
        }
        if let Some(span) = instance_span(node) {
            return self.memoised(span, budget, || {
                self.type_the_instance_variable(span, budget)
            });
        }
        // `a && b` and `a || b` return **one of their operands**, never a third thing: Ruby's rule.
        // Asked here, decided elsewhere: which operand depends on `a`'s class, and this module
        // decides no classes. See [`Receiver::Shortcut`].
        if let Some(receiver) = self.shortcut(node, budget) {
            return receiver;
        }
        // `!x` returns `true` or `false`, never a third thing (Ruby's rule), and unlike `&&` this
        // holds however unreadable the operand is. Asked here, decided elsewhere, for
        // [`Receiver::Shortcut`]'s reason.
        if let Some(receiver) = self.negated(node, budget) {
            return receiver;
        }
        self.returned_by(node, budget)
    }

    /// `a && b` or `a || b`, as the two shapes it joins.
    ///
    /// - **`and` and `or` are the same Prism nodes as `&&` and `||`.** They differ only in
    ///   precedence, which this does not read.
    /// - **A spread, not a link**, which is what [`Budget`]'s two counters are for: this rule
    ///   *branches*, resolving two sub-expressions. `a && b && c` nests, so the bound is real.
    /// - **An unreadable operand is carried as [`Receiver::Unknown`]**, not a refusal of the pair,
    ///   because the untaken side is never read: `nil && whatever` is `nil`.
    fn shortcut(&self, node: &Node<'_>, budget: Budget) -> Option<Receiver> {
        let (left, right, and) = match node.as_and_node() {
            Some(found) => (found.left(), found.right(), true),
            None => {
                let found = node.as_or_node()?;
                (found.left(), found.right(), false)
            }
        };
        let budget = budget.spread();
        Some(Receiver::Shortcut {
            left: Box::new(self.receiver_of(Some(&left), budget)),
            right: Box::new(self.receiver_of(Some(&right), budget)),
            and,
        })
    }

    /// `!x`, as the shape it negates.
    ///
    /// - **Prism spells both `!x` and `not x` as a `CallNode` named `!` with no arguments**, so one
    ///   test covers both.
    /// - **The argument check keeps it honest.** `x.!(y)` is someone's own two-argument `!`, not
    ///   the operator, and neither is a `!` with a block.
    /// - **A spread, not a link**, like [`Self::shortcut`]: it resolves a sub-expression instead of
    ///   following the chain one step.
    fn negated(&self, node: &Node<'_>, budget: Budget) -> Option<Receiver> {
        let call = node.as_call_node()?;
        if call.name().as_slice() != b"!" {
            return None;
        }
        if call.block().is_some()
            || call
                .arguments()
                .is_some_and(|written| !written.arguments().is_empty())
        {
            return None;
        }
        let on = call.receiver()?;
        Some(Receiver::Negated(Box::new(
            self.receiver_of(Some(&on), budget.spread()),
        )))
    }

    /// Give a local the type of the assignment it came from.
    ///
    /// - **The nearest preceding assignment *that produced a type* wins.** That is the whole
    ///   analysis. A variable reassigned in a branch, or in a block that never runs, is answered by
    ///   the textually last assignment: a *wrong* answer, not a missing one, and the only place in
    ///   this module where that happens. The provenance makes it visible.
    /// - **Prism's `depth` is ignored on purpose.** A block's `x` and the outer `x` are treated as
    ///   one variable, which they usually are.
    /// - **Only assignments whose value ends before the *read* count.** Stricter than "before the
    ///   cursor", this makes `x = x.foo` terminate by itself, not via `fanout`: the read inside
    ///   the value cannot be answered by its own write.
    /// - **The answer carries the variable's own spelling** beside the assignment's shape, so an
    ///   untypable assignment never leaves the variable worse off than no assignment (see
    ///   [`Receiver::Spelled`]).
    fn type_the_local(&self, span: (u32, u32), budget: Budget) -> Receiver {
        let name = &self.source[span.0 as usize..span.1 as usize];
        // Two slots: the latest write that produced a shape, and the latest that produced one only
        // by *relaying a block parameter* (`max_distance_color = color` inside
        // `palette.each do |color|`) or by **`super`**.
        //
        // Two slots, not one `max_by_key`, because a relayed shape is a disguised fallback.
        // `Receiver::Yielded` ends at the name when the callee's signature says nothing about its
        // block, and `Receiver::Super` ends at nothing when written in a module; both are the usual
        // case. Letting them win on position alone would drop `max_distance_color = nil` written
        // above, and `render_output = nil` above `render_output = super`, which every Rails action
        // ending in `render` reads.
        let mut solid: Option<(u32, Receiver)> = None;
        let mut relayed: Option<(u32, Receiver)> = None;
        // The third slot, ranked **below** the block parameter: a write in another method whose
        // value is a call on `self` must not take `uploader` away from the
        // `SubforemImageUploader.new.tap do |uploader|` the cursor is inside.
        let mut rooted: Option<(u32, Receiver)> = None;
        // **Newest first**, so the loop can stop at the first write that fills `solid`.
        //
        // Exact, not an approximation: `solid` is taken outright below and was already the write
        // with the highest `at`, so older candidates were visited only to be discarded, and each
        // visit is a *recursion* (why this arm multiplies). `relayed` and `rooted` cannot end the
        // loop, because an older solid write still beats a newer relayed one.
        //
        // Reversed before the stable sort, so two writes whose values end at the same offset still
        // resolve to the later one.
        let mut candidates = self
            .locals
            .iter()
            .filter(|write| {
                write.at <= span.0
                    && self.source[write.name.0 as usize..write.name.1 as usize] == *name
            })
            .collect::<Vec<_>>();
        candidates.reverse();
        candidates.sort_by_key(|write| std::cmp::Reverse(write.at));
        for write in candidates {
            // [`Budget::spread`], not `linked`: this loop runs once per write of the name, so the
            // step multiplies instead of adding.
            let receiver = self.receiver_of(Some(&write.value), budget.spread());
            // A `Receiver::Named` counts as nothing here; that keeps the last rung last.
            // `x = Person.new` above `x = whatever` must keep answering `Person`, not be displaced
            // by a name to guess from. The guess uses *this* variable's own spelling, below.
            if matches!(receiver, Receiver::Unknown | Receiver::Named(_)) {
                continue;
            }
            // A multiple-assignment target holds the value's `index`th element, not the value.
            // Wrapped *after* the two refusals above, so a destructure of something unanswerable
            // stays unanswerable rather than becoming an index into nothing.
            let receiver = match write.index {
                Some(index) => Receiver::Destructured {
                    of: Box::new(receiver),
                    index,
                },
                None => receiver,
            };
            let slot = if relays_a_parameter(&receiver) || reaches_a_super(&receiver) {
                &mut relayed
            } else if rooted_in_self(&receiver) {
                &mut rooted
            } else {
                &mut solid
            };
            // Writes arrive newest first, so the first to reach a slot is its answer.
            if slot.is_none() {
                *slot = Some((write.at, receiver));
            }
            if solid.is_some() {
                break;
            }
        }
        solid
            .or(relayed)
            .map(|(_, receiver)| receiver)
            // No assignment produced a type, so the variable may be a **block parameter**, typed by
            // the signature of the method its block was passed to. Asked after the writes, never
            // before, so it cannot displace their answers; the same order `Receiver::Spelled` keeps
            // between an assignment and a name.
            .or_else(|| self.yielded_to(span, name, budget))
            // Then a write rooted in a call on `self`, which may resolve to nothing (see
            // [`rooted_in_self`]). A precedence, not a refusal: with no other write and no
            // enclosing block, the chain is taken.
            .or_else(|| rooted.map(|(_, receiver)| receiver))
            // Last, the `def`'s own header. Nothing above wrote this name, so it is a **method
            // parameter**: the one binding this walk never collects, since `locals` is filled by
            // writes and a parameter is not one. Asked after every write and the block, so it only
            // fills gaps. See [`Finder::parameter_of`].
            .or_else(|| self.parameter_of(span, name, budget))
            .map_or_else(
                || Receiver::Named(name.to_owned()),
                // The shape *and* the spelling, because the shape can still fail to type: a chain
                // through an undeclared method must end where a bare `story` ends, not below it.
                // See `Receiver::Spelled`.
                |receiver| Receiver::Spelled {
                    was: Box::new(receiver),
                    name: name.to_owned(),
                },
            )
    }

    /// Give a block parameter the type the called method says its block receives.
    ///
    /// **The innermost enclosing block wins**, the one place this walk cares about nesting: in
    /// `stories.each { |s| s.tags.each { |s| ... } }` the read means the inner `s`. Blocks not
    /// containing the read are not candidates, which makes this narrower than
    /// [`Finder::type_the_local`], where any write above the cursor counts.
    fn yielded_to(&self, span: (u32, u32), name: &str, budget: Budget) -> Option<Receiver> {
        let parameter = self
            .yielded
            .iter()
            .filter(|parameter| {
                parameter.body.0 <= span.0
                    && span.1 <= parameter.body.1
                    && self.source[parameter.name.0 as usize..parameter.name.1 as usize] == *name
            })
            // Innermost: the shortest span still containing the read.
            .min_by_key(|parameter| parameter.body.1 - parameter.body.0)?;
        self.yielded_shape(parameter, budget)
    }

    /// The same shape, for a parameter already picked out.
    ///
    /// [`Finder::yielded_to`] searches for the parameter a *read* means; [`bindings_in`] already
    /// has one, since it reports every parameter. Both need the call classified, which cannot
    /// happen during the walk, so the tail is shared.
    fn yielded_shape(&self, parameter: &BlockParameter<'pr>, budget: Budget) -> Option<Receiver> {
        // One block, one call: the innermost parameter is picked before this runs, so this is a
        // link, not a visit to every candidate.
        let Receiver::Returned { on, method, .. } =
            self.receiver_of(Some(&parameter.call), budget.linked())
        else {
            // No receiver written means an implicit `self`, which reaches `Unknown`, so there is no
            // signature to ask. `each { |x| }` in a model body is a `yield`, and what a `yield`
            // hands over is the method body's business, not a declaration's.
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
    /// - **Which `@foo` this is, is not a syntax question**, so [`scopes`] answers it, not this
    ///   walk. `@v` in `def a` and in `def self.b` are two variables; a `def c` inside
    ///   `class << self` shares the second. That logic was written for `documentHighlight` and is
    ///   reused unchanged: a second copy would drift, and a highlight and a completion disagreeing
    ///   about `@foo` would be invisible in both.
    /// - **It costs a second parse**, on this path only, and guarantees the two walks agree.
    /// - **The textually last typed assignment wins**: *last*, not last-before-the-cursor, because
    ///   `initialize` is as often below the reading method as above it.
    /// - **Bounded to the file.** A class reopened elsewhere is a second question, handled by the
    ///   ancestor rung.
    fn type_the_instance_variable(&self, span: (u32, u32), budget: Budget) -> Receiver {
        // The name is what is left when no assignment answers, and both rungs below the graph work
        // from it. `@` included, as written and as `scopes` spans it.
        let spelling = &self.source[span.0 as usize..span.1 as usize];
        let named = || Receiver::Named(spelling.to_owned());
        let repaired = self.without_the_half_typed_call();
        let Some((_, occurrences)) = scopes::variable(&repaired, span.0) else {
            return named();
        };
        // Two slots, the same rule as `type_the_local`: a write whose value may still end as a
        // guess is taken only if no write produced a shape that cannot.
        let mut solid: Option<(u32, Receiver)> = None;
        let mut relayed: Option<(u32, Receiver)> = None;
        let mut rooted: Option<(u32, Receiver)> = None;
        // Newest first, for [`Finder::type_the_local`]'s reason: `scopes` returns occurrences
        // sorted by offset and the last write wins, so reading backwards lets the loop stop at the
        // first that fills `solid`. Every candidate not reached is a recursion not taken.
        for occurrence in occurrences
            .iter()
            .rev()
            .filter(|occurrence| occurrence.write)
        {
            let Some(write) = self
                .instance_writes
                .iter()
                .find(|write| write.name == (occurrence.start, occurrence.end))
            else {
                continue;
            };
            // `@foo = @foo.bar` reads the variable inside its own write, which cannot answer that
            // read. The local rule, stated against the value's span because an instance variable
            // has no "before".
            if write.value_span.0 <= span.0 && span.1 <= write.value_span.1 {
                continue;
            }
            // Nor can it answer **any** read of that variable, not just one inside it: the value's
            // type is the question being asked. `type_the_local` gets this from position
            // (candidates are writes *before* the read, shrinking every hop). An instance variable
            // has no "before", since a write in another method is a valid answer, so the rule must
            // be stated.
            //
            // Stating it also stops the walk exploding. `fanout` bounds how many times this is
            // asked, not how *wide* one ask is: every write of `@x` is a candidate for every read,
            // and each `@x = @x.foo` asks again. discourse's `lib/topics_filter.rb` assigns
            // `@scope` sixty times, mostly from itself; without this arm one
            // `textDocument/definition` there took seconds instead of milliseconds.
            if occurrences.iter().any(|read| {
                !read.write && write.value_span.0 <= read.start && read.end <= write.value_span.1
            }) {
                continue;
            }
            // [`Budget::spread`], for the reason above: this is the branching arm, and `fanout`
            // is its bound.
            let receiver = self.receiver_of(Some(&write.value), budget.spread());
            // As in `type_the_local`: a name is not a type, and letting one win would lose an exact
            // assignment written above.
            if matches!(receiver, Receiver::Unknown | Receiver::Named(_)) {
                continue;
            }
            let slot = if relays_a_parameter(&receiver) || reaches_a_super(&receiver) {
                &mut relayed
            } else if rooted_in_self(&receiver) {
                &mut rooted
            } else {
                &mut solid
            };
            if slot.is_none() {
                *slot = Some((occurrence.start, receiver));
            }
            if solid.is_some() {
                break;
            }
        }
        let typed = solid.or(relayed).or(rooted);
        // The spelling survives an assignment here as for a local, and the wrapped shape is the
        // whole `Assigned`. A chain that types keeps the note naming its assignment line; one that
        // does not falls to the rungs a bare `@story` would reach.
        typed.map_or_else(named, |(at, was)| Receiver::Spelled {
            was: Box::new(Receiver::Assigned {
                at,
                was: Box::new(was),
            }),
            name: spelling.to_owned(),
        })
    }

    /// A call's return value, as a shape a lookup can use.
    ///
    /// Nothing is followed and no type decided: this records that a name was called on something,
    /// and [`types`](super::types) asks RBS what that returns. A call with no written receiver
    /// **is** one of these, on [`Receiver::SelfObject`], as Ruby says, which makes it a graph
    /// question.
    fn returned_by(&self, node: &Node<'_>, budget: Budget) -> Receiver {
        let Some(call) = node.as_call_node() else {
            return Receiver::Unknown;
        };
        // **The name Ruby looks up, not the text under the message span.** They agree for dotted
        // calls and differ for the two punctuation shapes: `rows[key]`'s span is `[key]` but its
        // name is `[]`, and `-count`'s span is `-` but its name is `-@`. Reading the span would
        // send every index to the table as a member called `[key]`, so `Hash#[]` and `Array#[]`
        // could never be asked for.
        let name = call.name();
        let method = String::from_utf8_lossy(name.as_slice());
        // `foo.()` is `foo.call()` written with no name, and a call Prism recovered with no message
        // span is the same fact. **The span says so; the name cannot**: `foo.()` is named `call`
        // and a mid-edit `foo.` is named nothing. Below this line both mean one thing: there is no
        // name *here*.
        if call
            .message_loc()
            .is_none_or(|message| message.start_offset() == message.end_offset())
        {
            return Receiver::Unknown;
        }
        let Some(receiver) = call.receiver() else {
            // No receiver written means an implicit `self`, taken literally. So
            // `api_key_scopes.first` on a model resolves exactly like `self.api_key_scopes.first`,
            // instead of guessing a class called `ApiKeyScopes` and offering hundreds of
            // definitions.
            //
            // Nothing new is declared and no rung added: `SelfObject` is what a *written* `self`
            // already produces, and `types::method_receiver` types it as the enclosing class.
            let returned = Receiver::Returned {
                // The implicit `self` stands where the call is written, the same fact a written one
                // records (see `Receiver::SelfObject`).
                on: Box::new(Receiver::SelfObject(node.location().start_offset() as u32)),
                method: method.to_string(),
                block: self.block_written(&call, budget),
                arity: arity_of(&call),
                arguments: self.written_arguments(&call, budget),
            };
            // The name rung sits **below** the lookup, not beside it: `Receiver::Spelled`'s reason
            // to exist. `types` asks the shape first and reaches the name only if that answered
            // nothing, so a resolving chain is never displaced by a guess, and a guess is never
            // lost to a chain that failed.
            //
            // Only a bare name reaches it. `find(id).title` and `each { }.first` are expressions
            // whose *spelling* says nothing about what they return, so no guess is made from them.
            // Asking the graph what `self.find(id)` returns is not a guess at all.
            return if call.arguments().is_none() && call.block().is_none() {
                Receiver::Spelled {
                    was: Box::new(returned),
                    name: method.into_owned(),
                }
            } else {
                returned
            };
        };
        // The linear arm, and the only one [`links`]' twenty is spent on: one call per `.` written,
        // with no candidate list below.
        let on = self.receiver_of(Some(&receiver), budget.linked());
        // One `Unknown` ends the chain instead of being carried: with nothing to look the method up
        // *on*, every link above is unanswerable, and a `Returned` around an `Unknown` would only
        // make the graph side rediscover that.
        if matches!(on, Receiver::Unknown) {
            return Receiver::Unknown;
        }
        Receiver::Returned {
            on: Box::new(on),
            method: method.into_owned(),
            block: self.block_written(&call, budget),
            arity: arity_of(&call),
            arguments: self.written_arguments(&call, budget),
        }
    }

    /// The shape of every positional argument a call wrote, or nothing at all.
    ///
    /// - **[`arity_of`]'s twin**, walking the same list by the same rules (a keyword hash is
    ///   skipped; a splat or `...` gives up), so the two cannot disagree about what an argument is.
    /// - **A fan-out step**: the walk visits several expressions instead of following one.
    /// - **Past [`MAX_WIDTH`] arguments, or with the budget spent, the list is emptied, not
    ///   shortened.** A partial list is worse than none: every position after a dropped one would
    ///   be miscounted, and the consumer needs one shape per counted argument. See
    ///   [`Receiver::Returned`]'s `arguments`.
    fn written_arguments(&self, call: &CallNode<'_>, budget: Budget) -> Vec<Receiver> {
        let Some(written) = call.arguments() else {
            return Vec::new();
        };
        if budget.spent() {
            return Vec::new();
        }
        let mut shapes = Vec::new();
        for argument in written.arguments().iter() {
            if argument.as_splat_node().is_some()
                || argument.as_forwarding_arguments_node().is_some()
            {
                return Vec::new();
            }
            if argument.as_keyword_hash_node().is_some() {
                continue;
            }
            if shapes.len() >= MAX_WIDTH {
                return Vec::new();
            }
            shapes.push(self.receiver_of(Some(&argument), budget.spread()));
        }
        shapes
    }

    /// What a call's block slot holds: whether a block was written, and what it returns.
    fn block_written(&self, call: &CallNode<'_>, budget: Budget) -> Block {
        let Some(written) = call.block() else {
            return Block::None;
        };
        // `&:upcase` and a forwarded `&blk`. Ruby passes a block either way, so the signature's
        // block arm applies, but there is no body here to read a value from.
        let Some(block) = written.as_block_node() else {
            return Block::Written(Box::default());
        };
        Block::Written(self.handed_back(&block, budget.spread()))
    }

    /// The shapes a block's body returns.
    ///
    /// [`returns_of`]'s walk, over one block: the last statement, expanded through a tail-position
    /// conditional, with an unwritten branch filed as its `nil`. An empty block is that `nil` too:
    /// `[1, 2].map { }` is `[nil, nil]`, an answer, not a gap.
    ///
    /// - **Deliberately not [`Visit::visit`]** (what [`returns_in`] runs): a `return` inside a
    ///   block leaves the enclosing *method*, so collecting it would file another method's exit as
    ///   this block's value.
    /// - **A `next` or `break` anywhere inside refuses the whole block.** `next` is a block's
    ///   `return` and `break` abandons the method the block was passed to; neither is in tail
    ///   position. Reading only the tail would be a confident half-answer (see [`Exits::tail`]), so
    ///   the block comes back as one unreadable exit and the caller declines it.
    /// - **A spread, not a link**: the block is a second expression hanging off the call, not
    ///   another `.` in the chain, and a block whose tail is a local sends the walk to every write
    ///   of that name.
    fn handed_back(&self, block: &BlockNode<'_>, budget: Budget) -> Box<[Receiver]> {
        if escapes(block) {
            return Box::new([Receiver::Unknown]);
        }
        let here = (
            block.location().start_offset() as u32,
            block.location().end_offset() as u32,
        );
        let mut exits = Exits {
            open: vec![here],
            found: HashMap::new(),
            lambdas: 0,
        };
        match block.body() {
            Some(body) => match body.as_begin_node() {
                Some(found) => exits.rescued(&found, 0),
                None => exits.statements(body.as_statements_node().as_ref(), 0),
            },
            None => exits.push(Exit::Nil),
        }
        exits
            .found
            .remove(&here)
            .unwrap_or_default()
            .iter()
            .map(|exit| match exit {
                Exit::Written(node) => self.receiver_of(Some(node), budget),
                Exit::Nil => Receiver::literal("NilClass"),
                Exit::Unknown => Receiver::Unknown,
            })
            .collect()
    }

    /// `content` is the literal's text, delimiters excluded.
    ///
    /// Delimiters are excluded on purpose and the bounds are inclusive: a cursor on the closing
    /// quote of `"foo"` is where the next `.` gets typed, while a cursor at the end of `:foo` (no
    /// closing delimiter) is still inside the symbol.
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
    // `(a; b)` evaluates to `b`, but no real code writes a receiver that way, and guessing at it is
    // how a classification goes wrong.
    statements.next().is_none().then_some(only)
}

/// How many positional arguments a call wrote, as [`Arity`] spells it.
///
/// - **A keyword hash is not counted, on either side.** `3.7.round(half: :up)` writes none, and
///   RBS's `(?half: :up | :down | :even)` takes none. Prism reads a bare `k => v` tail (including
///   `**opts`) as keywords, Ruby 3's rule; braces make it a positional `Hash`, which counts.
/// - **A splat or `...` gives up on the count.** Guessing low would silently pick the arm with the
///   fewest parameters, the "nearest arm" this partition refuses.
fn arity_of(call: &CallNode<'_>) -> Arity {
    written_arity(call.arguments().as_ref())
}

/// The same count, read from an argument list instead of a call.
///
/// Split out for `super(a, b)`, whose node is not a [`CallNode`] but whose arguments count by the
/// same rule. One copy, so the keyword-hash and splat rules cannot drift apart.
fn written_arity(arguments: Option<&ruby_prism::ArgumentsNode<'_>>) -> Arity {
    let Some(arguments) = arguments else {
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

/// The value an assignment returns, which in Ruby is the assignment's own value.
///
/// `def show_title_h1; @title_h1 = true; end` returns `true`, exactly like
/// `def show_title_h1; true; end`: `=` is not a statement in Ruby. Many `def`s end in an
/// assignment, and many of those name their class right on the line.
///
/// Read here, not in [`Exits::tail`], because it is not a method-body rule: `(@a = b).c`,
/// `x = (y = f)` and a `def` ending in a write are the same rule, and one arm in
/// [`Finder::receiver_of`] answers all three.
///
/// **Left out, each a different question:**
///
/// - **`+=` and its family** ([`ruby_prism::InstanceVariableOperatorWriteNode`] and the rest).
///   `x += 1` returns the operator's result, not this node's `value()`; reading `value()` would
///   answer `Integer` for `list += [one]`.
/// - **`&&=`.** `@x &&= v` returns `@x` when it is falsy, so it is `v | nil | false`: a union,
///   which this module declines.
/// - **`obj&.x = v`.** It returns `nil` when the receiver is `nil`, so it is `v | nil`: the same
///   union, declined.
///
/// **`||=` is in, as an idiom rather than a rule.** `@memo ||= f` strictly returns `f`'s type
/// joined with the variable's old value. But it is usually memoisation, where the variable starts
/// `nil` and `f` is the answer. Where it is not, the *read* path already answers an instance
/// variable from its last write, so a card on `@memo` and a label on the `def` agree. Agreement is
/// worth more than one of them declining.
fn assigned_value<'pr>(node: &Node<'pr>) -> Option<Node<'pr>> {
    let value = if let Some(found) = node.as_local_variable_write_node() {
        found.value()
    } else if let Some(found) = node.as_local_variable_or_write_node() {
        found.value()
    } else if let Some(found) = node.as_instance_variable_write_node() {
        found.value()
    } else if let Some(found) = node.as_instance_variable_or_write_node() {
        found.value()
    } else if let Some(found) = node.as_class_variable_write_node() {
        found.value()
    } else if let Some(found) = node.as_class_variable_or_write_node() {
        found.value()
    } else if let Some(found) = node.as_global_variable_write_node() {
        found.value()
    } else if let Some(found) = node.as_global_variable_or_write_node() {
        found.value()
    } else if let Some(found) = node.as_constant_write_node() {
        found.value()
    } else if let Some(found) = node.as_constant_or_write_node() {
        found.value()
    } else if let Some(found) = node.as_constant_path_write_node() {
        found.value()
    } else if let Some(found) = node.as_constant_path_or_write_node() {
        found.value()
    } else if let Some(found) = node.as_call_node() {
        return attribute_written(&found);
    } else {
        return None;
    };
    Some(value)
}

/// The value `obj.x = v` and `h[k] = v` return: the **argument**, never the setter's body.
///
/// - **A rule about calls.** Ruby discards the setter's return and evaluates the assignment to its
///   right-hand side. `def x=(value); @x = value.to_s; end` really returns a `String` (via
///   `obj.send(:x=, v)`), yet `obj.x = v` still evaluates to `v`. So this reads the argument and
///   needs no lookup or graph.
/// - **Prism marks both spellings the same.** `attribute_write` is set on `obj.x = v`, on
///   `h[k] = v` (name `[]=`) and on `obj.x=(v)`, which is assignment syntax too. An ordinary
///   `obj.send(:x=, v)` is not marked, and does return the body's value.
/// - **The value is the last argument** in both spellings: `h[k, j] = v` writes the subscripts
///   first. A multiple assignment never reaches here: `obj.x, obj.y = 1, 2` parses as targets, and
///   the walk hands each its own element.
/// - **Safe navigation is refused**: `obj&.x = v` is `nil` where the receiver is, a union.
/// - **No fallback to the signature.** `Hash#[]=` is `(K, V) -> V`, but the argument written on the
///   line is the value itself, better than the bound declared on it.
fn attribute_written<'pr>(node: &ruby_prism::CallNode<'pr>) -> Option<Node<'pr>> {
    if !node.is_attribute_write() || node.is_safe_navigation() {
        return None;
    }
    node.arguments()?.arguments().iter().last()
}

/// Whether this is the one write [`attribute_written`] refuses: `obj&.x = v`.
///
/// Checked in [`Finder::receiver_of`], not here, because merely returning `None` would not refuse
/// it: the arms below would go on to answer the node as an ordinary call, whose message is the
/// **getter**'s name.
fn is_safe_attribute_write(node: &Node<'_>) -> bool {
    node.as_call_node()
        .is_some_and(|found| found.is_attribute_write() && found.is_safe_navigation())
}

/// The class Ruby gives a literal, or `None` if the node is not one.
///
/// Every entry reads back a parser decision, not a guess: `[1, 2]` is an `Array` in every program.
/// Interpolated forms are the same classes: `"a#{b}"` is a `String` however it was assembled.
fn literal_class(node: &Node<'_>) -> Option<&'static str> {
    let class = if node.as_string_node().is_some()
        || node.as_interpolated_string_node().is_some()
        // Backticks run a command and return its output.
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

/// What a literal holds, by the position of its class's type parameter.
///
/// Three Ruby literals are written *holding* something, and their classes are the three RBS gives
/// type parameters: `Array[E]`, `Hash[K, V]`, `Range[Elem]`. Every other literal answers an empty
/// list, as the three do when their contents cannot be named.
///
/// - **Only a literal counts as contents.** `[1, 2]` holds `Integer` because the parser said so;
///   `[story, other]` holds locals this function has no graph to resolve. So no budget, receiver,
///   recursion or lookup is needed. `[[1], [2]]` holds `Array`: the inner literal's class is what a
///   `.` on the element reaches, and what *it* holds is dropped, as a generic's argument is dropped
///   elsewhere.
/// - **Every element, or nothing.** A mixed array is a union, an empty one says nothing about its
///   future contents, and a splat is a value from elsewhere. All three give `None` at that
///   position, never the first element's class.
fn held_by(node: &Node<'_>, class: &str) -> Vec<Option<&'static str>> {
    match class {
        "Array" => {
            let elements = node
                .as_array_node()
                .expect("an Array literal is an array node");
            vec![one_class(elements.elements().iter())]
        }
        "Hash" => {
            let elements = node.as_hash_node().expect("a Hash literal is a hash node");
            // `**rest` is an `AssocSplatNode` with neither half, so a hash holding one names
            // neither keys nor values. Collecting into two lists that must both match the element
            // count already says that.
            let pairs: Vec<_> = elements
                .elements()
                .iter()
                .filter_map(|element| element.as_assoc_node())
                .collect();
            if pairs.len() != elements.elements().iter().count() {
                return vec![None, None];
            }
            vec![
                one_class(pairs.iter().map(AssocNode::key)),
                one_class(pairs.iter().map(AssocNode::value)),
            ]
        }
        // A beginless or endless range has one bound, which is enough: `(1..)` is a
        // `Range[Integer]` as plainly as `(1..9)`. Where both sides exist both are read, because
        // `(1..x)` names nothing.
        "Range" => {
            let range = node
                .as_range_node()
                .expect("a Range literal is a range node");
            vec![one_class(
                [range.left(), range.right()].into_iter().flatten(),
            )]
        }
        _ => Vec::new(),
    }
}

/// The class every one of these nodes is a literal of, or `None`.
fn one_class<'a>(nodes: impl Iterator<Item = Node<'a>>) -> Option<&'static str> {
    let mut held: Option<&'static str> = None;
    let mut seen = false;
    for node in nodes {
        seen = true;
        let class = literal_class(&node)?;
        if *held.get_or_insert(class) != class {
            return None;
        }
    }
    seen.then_some(held?)
}

/// `Foo.new` and `Foo::Bar.new`, as an offset into the constant.
///
/// Only the literal message `new`. Overriding `new` to return something else is rare. A factory
/// method with another name needs its return type, which the signature rung handles, not this
/// syntax check.
fn instantiated(node: &Node<'_>) -> Option<u32> {
    let call = node.as_call_node()?;
    if call.name().as_slice() != b"new" {
        return None;
    }
    let receiver = call.receiver()?;
    is_constant(&receiver).then(|| receiver.location().end_offset() as u32)
}

/// The name a keyword argument is written under, if the node is one.
///
/// Both Ruby spellings: `f(name: "ada")` and `f(:name => "ada")` pass the same keyword, and
/// `def f(name:)` accepts either. `value_loc` is the name without its colon. A non-symbol key
/// (`f("name" => 1)`) is a hash entry, not a keyword, and has no name.
fn keyword_name(source: &str, element: &Node<'_>) -> Option<String> {
    let key = element.as_assoc_node()?.key();
    let name = key.as_symbol_node()?.value_loc()?;
    Some(source[name.start_offset()..name.end_offset()].to_owned())
}

/// The span of a local variable read: one of the two receivers whose type can be recovered from
/// elsewhere in the file.
fn local_span(node: &Node<'_>) -> Option<(u32, u32)> {
    let read = node.as_local_variable_read_node()?;
    let location = read.location();
    Some((location.start_offset() as u32, location.end_offset() as u32))
}

/// The span of an instance variable read, `@` included, as [`scopes`] spans one.
fn instance_span(node: &Node<'_>) -> Option<(u32, u32)> {
    let read = node.as_instance_variable_read_node()?;
    let location = read.location();
    Some((location.start_offset() as u32, location.end_offset() as u32))
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {

    use super::*;

    /// Classify the cursor written as `~` in the fixture; the `~` is removed before parsing.
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

    /// [`self_call`]'s shape with arguments. The count comes from the list, not a separate
    /// parameter, so the two cannot disagree.
    fn self_call_with(at: u32, written: Vec<Receiver>, name: &str) -> Receiver {
        Receiver::Returned {
            on: Box::new(Receiver::SelfObject(at)),
            method: name.to_owned(),
            block: Block::None,
            arity: Arity::Exactly(written.len() as u32),
            arguments: written,
        }
    }

    /// An integer literal's shape, the argument most tests here use.
    fn integer() -> Receiver {
        Receiver::Literal {
            class: "Integer",
            arguments: Vec::new(),
        }
    }

    /// The shape of a bare `name` with no receiver: the lookup on `self`, with the method's own
    /// spelling kept underneath for the rung below.
    ///
    /// `at` is where the call is written, where its implicit `self` stands (see
    /// [`Receiver::SelfObject`]). Each is a byte offset into the test's source with the `~`
    /// removed, so a source that gains a line before the call must move it.
    fn self_call(at: u32, name: &str) -> Receiver {
        Receiver::Spelled {
            was: Box::new(Receiver::Returned {
                on: Box::new(Receiver::SelfObject(at)),
                method: name.to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
            }),
            name: name.to_owned(),
        }
    }

    /// The shape a variable's assignment produced, with the fall-through spelling peeled off.
    ///
    /// Every assignment-typed variable is a [`Receiver::Spelled`]: the shape, plus the name to try
    /// if the shape fails. Nearly every test here is about the shape alone;
    /// `a_typed_variable_still_carries_the_name_it_is_written_as` pins the wrapper once, so other
    /// assertions each test one thing.
    fn typed(marked: &str) -> Receiver {
        match receiver(marked) {
            Receiver::Spelled { was, .. } => *was,
            other => panic!("expected a variable an assignment typed, got {other:?}"),
        }
    }

    #[test]
    fn nothing_written_after_the_cursor_is_something_the_cursor_is_inside() {
        // Every span tested here has two bounds, and a fixture written *around* the cursor only
        // exercises the upper one. A comment, a `::` and a literal all beginning after the offset
        // must leave the classification alone; missing the lower bound would silence completion on
        // the first line of any file with a string further down.
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
        // `foo.()` is `foo.call()` with no name: the only shape where Prism gives a call operator
        // without a message. Neither half of `visit_call_node` may claim it: there is no name to
        // replace, and no signature for the parentheses to be arguments of.
        assert!(
            matches!(context("foo.(~)"), Context::Expression),
            "the parentheses of a `.()` call are not an argument list we know the callee of"
        );
    }

    #[test]
    fn a_comment_ends_at_its_line_and_code_after_it_is_code() {
        // Both bounds of the same test. Completion must not fire inside a comment, and must fire
        // again on the next line. The check includes the end because a comment's span stops at its
        // last character and the cursor parks past it.
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
        // The one place this module can be confidently wrong, so the rule is stated: the textually
        // last preceding assignment whose value ends before the cursor, skipping assignments whose
        // value has no knowable type.
        assert_eq!(typed("x = 1\nx.~"), Receiver::literal("Integer"));
        assert_eq!(
            typed("x = whatever\nx = \"s\"\nx.~"),
            Receiver::literal("String"),
            "an untypeable assignment is not the answer when a typed one exists"
        );
        assert_eq!(
            typed("x = 1\nother = \"s\"\nx.~"),
            Receiver::literal("Integer"),
            "and neither is a later assignment to a different name"
        );
        assert_eq!(
            typed("x = \"s\"\nx = whatever\nx.~"),
            Receiver::literal("String"),
            "and it does not erase one either"
        );
        assert_eq!(
            receiver("x = whatever\nx.~"),
            Receiver::Spelled {
                was: Box::new(self_call(4, "whatever")),
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
        // The bound that lets `"foo".` complete at all: the closing quote is where the next `.` is
        // typed, and everything after the literal is ordinary code.
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
            // Interpolated forms are the same classes: `"a#{b}"` is a String however it was
            // assembled, and Prism gives each form its own node.
            ("`ls #{dir}`.~", "String"),
            (r#":"a#{b}".~"#, "Symbol"),
            ("/re#{x}/.~", "Regexp"),
            ("__ENCODING__.~", "Encoding"),
        ] {
            assert_eq!(
                receiver(source),
                match class {
                    // The three generic literals and their contents. This test is about the
                    // *class*; `what_a_literal_was_written_holding` is about the arguments.
                    "Array" => holding("Array", &[Some("Integer")]),
                    "Hash" => holding("Hash", &[Some("Symbol"), Some("Integer")]),
                    "Range" => holding("Range", &[Some("Integer")]),
                    _ => Receiver::literal(class),
                },
                "classifying {source:?}"
            );
        }
    }

    /// A literal carrying what it holds, for tests about something else.
    fn holding(class: &'static str, arguments: &[Option<&'static str>]) -> Receiver {
        Receiver::Literal {
            class,
            arguments: arguments.to_vec(),
        }
    }

    #[test]
    fn what_a_literal_was_written_holding_is_read_off_the_source_and_never_guessed() {
        // The three classes RBS gives a type parameter, the only three a literal can carry anything
        // in. Not inferred: the parser decided `1` is an `Integer` as it decided `[1]` is an
        // `Array`.
        assert_eq!(receiver("[1, 2].~"), holding("Array", &[Some("Integer")]));
        assert_eq!(receiver("%w[a b].~"), holding("Array", &[Some("String")]));
        assert_eq!(receiver("%i[a b].~"), holding("Array", &[Some("Symbol")]));
        assert_eq!(
            receiver("{ a: 1, b: 2 }.~"),
            holding("Hash", &[Some("Symbol"), Some("Integer")])
        );
        assert_eq!(receiver("(1..9).~"), holding("Range", &[Some("Integer")]));
        // One bound is enough: `(1..)` is a `Range[Integer]` as plainly as `(1..9)`.
        assert_eq!(receiver("(1..).~"), holding("Range", &[Some("Integer")]));
        // The inner literal's own class; what *it* holds is dropped. A `.` on the element reaches
        // `Array`'s members whatever is inside.
        assert_eq!(receiver("[[1], [2]].~"), holding("Array", &[Some("Array")]));

        // Every element or nothing at that position. A mixed literal is a union; an empty one says
        // nothing; a splat is a value from elsewhere; a name needs the graph, not this module.
        assert_eq!(receiver("[1, \"a\"].~"), holding("Array", &[None]));
        assert_eq!(receiver("[].~"), holding("Array", &[None]));
        assert_eq!(receiver("[1, *rest].~"), holding("Array", &[None]));
        assert_eq!(receiver("[story, other].~"), holding("Array", &[None]));
        // A hash answers each side separately, the common case: symbol keys, values whatever the
        // configuration needed.
        assert_eq!(
            receiver("{ a: 1, b: x }.~"),
            holding("Hash", &[Some("Symbol"), None])
        );
        // `**rest` has neither half, so a hash holding one names neither side.
        assert_eq!(
            receiver("{ a: 1, **rest }.~"),
            holding("Hash", &[None, None])
        );
        // Every other literal is generic over nothing and carries nothing: the same absence,
        // deliberately spelled the same.
        assert_eq!(receiver("\"hello\".~"), Receiver::literal("String"));
        assert_eq!(receiver("nil.~"), Receiver::literal("NilClass"));
    }

    #[test]
    fn a_decimal_point_is_not_a_method_call() {
        // `4.2` is one literal, and the cursor after it completes on a Float, not on an Integer `4`
        // with a message. A backwards text scan gets this wrong.
        assert_eq!(receiver("4.2.~"), Receiver::literal("Float"));
    }

    #[test]
    fn new_gives_an_instance_rather_than_the_class() {
        // The offset points inside the constant, where the graph files the resolved reference, as
        // for `Receiver::Constant`.
        assert_eq!(receiver("Person.new.~"), Receiver::Instance(6));
        assert_eq!(receiver("HR::Person.new.~"), Receiver::Instance(10));
        assert_eq!(receiver("Person.new(1, 2).~"), Receiver::Instance(6));
        // The class itself is still the class.
        assert_eq!(receiver("Person.~"), Receiver::Constant(6));
    }

    #[test]
    fn only_new_makes_an_instance_and_every_other_call_is_a_chain() {
        // `new` is the one factory this module names alone. Other factories become the *shape* of a
        // lookup, which the graph side may or may not answer. Nothing here decides `Person.build`
        // returns a `Person`, only that the question is about `Person`'s singleton.
        assert_eq!(
            receiver("Person.build.~"),
            Receiver::Returned {
                on: Box::new(Receiver::Constant(6)),
                method: "build".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
            }
        );
        // `person` is an unassigned bare name, which Ruby reads as `self.person`, so the chain runs
        // through it, with the name underneath as the fallback rung. Nothing is decided here about
        // either.
        assert_eq!(
            receiver("person.new.~"),
            Receiver::Returned {
                on: Box::new(self_call(0, "person")),
                method: "new".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
            }
        );
    }

    #[test]
    fn a_multiple_assignment_gives_each_target_the_position_it_was_written_at() {
        // One value spread across several names. The shape must carry *which* name, because the
        // type is a position in the value, not the value.
        assert_eq!(
            receiver("read_io, write_io = IO.pipe\nwrite_io.~"),
            Receiver::Spelled {
                was: Box::new(Receiver::Destructured {
                    of: Box::new(Receiver::Returned {
                        on: Box::new(Receiver::Constant(22)),
                        method: "pipe".to_owned(),
                        block: Block::None,
                        arity: Arity::Exactly(0),
                        arguments: Vec::new(),
                    }),
                    index: 1,
                }),
                name: "write_io".to_owned(),
            }
        );

        // **A written list is not a destructure.** `a, b = foo, bar` gives each name its own
        // element, exactly, so no index travels and the shape is what `b = bar` would give.
        assert_eq!(
            receiver("a, b = Person.new, Widget.new\nb.~"),
            Receiver::Spelled {
                was: Box::new(Receiver::Instance(25)),
                name: "b".to_owned(),
            }
        );

        // A `*rest` fixes no position after it, so the whole assignment is refused (see
        // `visit_multi_write_node`).
        assert_eq!(
            receiver("head, *rest = IO.pipe\nhead.~"),
            Receiver::Named("head".to_owned())
        );

        // **A non-local target is skipped; its neighbours are not.** `@held` is an instance
        // variable, collected by `instance_writes` under its own span; counting it here would file
        // an ivar's type under a local's name.
        assert_eq!(
            receiver("kept, @held = IO.pipe\nkept.~"),
            Receiver::Spelled {
                was: Box::new(Receiver::Destructured {
                    of: Box::new(Receiver::Returned {
                        on: Box::new(Receiver::Constant(16)),
                        method: "pipe".to_owned(),
                        block: Block::None,
                        arity: Arity::Exactly(0),
                        arguments: Vec::new(),
                    }),
                    index: 0,
                }),
                name: "kept".to_owned(),
            }
        );

        // A write ending *after* the cursor cannot answer it: `visit_local_variable_write_node`'s
        // guard, and why `at` is the end of the **value**, not the name. Not asserted through
        // `receiver`: a trailing `.` alone on a line merges with what follows, so the fixture would
        // test Prism's error recovery instead.
    }

    #[test]
    fn a_destructured_target_rebases_into_the_graph_by_its_value_alone() {
        // The index is a position left of an `=`, not in any text, so it travels unchanged while
        // the indexed value moves like any shape.
        let shape = Receiver::Destructured {
            of: Box::new(Receiver::Constant(10)),
            index: 3,
        };
        assert_eq!(
            shape.rebased(&Rebase::identity(64)),
            Some(Receiver::Destructured {
                of: Box::new(Receiver::Constant(10)),
                index: 3,
            })
        );
    }

    #[test]
    fn a_chain_carries_how_many_positional_arguments_the_call_wrote() {
        // The companion to `block`. Nothing here decides `first(3)` returns an `Array`; only that
        // the question is about a one-argument call, which differs from a zero-argument one.
        let arity = |source: &str| match receiver(source) {
            Receiver::Returned { arity, .. } => arity,
            other => panic!("{other:?}"),
        };
        assert_eq!(arity("[1, 2].first.~"), Arity::Exactly(0));
        assert_eq!(arity("[1, 2].first(3).~"), Arity::Exactly(1));
        assert_eq!(arity("\"x\".sub(\"a\", \"b\").~"), Arity::Exactly(2));
        // Keywords are not positional, and RBS counts them apart too: `3.7.round` and
        // `3.7.round(half: :up)` reach the same zero-argument arm.
        assert_eq!(arity("3.7.round(half: :up).~"), Arity::Exactly(0));
        assert_eq!(arity("3.7.round(1, half: :up).~"), Arity::Exactly(1));
        // `**opts` is the same fact. Prism reads a bare `k => v` tail as keywords whatever the keys
        // (Ruby 3's rule). Braces make it a positional `Hash`, which counts.
        assert_eq!(arity("f.g(**opts).~"), Arity::Exactly(0));
        assert_eq!(arity("f.g(\"a\" => 1).~"), Arity::Exactly(0));
        assert_eq!(arity("f.g({ \"a\" => 1 }).~"), Arity::Exactly(1));
        // A block argument is a block, not an argument; `block` already says so.
        assert_eq!(arity("f.map(&:upcase).~"), Arity::Exactly(0));
        // An uncountable count is stated, not guessed. Guessing low would pick the arm with the
        // fewest parameters: the "nearest arm" the partition refuses.
        assert_eq!(arity("f.g(*args).~"), Arity::Unknown);
        assert_eq!(arity("f.g(1, *rest).~"), Arity::Unknown);
        assert_eq!(arity("def wrap(...)\n  f.g(...).~\nend\n"), Arity::Unknown);
    }

    #[test]
    fn a_chain_is_the_receiver_it_was_written_on_and_the_name_it_called() {
        assert_eq!(
            receiver("\"hi\".upcase.~"),
            Receiver::Returned {
                on: Box::new(Receiver::literal("String")),
                method: "upcase".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
            }
        );
        // Chains compose innermost first, and the shape gives the resolution order.
        assert_eq!(
            receiver("\"hi\".upcase.strip.~"),
            Receiver::Returned {
                on: Box::new(Receiver::Returned {
                    on: Box::new(Receiver::literal("String")),
                    method: "upcase".to_owned(),
                    block: Block::None,
                    arity: Arity::Exactly(0),
                    arguments: Vec::new(),
                }),
                method: "strip".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
            }
        );
    }

    #[test]
    fn an_argument_list_past_the_width_is_no_claim_rather_than_a_short_one() {
        // One shape per counted argument or nothing, never a prefix: positions after a dropped
        // argument would be miscounted, and the consumer needs the whole list to pick an arm. So
        // the count stays exact and the shapes go.
        let many = (1..=(MAX_WIDTH + 1))
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let Receiver::Returned {
            arity, arguments, ..
        } = receiver(&format!("f.g({many}).~"))
        else {
            panic!("a chain");
        };
        assert_eq!(arity, Arity::Exactly(MAX_WIDTH as u32 + 1));
        assert!(arguments.is_empty(), "{arguments:?}");
    }

    #[test]
    fn one_unknown_ends_the_chain_rather_than_being_carried_up_it() {
        // `x.()` is `x.call` with no name, so there is nothing to look up, and a `Returned` around
        // an `Unknown` would only make the graph side rediscover that.
        assert_eq!(receiver("x.().foo.~"), Receiver::Unknown);
        // Two shapes are deliberately off that list. `thing(1)` and `thing { }` are calls on an
        // implicit `self`, like a bare `thing`, and asking the graph what they return is no guess.
        // What their *spelling* cannot support is the name rung below, which is all this guard
        // bounds. So the chain carries the lookup, with no name underneath.
        assert_eq!(
            receiver("thing(1).foo.~"),
            Receiver::Returned {
                on: Box::new(Receiver::Returned {
                    on: Box::new(Receiver::SelfObject(0)),
                    method: "thing".to_owned(),
                    block: Block::None,
                    arity: Arity::Exactly(1),
                    arguments: vec![integer()],
                }),
                method: "foo".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
            }
        );
        let Receiver::Returned { on, .. } = receiver("thing { }.foo.~") else {
            panic!("a chain");
        };
        assert_eq!(
            *on,
            Receiver::Returned {
                on: Box::new(Receiver::SelfObject(0)),
                method: "thing".to_owned(),
                // An empty block returns `nil`, as Ruby says: `[1, 2].map { }` is `[nil, nil]`. An
                // answer, not a missing one.
                block: Block::Written(Box::new([Receiver::literal("NilClass")])),
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
            },
            "and the block the call wrote is carried, because RBS tells the arms apart by it"
        );
        // A *bare* call keeps both: the lookup, and the name beneath it that the two lower rungs
        // read. `@user.name.` is the expression this is for.
        assert_eq!(
            receiver("thing.foo.bar.~"),
            Receiver::Returned {
                on: Box::new(Receiver::Returned {
                    on: Box::new(self_call(0, "thing")),
                    method: "foo".to_owned(),
                    block: Block::None,
                    arity: Arity::Exactly(0),
                    arguments: Vec::new(),
                }),
                method: "bar".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
            }
        );
        assert_eq!(receiver("build.~"), self_call(0, "build"));
    }

    /// The block's own value, carried beside the fact that a block was written.
    ///
    /// Shapes only; what `map` does with them is [`types`](super::types)' concern.
    #[test]
    fn a_block_carries_what_it_hands_back() {
        let block = |marked: &str| match receiver(marked) {
            Receiver::Returned { block, .. } => block,
            other => panic!("expected a call, got {other:?}"),
        };
        // The tail, read as a shape like any other.
        assert_eq!(
            block("[1].map { |n| \"x\" }.~"),
            Block::Written(Box::new([Receiver::literal("String")]))
        );
        // A tail-position conditional is one exit per branch, and an unwritten branch is the `nil`
        // Ruby really returns ([`Exits::tail`], shared unchanged).
        assert_eq!(
            block("[1].map { |n| \"x\" if n }.~"),
            Block::Written(Box::new([
                Receiver::literal("String"),
                Receiver::literal("NilClass"),
            ]))
        );
        // An empty block returns `nil`: `[1, 2].map { }` is `[nil, nil]`.
        assert_eq!(
            block("[1].map { }.~"),
            Block::Written(Box::new([Receiver::literal("NilClass")]))
        );
        // **A `next` or `break` refuses the whole block**: neither is in tail position, and reading
        // only the tail would be a confident half-answer.
        assert_eq!(
            block("[1].map { |n| next 1 if n\n \"x\" }.~"),
            Block::Written(Box::new([Receiver::Unknown]))
        );
        assert_eq!(
            block("[1].each { |n| break if n\n \"x\" }.~"),
            Block::Written(Box::new([Receiver::Unknown]))
        );
        // `&:upcase` and a forwarded `&blk` are blocks Ruby passes with no body in this file: the
        // arm applies, with nothing to read.
        assert_eq!(block("[1].map(&:to_s).~"), Block::Written(Box::default()));
        // No block at all.
        assert_eq!(block("[1].first.~"), Block::None);
    }

    #[test]
    fn a_local_assigned_a_call_carries_the_call() {
        // `type_the_local` goes beyond literals and `.new`: it returns whatever shape the assigned
        // expression has.
        assert_eq!(
            typed("shouted = \"hi\".upcase\nshouted.~\n"),
            Receiver::Returned {
                on: Box::new(Receiver::literal("String")),
                method: "upcase".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
            }
        );
    }

    #[test]
    fn a_local_assigned_from_itself_terminates() {
        // `x = x.foo` reads a local inside its own write. Only assignments whose value ends before
        // the *read* count, so the write cannot answer its inner read, and the recursion ends
        // there, not at the width limit. It ends *at* the name: one link, not a fixpoint.
        assert_eq!(
            typed("x = x.foo\nx.~\n"),
            Receiver::Returned {
                on: Box::new(Receiver::Named("x".to_owned())),
                method: "foo".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
            }
        );
        // The inner `x` is itself assignment-typed, so it arrives wrapped: the fall-through is per
        // variable and nests where variables do.
        assert_eq!(
            typed("x = \"hi\"\nx = x.upcase\nx.~\n"),
            Receiver::Returned {
                on: Box::new(Receiver::Spelled {
                    was: Box::new(Receiver::literal("String")),
                    name: "x".to_owned(),
                }),
                method: "upcase".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
            }
        );
    }

    #[test]
    fn a_call_is_named_by_what_ruby_looks_up_and_not_by_what_was_typed() {
        // The two punctuation shapes, where the message span and method name differ. Reading the
        // span would send an index to the table as `[key]` and a negation as `-`, so `Hash#[]` and
        // `Integer#-@` could never be asked for.
        let named = |marked: &str| match receiver(marked) {
            Receiver::Returned { method, arity, .. } => format!("{method}/{arity:?}"),
            other => format!("{other:?}"),
        };
        assert_eq!(named("rows[key].~\n"), "[]/Exactly(1)");
        assert_eq!(named("(-count).~\n"), "-@/Exactly(0)");
        // The span still says when there is *no* name: a call Prism recovered with no message span
        // is named `call`, and must stop here anyway.
        assert_eq!(receiver("handler.().~\n"), Receiver::Unknown);
    }

    #[test]
    fn a_chain_is_bounded_rather_than_followed_to_the_end() {
        // A chain is not a fixpoint: this runs on the analysis thread per keystroke. Past
        // `MAX_WIDTH` links the answer is `Unknown`, like any unanswerable receiver. **This axis
        // can be generous**: one question per link, no candidate list under any of them.
        let long = format!("\"hi\"{}.~", ".upcase".repeat(MAX_WIDTH + 2));
        let mut links = 0;
        let mut at = &receiver(&long);
        while let Receiver::Returned { on, .. } = at {
            links += 1;
            at = on;
        }
        assert_eq!(*at, Receiver::Unknown);
        assert!(links < MAX_WIDTH, "{links} links");
    }

    #[test]
    fn a_local_takes_the_type_of_what_was_assigned_to_it() {
        assert_eq!(
            typed("name = \"ada\"\nname.~\n"),
            Receiver::literal("String")
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
            holding("Array", &[Some("Integer")])
        );
        // An assignment *after* the cursor is not in scope yet, whatever the parser saw.
        assert_eq!(
            typed("x = \"a\"\nx.~\nx = [1]\n"),
            Receiver::literal("String")
        );
    }

    #[test]
    fn a_local_assigned_something_untypable_keeps_only_its_name() {
        // Two names, one below the other, both surviving the `self` rung. The assignment's value is
        // a lookup (`self.compute`), and the local's own spelling is still tried last, so a
        // workspace with no `compute` gives the name-rung answer.
        assert_eq!(
            receiver("x = compute\nx.~\n"),
            Receiver::Spelled {
                was: Box::new(self_call(4, "compute")),
                name: "x".to_owned(),
            }
        );
        // Never assigned: a bare word, which Prism reads as a receiverless call, so it has the
        // *call's* shape with the same spelling underneath.
        assert_eq!(receiver("x.~\n"), self_call(0, "x"));
        // `x = x.` must not type `x` from the half-written statement it is part of. It stays a bare
        // `Named` and the `self` rung does not reach it: the assignment makes `x` a *local* to
        // Prism, so this is the local's fall-through, never a call on `self`.
        assert_eq!(receiver("x = x.~\n"), Receiver::Named("x".to_owned()));
    }

    #[test]
    fn a_typed_variable_still_carries_the_name_it_is_written_as() {
        // The wrapper every other test peels, pinned once. Both halves must be present: the shape,
        // so a typing chain is answered from the code; and the spelling, so a failing chain reaches
        // the same rung a bare name does.
        assert_eq!(
            receiver("story = Story.where(x).first\nstory.~\n"),
            Receiver::Spelled {
                was: Box::new(Receiver::Returned {
                    on: Box::new(Receiver::Returned {
                        on: Box::new(Receiver::Constant(13)),
                        method: "where".to_owned(),
                        block: Block::None,
                        arity: Arity::Exactly(1),
                        // The argument is a shape like any other: a bare `x` is a receiverless call
                        // here as everywhere.
                        arguments: vec![self_call(20, "x")],
                    }),
                    method: "first".to_owned(),
                    block: Block::None,
                    arity: Arity::Exactly(0),
                    arguments: Vec::new(),
                }),
                name: "story".to_owned(),
            }
        );
        // An instance variable wraps the *whole* `Assigned`, so a typing chain keeps the note
        // naming its assignment line, and only a failing chain reaches the name.
        assert_eq!(
            receiver(
                "class C\n  def a\n    @story = fetch.first\n  end\n  def b\n    @story.~\n  end\nend\n"
            ),
            Receiver::Spelled {
                was: Box::new(Receiver::Assigned {
                    at: 20,
                    was: Box::new(Receiver::Returned {
                        on: Box::new(self_call(29, "fetch")),
                        method: "first".to_owned(),
                        block: Block::None,
                        arity: Arity::Exactly(0),
                        arguments: Vec::new(),
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
        // An instance variable is typed from an assignment in its own class, and `scopes` already
        // decides which `@foo` is which, so it is cheap.
        assert_eq!(
            assigned(
                "class Person\n  def initialize\n    @name = \"ada\"\n  end\n\n  def shout\n    @name.~\n  end\nend\n"
            ),
            (34, Receiver::literal("String"))
        );
        // The assignment can be *below* the reading method, which is why "textually last" is not
        // "last before the cursor", and why the half-typed `.` must be blanked first. Without that
        // repair Prism reads the `end` below the cursor as the method name, reparents the rest of
        // the class into a nested `def`, and this `@name` becomes a different variable with a
        // different `self`.
        assert_eq!(
            assigned("class Person\n  def shout\n    @name.~\n  end\n\n  def initialize\n    @name = \"ada\"\n  end\nend\n").1,
            Receiver::literal("String")
        );
        // The other half-typed shape: a word already begun. The message blanks with the operator,
        // because it ends at the cursor, not past it.
        assert_eq!(
            assigned("class Person\n  def shout\n    @name.up~\n  end\n\n  def initialize\n    @name = \"ada\"\n  end\nend\n").1,
            Receiver::literal("String")
        );
    }

    #[test]
    fn an_instance_variable_in_another_self_is_a_different_variable() {
        // `@v` in `def a` and `@v` in `def self.b` belong to different objects. Joining them would
        // be confidently wrong, worse than `Unknown`, and the rule is `scopes`', asked, not
        // re-derived.
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
        // `type_the_local`'s caveat, for a second shape: a wrong answer rather than a missing one,
        // which is why the provenance footnote exists.
        let (_, was) = assigned(
            "class Person\n  def a\n    @v = \"s\"\n  end\n  def b\n    @v = 1\n  end\n  def c\n    @v.~\n  end\nend\n",
        );
        assert_eq!(was, Receiver::literal("Integer"));
    }

    #[test]
    fn a_memoised_instance_variable_is_an_assignment() {
        // `@cache ||= …` is Ruby's memoisation and as much an assignment as `=`. `@n += 1` is not
        // and stays unknown: an operator write says what happens to a value, not what it is.
        assert_eq!(
            assigned("class C\n  def cache\n    @cache ||= \"x\"\n  end\n  def use\n    @cache.~\n  end\nend\n").1,
            Receiver::literal("String")
        );
        assert_eq!(
            receiver("class C\n  def bump\n    @n += 1\n  end\n  def use\n    @n.~\n  end\nend\n"),
            Receiver::Named("@n".to_owned())
        );
    }

    #[test]
    fn what_a_constant_was_assigned_is_found_by_the_span_of_its_name() {
        // The four shapes rubydex files a `Definition::Constant` for, keyed as it keys them: the
        // span of the written name, which for a path is the **last segment alone**. A name match
        // would answer `Vault::HANDLE` for a `HANDLE` elsewhere; a span cannot.
        let source = "\
HOLDER = Vault::Store.new
MEMO ||= \"x\"
Vault::HANDLE = Vault::Store.new
Vault::KEPT ||= 1
GUARD &&= Vault::Store.new
";
        let at = |needle: &str| {
            let start = source.find(needle).expect("needle") as u32;
            (start, start + needle.len() as u32)
        };
        // `Klass.new` is its own shape, not a call whose return must be looked up, so the answer
        // names the class, not the method.
        let built = |nth: usize| {
            let (start, found) = source.match_indices("Vault::Store").nth(nth).unwrap();
            Some(Receiver::Instance((start + found.len()) as u32))
        };

        assert_eq!(constant_assignment(source, at("HOLDER")), built(0));
        assert_eq!(
            constant_assignment(source, at("MEMO")),
            Some(Receiver::literal("String"))
        );
        // The path forms, found by their last segment, not the whole path.
        assert_eq!(constant_assignment(source, at("HANDLE")), built(1));
        assert_eq!(
            constant_assignment(source, at("KEPT")),
            Some(Receiver::literal("Integer"))
        );

        // **`&&=` is not one of the four**, and cannot be reached anyway: rubydex records a
        // reference for it and no definition, so no caller holds its span. Asked directly, it
        // answers nothing rather than reading the value beside it.
        assert_eq!(constant_assignment(source, at("GUARD")), None);
        // A span naming nothing in this text: what an edit since the last index looks like from
        // here.
        assert_eq!(constant_assignment(source, (900, 906)), None);
    }

    #[test]
    fn an_instance_variable_assigned_from_itself_terminates() {
        // `@v = @v.foo` reads the variable inside its own write, which cannot answer that read.
        assert_eq!(
            receiver("class C\n  def a\n    @v = @v.~\n  end\nend\n"),
            Receiver::Named("@v".to_owned())
        );
    }

    #[test]
    fn an_instance_variable_assigned_from_itself_many_times_is_typed_by_the_one_that_can() {
        // The wide case the single-assignment test misses. Every write of `@v` is a candidate for
        // every read, and each `@v = @v.foo` reads `@v` again. The width bounds how often that is
        // asked, not how wide one ask is. This is the shape of discourse's `lib/topics_filter.rb`,
        // which assigns `@scope` sixty times, mostly from itself.
        //
        // Without the guard this test **hangs** rather than fails, which is why the assertion is
        // about the answer, not a duration. The answer matters too: refusing re-entry must not lose
        // the one write that can type the variable.
        let mut source = String::from("class Person\n  def initialize\n    @name = \"ada\"\n");
        for _ in 0..12 {
            source.push_str("    @name = @name.strip\n");
        }
        source.push_str("  end\n\n  def shout\n    @name.~\n  end\nend\n");
        assert_eq!(assigned(&source).1, Receiver::literal("String"));
    }

    #[test]
    fn an_instance_variable_nothing_assigned_keeps_only_its_name() {
        assert_eq!(
            receiver("class C\n  def a\n    @v.~\n  end\nend\n"),
            Receiver::Named("@v".to_owned())
        );
        // Assigned a receiverless call, which is `self.compute`, and every older rung is still
        // underneath in order: the assignment's own name, then (when `method_receiver` returns
        // nothing for the whole `Assigned`) the variable's, which the guess uses.
        assert_eq!(
            receiver("class C\n  def a\n    @v = compute\n  end\n  def b\n    @v.~\n  end\nend\n"),
            Receiver::Spelled {
                was: Box::new(Receiver::Assigned {
                    at: 20,
                    was: Box::new(self_call(25, "compute")),
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
                receiver: Receiver::literal("String")
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
        // The half-written form matters most: `HR::` does not parse, and Prism's recovery puts a
        // zero-width name exactly at the cursor.
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
        // This module still guesses nothing: the two `Spelled` names are spellings, not types.
        // Whether a `p` can be a `P` is for the graph, a rung down, and a setting can turn it off.
        // The `self` rung adds a *lookup* above them, not a guess.
        assert_eq!(
            context("p = build_person\np.~\n"),
            Context::MethodCall {
                receiver: Receiver::Spelled {
                    was: Box::new(self_call(4, "build_person")),
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
                receiver: Receiver::SelfObject(0)
            }
        );
    }

    #[test]
    fn a_block_parameter_carries_the_call_that_hands_it_over() {
        // The block-parameter shape, with nothing resolved: it records *which call* the block was
        // written on and *which* parameter this is. What the signature says is `types`'.
        assert_eq!(
            context("Story.where(id: 1).each do |story|\n  story.~\nend\n"),
            Context::MethodCall {
                receiver: Receiver::Spelled {
                    was: Box::new(Receiver::Yielded {
                        on: Box::new(Receiver::Returned {
                            on: Box::new(Receiver::Constant(5)),
                            method: "where".to_owned(),
                            block: Block::None,
                            arity: Arity::Exactly(0),
                            arguments: Vec::new(),
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
        // The one place this walk cares about nesting. Shadowing a block parameter is legal, and a
        // read in the inner block means the inner one. (`type_the_local`, by contrast, treats a
        // block's `x` and the outer `x` as one variable, as they usually are.)
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
        assert_eq!(*on, self_call(16, "b"));
    }

    #[test]
    fn a_block_on_a_call_with_no_receiver_is_asked_of_self() {
        // `each { |x| }` with no receiver is an implicit `self`, which is a question, not a dead
        // end: the parameter is whatever `self.each` says it yields. Where nothing declares it (a
        // `yield` in the method's own body is not a declaration), the `Spelled` underneath is the
        // name rung.
        assert_eq!(
            context("each do |story|\n  story.~\nend\n"),
            Context::MethodCall {
                receiver: Receiver::Spelled {
                    was: Box::new(Receiver::Yielded {
                        on: Box::new(Receiver::SelfObject(0)),
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
        // Mastodon's `lib/paperclip/color_extractor.rb`. `max_distance_color = nil` above the loop
        // types it, and `max_distance_color = color` inside the loop is the *later* write, so on
        // position alone it would win and answer nothing, because nothing declares what
        // `palette.each` hands its block. A relayed block parameter is a disguised fallback, taken
        // only when no other write produced a shape.
        let Context::MethodCall {
            receiver: Receiver::Spelled { was, .. },
        } = context("best = nil\npalette.each do |color|\n  best = color\nend\nbest.~\n")
        else {
            panic!("a method call on a local");
        };
        assert!(
            matches!(*was, Receiver::Literal { .. }),
            "the `nil` above the loop lost to a block parameter that types nothing: {was:?}"
        );
    }

    #[test]
    fn an_assignment_rooted_in_a_call_on_self_never_displaces_one_that_typed() {
        // The precedence arm. A receiverless call may resolve to nothing (`self` in a spec, a rake
        // task or a top-level script is `Object`), so a write whose value is one must not take the
        // answer from a write that produced a type.
        assert_eq!(
            typed("x = \"s\"\nx = whatever\nx.~"),
            Receiver::literal("String"),
            "a bare call on `self`"
        );
        assert_eq!(
            typed("x = \"s\"\nx = whatever(1)\nx.~"),
            Receiver::literal("String"),
            "and one that wrote an argument, which carries no name under it at all"
        );
        // **Through the whole chain.** `tokens = user_tokens(a) + contact(b)` is a call on a call
        // on `self`; asked only about its last link it looks as solid as `Foo.bar.baz`, and would
        // displace `tokens = [x]` in the method above.
        assert_eq!(
            typed("x = [1]\nx = one(2) + two(3)\nx.~"),
            holding("Array", &[Some("Integer")]),
            "a chain rooted in a call on `self` is rooted in one however long it is"
        );
        // Still a *precedence*: with no other write, the chain is taken.
        assert!(
            matches!(typed("x = whatever(1)\nx.~"), Receiver::Returned { .. }),
            "the only write there is has to be taken"
        );
        // Through calls, never through a **variable**: in `link = c.links.last`, where `c` is
        // itself a call on `self`, the chain is `c`'s problem, and `c` has its own name rung.
        // Treating it as rooted would let a String literal in another `it` block take `link`.
        assert_eq!(
            typed("l = \"s\"\nc = make(1)\nl = c.rows.last\nl.~"),
            Receiver::Returned {
                on: Box::new(Receiver::Returned {
                    on: Box::new(Receiver::Spelled {
                        was: Box::new(self_call_with(12, vec![integer()], "make")),
                        name: "c".to_owned(),
                    }),
                    method: "rows".to_owned(),
                    block: Block::None,
                    arity: Arity::Exactly(0),
                    arguments: Vec::new(),
                }),
                method: "last".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
            }
        );
        // Ranked below the block parameter too, the third slot's reason to exist:
        // `uploader = upload_subforem_image(a, b)` in one method and
        // `Uploader.new.tap do |uploader|` in the next, which `type_the_local` treats as one
        // variable.
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
        // The other half: a *precedence*, not a refusal, so a variable whose only write is a block
        // parameter still carries the shape.
        let Context::MethodCall {
            receiver: Receiver::Spelled { was, .. },
        } = context("stories.each do |story|\n  row = story\n  row.~\nend\n")
        else {
            panic!("a method call on a local");
        };
        assert!(
            relays_a_parameter(&was),
            "the only write there is has to be taken: {was:?}"
        );
    }

    #[test]
    fn a_name_read_outside_the_block_it_is_a_parameter_of_is_not_one() {
        // Narrower than an assignment: any write above the cursor counts for `type_the_local`, but
        // a parameter of a block the cursor is not inside means nothing.
        assert_eq!(
            context("a.each do |story|\n  story\nend\nstory.~\n"),
            Context::MethodCall {
                receiver: self_call(30, "story")
            }
        );
    }

    #[test]
    fn an_assignment_is_still_asked_before_the_block_it_is_written_in() {
        // `Receiver::Spelled`'s order, unchanged: an assignment's shape can never be displaced by
        // this, so every existing answer survives the block-parameter rung by construction.
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
        // Ruby continues an expression across a trailing `.`, so Prism reads the `end` below as the
        // method name and the cursor lands *before* the message.
        //
        // `i` is a block parameter, so the reader wraps the name in the shape asking what
        // `items.each` hands its block, and `Spelled` keeps the name rung underneath for when
        // nothing declares it.
        assert_eq!(
            context("items.each do |i|\n  i.~\nend\n"),
            Context::MethodCall {
                receiver: Receiver::Spelled {
                    was: Box::new(Receiver::Yielded {
                        on: Box::new(self_call(0, "items")),
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
        // Prism closes an unterminated call at the last token it read (the comma), so the cursor is
        // past the node unless the region steps over the separator.
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
        // Both contain the cursor; what is completed is the receiver's methods.
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
        // The code around the literal does: the next `.` is typed there, when the literal already
        // has a class.
        assert_eq!(
            context("\"text\".~\n"),
            Context::MethodCall {
                receiver: Receiver::literal("String")
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
        // `::Foo` is how Rails code says "the outer one", the only receiver absent on purpose
        // rather than unknown.
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
        // A block argument is an argument; a `do ... end` block is not, since it is written outside
        // the parentheses and a cursor in it is outside the call's arguments.
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
        // Nesting needs no special handling: the walk is pre-order, so the innermost call is the
        // last to claim the cursor.
        let inner = call("outer(1, inner(2, ~))").expect("the inner call");
        assert_eq!(inner.name, 9, "`inner`, not `outer`");
        assert_eq!(inner.active, Active::Nth(1));

        let outer = call("outer(1, inner(2, 3), ~)").expect("the outer call");
        assert_eq!(outer.name, 0);
        assert_eq!(outer.active, Active::Nth(2));
    }

    #[test]
    fn a_keyword_argument_is_named_rather_than_counted() {
        // Keywords may come in any order, so counting them answers the wrong parameter as soon as
        // someone reorders.
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
        // Ruby accepts both spellings of a keyword, so both are named.
        assert_eq!(active("f(:name => ~)"), Active::Keyword("name".to_owned()));
        // A string key is a hash entry, not a keyword, and a hash of them is one argument however
        // many pairs it holds.
        assert_eq!(active("f(\"name\" => ~)"), Active::Nth(0));
        assert_eq!(active("f(\"a\" => 1, \"b\" => 2, ~)"), Active::Nth(1));
    }

    #[test]
    fn a_keyword_hash_counts_as_its_own_elements() {
        // One Prism node, two arguments written. Without spreading, every keyword after a
        // positional would answer the same parameter.
        assert_eq!(active("f(1, a: 2, b: ~)"), Active::Keyword("b".to_owned()));
        // Past a finished keyword, before the next: which keyword is unknowable, but that it is one
        // is not, since Ruby forbids a positional after a keyword. Counting would answer a
        // parameter the call can no longer reach.
        assert_eq!(active("f(a: 1, ~)"), Active::AnyKeyword);
        assert_eq!(active("f(1, a: 2, ~)"), Active::AnyKeyword);
        assert_eq!(active("f(a: 1, b: 2, ~)"), Active::AnyKeyword);
    }

    #[test]
    fn a_heredoc_argument_ends_at_its_marker_and_not_at_its_body() {
        // `execute(<<~SQL, user_id)` is how SQL is written, with the next argument three lines
        // above the heredoc's end. The counting rule holds because Prism scopes the node to the
        // opening marker and keeps the body separately. A test, not a comment, because the obvious
        // reading of "where the node ends" would put every later argument inside the first. (`<<-`
        // rather than `<<~` only because `~` is also the cursor marker.)
        for body in ["  body\n", "  body #{x}\n"] {
            let marked = format!("f(<<-TEXT, ~)\n{body}  TEXT\n");
            assert_eq!(active(&marked), Active::Nth(1), "{body:?}");
        }
        // The cursor on the marker itself is still the first argument.
        assert_eq!(active("f(<<-TEXT~, 2)\n  body\n  TEXT\n"), Active::Nth(0));
    }

    #[test]
    fn a_call_with_nothing_to_resolve_is_not_a_call() {
        // `foo.()` is `foo.call()` with no name: no callee to look up, so no signature to show.
        assert_eq!(call("foo.(~)"), None);
        assert_eq!(call("~"), None, "nowhere near a call");
        assert_eq!(call("f(1) ~"), None, "past the closing paren");
    }

    #[test]
    fn a_literal_and_a_comment_end_completion_and_not_the_signature() {
        // The two places `at` deliberately gives up. Editors keep the signature popup up through
        // both, and `null` makes it flicker per keystroke.
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
        // The completion half is `an_operator_beats_the_argument_list_it_is_written_in`: the cursor
        // completes `Person`'s methods, and the call it passes an argument to is still `build`.
        let found = call("build(Person.~").expect("the enclosing call");
        assert_eq!(found.name, 0);
        assert_eq!(found.active, Active::Nth(0));
    }

    /// [`macro_symbol`] at the `~`, as `macro name` or the reason there is none.
    fn macro_at(marked: &str) -> String {
        let offset = marked.find('~').expect("a ~ marking the cursor") as u32;
        let source = marked.replace('~', "");
        macro_symbol(&source, offset).map_or_else(
            || "none".to_owned(),
            |found| {
                format!(
                    "{} {} [{}..{}]",
                    found.macro_name, found.name, found.start, found.end
                )
            },
        )
    }

    #[test]
    fn a_macros_positional_symbol_is_the_name_it_is_about() {
        // The span is the name, not the literal, so an editor underlines `user`, not `:user`. That
        // is also the span `workspace/rails/` records as the declaration's place, so the two draw
        // the same box.
        assert_eq!(
            macro_at("class Story\n  belongs_to :u~ser\nend\n"),
            "belongs_to user [26..30]"
        );
        // The colon counts as being on it, as `locate` treats every span's edge.
        assert_eq!(
            macro_at("class Story\n  belongs_to ~:user\nend\n"),
            "belongs_to user [26..30]"
        );
        // A module body is a body, and so is a block inside one: `included do … end` holds half a
        // concern's macros.
        assert_eq!(
            macro_at("module Taggable\n  included do\n    before_save :nor~malise\n  end\nend\n"),
            "before_save normalise [47..56]"
        );
        // `class << self` too, a namespace by the same test.
        assert_eq!(
            macro_at("class Story\n  class << self\n    attr_reader :cac~hed\n  end\nend\n"),
            "attr_reader cached [45..51]"
        );
    }

    #[test]
    fn what_is_not_a_macros_own_name() {
        // A keyword argument configures the macro rather than naming its subject; telling `to:`
        // from `dependent:` needs Rails knowledge this module lacks.
        assert_eq!(
            macro_at("class Story\n  has_many :c, dependent: :des~troy\nend\n"),
            "none"
        );
        // Nested one level down, in both ways a macro nests: a block and an array.
        assert_eq!(
            macro_at("class Story\n  scope :recent, -> { order(created_at: :de~sc) }\nend\n"),
            "none"
        );
        assert_eq!(
            macro_at("class Story\n  enum :status, [:dr~aft, :live]\nend\n"),
            "none"
        );
        // Inside a `def`, a call is a call. The body flag follows `def` and nothing else.
        assert_eq!(
            macro_at("class Story\n  def run\n    send(:no~rmalise)\n  end\nend\n"),
            "none"
        );
        // A receiver makes it someone else's method on someone else's object.
        assert_eq!(
            macro_at("class Story\n  Other.validates :ti~tle\nend\n"),
            "none"
        );
        // Top-level code is not a class body, so nothing outside one is read.
        assert_eq!(macro_at("attr_reader :ca~ched\n"), "none");
        // A symbol with no static value names nothing to look up.
        assert_eq!(
            macro_at("class Story\n  validates :\"#{pre}_i~d\"\nend\n"),
            "none"
        );
        // Not on a symbol at all.
        assert_eq!(macro_at("class Story\n  belongs~_to :user\nend\n"), "none");
    }

    /// The cursor is the `~`, removed before parsing.
    fn closure_at(marked: &str) -> bool {
        let offset = marked.find('~').expect("a ~ marking the cursor") as u32;
        let source = marked.replace('~', "");
        let result = ruby_prism::parse(source.as_bytes());
        closure_in_a_body(&result.node(), offset)
    }

    #[test]
    fn a_block_in_a_namespace_body_is_a_closure_and_a_statement_in_one_is_not() {
        // The distinction the rung rests on. A class-body statement runs with the class object as
        // `self`, fixed; a block is a value, and what `rule` does with it is `rule`'s business.
        assert!(closure_at(
            "class Parser\n  rule(:colon) { st~r(':') }\nend\n"
        ));
        assert!(!closure_at("class Parser\n  st~r(':')\nend\n"));

        // `do … end` and `{ … }` are one construct to Ruby and one node to Prism.
        assert!(closure_at(
            "class Parser\n  included do\n    va~lidate\n  end\nend\n"
        ));
        // `-> { }` is a different node, and the spelling a Rails model uses.
        assert!(closure_at(
            "class Story\n  scope :recent, -> { whe~re(live: true) }\nend\n"
        ));
        // A module body counts as a class body here: a concern's `included do` block is written in
        // one, and Rails rebinds it most often.
        assert!(closure_at(
            "module Countable\n  included do\n    has_ma~ny :counts\n  end\nend\n"
        ));
        // `class << self` fixes `self` as `class` does, one singleton step out.
        assert!(closure_at(
            "class Story\n  class << self\n    [1].each { he~lper }\n  end\nend\n"
        ));
    }

    #[test]
    fn the_closure_answer_rides_on_the_cursor_rather_than_being_asked_for_again() {
        // Why this walk runs where it does: `locator` and `completion` both need the answer, and
        // computing it on the cursor means one parse and two walks instead of re-parsing per
        // reader.
        let cursor = |marked: &str| {
            let offset = marked.find('~').expect("a ~ marking the cursor") as u32;
            at(&marked.replace('~', ""), offset).expect("a cursor")
        };
        assert!(cursor("class Parser\n  rule(:colon) { st~r(':') }\nend\n").in_a_closure);
        assert!(!cursor("class Parser\n  st~r(':')\nend\n").in_a_closure);
    }

    #[test]
    fn a_def_ends_the_question_and_a_block_inside_one_never_starts_it() {
        // A block closes over the method's `self`, which cannot be rebound: `instance_exec` on a
        // block from inside a `def` still has the `self` the block was written with. So both are
        // the `def`'s answer, not this rung's.
        assert!(!closure_at(
            "class Story\n  def self.run\n    [1].each { he~lper }\n  end\nend\n"
        ));
        assert!(!closure_at(
            "class Story\n  def run\n    [1].each { he~lper }\n  end\nend\n"
        ));
        // A `def` *inside* a closure is an ordinary `def`: the innermost construct fixing `self` is
        // what counts, not the outermost.
        assert!(!closure_at(
            "class Story\n  [1].each do\n    def run\n      he~lper\n    end\n  end\nend\n"
        ));
    }

    #[test]
    fn a_block_with_no_namespace_body_over_it_is_not_this_question() {
        // Top level: `self` is `main`, there is no class object, and rubydex never hands this rung
        // a receiver for it. The syntax half says so alone.
        assert!(!closure_at("[1].each { he~lper }\n"));
        assert!(!closure_at("he~lper\n"));
        // A `class` keyword inside a block does not put the block in the body's statements: the
        // body starts after it.
        assert!(!closure_at(
            "[1].each do\n  class Story\n    he~lper\n  end\nend\n"
        ));
        // A sibling block that does not contain the cursor records nothing.
        assert!(!closure_at(
            "class Story\n  [1].each { helper }\n  ru~n\nend\n"
        ));
    }

    #[test]
    fn an_unparseable_file_does_not_panic() {
        assert!(at("class Broken\n  def foo\n", 5).is_some());
        assert!(at("", 0).is_some());
    }

    /// The span [`returns_in`] files a `def` under, found from where its `def` keyword is.
    fn def_span(source: &str, name: &str) -> (u32, u32) {
        let at = source
            .find(&format!("def {name}"))
            .expect("a def with that name") as u32;
        *returns_in(source)
            .keys()
            .find(|(start, _)| *start == at)
            .expect("a def filed under that offset")
    }

    #[test]
    fn every_def_in_a_document_is_read_out_of_one_parse() {
        let source = "class Story\n  def title\n    \"x\"\n  end\n\n                        def count\n    1\n  end\n\n  def tags\n    []\n  end\nend\n";
        let read = returns_in(source);
        assert_eq!(read.len(), 3);
        assert_eq!(
            read[&def_span(source, "title")],
            vec![Receiver::literal("String")]
        );
        assert_eq!(
            read[&def_span(source, "count")],
            vec![Receiver::literal("Integer")]
        );
        assert_eq!(
            read[&def_span(source, "tags")],
            vec![holding("Array", &[None])]
        );
        // Asking about one `def` alone gives exactly what the whole-file read holds for it, which
        // is what lets the memo stand in for the ask.
        for (span, exits) in &read {
            assert_eq!(returns_of(source, *span), *exits);
        }
    }

    #[test]
    fn a_def_with_no_body_at_all_hands_back_the_nil_ruby_hands_back() {
        // Ruby's answer for `def title; end`, and the same `nil` an unwritten branch files, so a
        // Rails action whose template does the work is answered, not declined.
        let source = "class Story\n  def title\n  end\nend\n";
        let read = returns_in(source);
        assert_eq!(read.len(), 1);
        assert_eq!(
            read[&def_span(source, "title")],
            vec![Receiver::literal("NilClass")]
        );
    }

    #[test]
    fn a_def_with_no_exit_is_read_and_empty_rather_than_not_read_at_all() {
        // An `ensure` runs for effect and never decides the return, so a body that is *only* one
        // has no readable exit. The entry exists and is empty: the difference between declining one
        // method and declining the file.
        let source = "class Story\n  def title\n  ensure\n    log\n  end\nend\n";
        let read = returns_in(source);
        assert_eq!(read.len(), 1);
        assert!(read[&def_span(source, "title")].is_empty());
        // A span no `def` starts and ends at is the other answer: absent.
        assert!(!read.contains_key(&(0, 1)));
        assert!(returns_of(source, (0, 1)).is_empty());
    }

    #[test]
    fn an_exit_belongs_to_the_innermost_def_open_over_it() {
        let source = "class Story\n  def outer\n    def inner\n      return \"x\"\n                          end\n  end\nend\n";
        let read = returns_in(source);
        assert_eq!(read.len(), 2);
        assert_eq!(
            read[&def_span(source, "inner")],
            vec![Receiver::literal("String")]
        );
        // The outer method's tail is the `def` expression, whatever that is worth. What matters is
        // that the nested method's `return` is not filed against it; the stack guarantees that.
        assert!(!read[&def_span(source, "outer")].contains(&Receiver::literal("String")));
    }

    #[test]
    fn a_return_written_outside_every_def_is_not_a_methods_exit() {
        // Legal at a file's top level, and not a method exit. Filing it against the last `def`
        // passed would give the next method a stranger's value.
        let source = "return \"x\"\nclass Story\n  def title\n    1\n  end\nend\n";
        let read = returns_in(source);
        assert_eq!(read.len(), 1);
        assert_eq!(
            read[&def_span(source, "title")],
            vec![Receiver::literal("Integer")]
        );
    }

    #[test]
    fn a_return_in_tail_position_is_the_value_it_hands_back() {
        // A tail-position `return` must be filed once. Pushing the node as its own exit beside its
        // value would make the two disagree and decline `def title; return "x"; end`, while the
        // same method without the keyword answers `String`.
        let source = "class Story\n  def title\n    return \"x\"\n  end\nend\n";
        assert_eq!(
            returns_in(source)[&def_span(source, "title")],
            vec![Receiver::literal("String")]
        );
        // A `return` not in tail position is read as before.
        let guard = "class Story\n  def title\n    return \"x\" if plain?\n    \"y\"\n                       end\nend\n";
        assert_eq!(
            returns_in(guard)[&def_span(guard, "title")],
            vec![Receiver::literal("String"), Receiver::literal("String")]
        );
    }

    #[test]
    fn a_branch_nobody_wrote_is_the_nil_the_method_really_hands_back() {
        /// The exits of the one `def` in a body written as `body`.
        fn exits(body: &str) -> Vec<Receiver> {
            let source = format!("class Story\n  def title\n{body}\n  end\nend\n");
            returns_in(&source)[&def_span(&source, "title")].clone()
        }
        // The written branch alone would answer `String`, while the common path returns `nil`.
        // Every spelling of the missing branch reaches this, including the modifier form (`x if y`
        // is one node).
        let written_and_nil = vec![Receiver::literal("String"), Receiver::literal("NilClass")];
        assert_eq!(
            exits("    if plain?\n      \"x\"\n    end"),
            written_and_nil
        );
        assert_eq!(exits("    \"x\" if plain?"), written_and_nil);
        assert_eq!(
            exits("    unless plain?\n      \"x\"\n    end"),
            written_and_nil
        );
        assert_eq!(exits("    \"x\" unless plain?"), written_and_nil);
        assert_eq!(
            exits("    case plain?\n    when 1 then \"x\"\n    end"),
            written_and_nil
        );
        // An `elsif` chain is a nested `if`, so the `nil` lands once, under the last one.
        assert_eq!(
            exits("    if plain?\n      \"x\"\n    elsif other?\n      \"y\"\n    end"),
            vec![
                Receiver::literal("String"),
                Receiver::literal("String"),
                Receiver::literal("NilClass"),
            ]
        );
        // A branch written empty is the same value as one not written.
        assert_eq!(
            exits("    if plain?\n      \"x\"\n    else\n    end"),
            written_and_nil
        );
    }

    #[test]
    fn a_conditional_that_writes_every_branch_gains_nothing() {
        /// The exits of the one `def` in a body written as `body`.
        fn exits(body: &str) -> Vec<Receiver> {
            let source = format!("class Story\n  def title\n{body}\n  end\nend\n");
            returns_in(&source)[&def_span(&source, "title")].clone()
        }
        // Nothing is invented where Ruby has no fall-through: an `else`, a `case` with one, and a
        // ternary return exactly what they contain.
        let two = vec![Receiver::literal("String"), Receiver::literal("Integer")];
        assert_eq!(
            exits("    if plain?\n      \"x\"\n    else\n      1\n    end"),
            two
        );
        assert_eq!(exits("    plain? ? \"x\" : 1"), two);
        assert_eq!(
            exits("    case plain?\n    when 1 then \"x\"\n    else 1\n    end"),
            two
        );
        // A conditional with no branch written is `nil` twice, which agrees with itself:
        // `if x then end` returns `nil` either way.
        assert_eq!(
            exits("    if plain?\n    end"),
            vec![Receiver::literal("NilClass"), Receiver::literal("NilClass")]
        );
    }

    #[test]
    fn a_bare_return_is_the_nil_it_hands_back_and_not_nothing_at_all() {
        /// The exits of the one `def` in a body written as `body`.
        fn exits(body: &str) -> Vec<Receiver> {
            let source = format!("class Story\n  def title\n{body}\n  end\nend\n");
            returns_in(&source)[&def_span(&source, "title")].clone()
        }
        // The other spelling of a branchless conditional, as code usually writes it: a guard that
        // bails early, then the work. The `return` gives `nil` on the guard's path; dropping it
        // would draw `-> Array` on a method whose commonest answer is `nil`. The tail is pushed
        // first, so the `nil` lands second.
        assert_eq!(
            exits("    return if plain?\n    [\"x\"]"),
            vec![
                holding("Array", &[Some("String")]),
                Receiver::literal("NilClass")
            ]
        );
        // Written out, it is the same value read off a node the parser saw, so both spellings of
        // the guard answer identically.
        assert_eq!(
            exits("    return nil if plain?\n    [\"x\"]"),
            exits("    return if plain?\n    [\"x\"]")
        );
        // In tail position it is the whole answer: `tail` steps over a `return` node, and this is
        // the only exit filed.
        assert_eq!(exits("    return"), vec![Receiver::literal("NilClass")]);
        // Two values are an `Array` this module has no node for, so an unreadable exit, which
        // declines the method (a `nil` does not).
        assert_eq!(exits("    return \"x\", 1"), vec![Receiver::Unknown]);
    }

    #[test]
    fn an_assignment_hands_back_its_value_wherever_one_is_read() {
        /// The exits of the one `def` in a body written as `body`.
        fn exits(body: &str) -> Vec<Receiver> {
            let source = format!("class Story\n  def title\n{body}\n  end\nend\n");
            returns_in(&source)[&def_span(&source, "title")].clone()
        }
        // All twelve node kinds [`assigned_value`] reads: six name spellings, two operators each.
        // Written as a value, not a `def`'s tail, because a constant assigned inside a method is a
        // syntax error.
        for written in [
            "@t = true",
            "@t ||= true",
            "t = true",
            "t ||= true",
            "@@t = true",
            "@@t ||= true",
            "$t = true",
            "$t ||= true",
            "T = true",
            "T ||= true",
            "S::T = true",
            "S::T ||= true",
        ] {
            assert_eq!(
                receiver(&format!("({written}).~")),
                Receiver::literal("TrueClass"),
                "{written}"
            );
        }
        // The common shape: `def show_title_h1; @title_h1 = true; end` must answer `TrueClass`,
        // like the same method without `@title_h1 =`.
        assert_eq!(
            exits("    @title_h1 = true"),
            vec![Receiver::literal("TrueClass")]
        );
        // The memoisation idiom.
        assert_eq!(
            exits("    @periods ||= [\"1d\"]"),
            vec![holding("Array", &[Some("String")])]
        );
        // `+=` returns what the *operator* returned, not the node's value, so it stays unknown:
        // reading the value would answer `Array` for a `list += [one]` that returns whatever `+`
        // did.
        assert_eq!(exits("    @count += 1"), vec![Receiver::Unknown]);
        // `&&=` returns the receiver where it is falsy: a union, declined rather than halved.
        assert_eq!(exits("    @title &&= \"x\""), vec![Receiver::Unknown]);
        // Not a method-body rule: the same arm answers a chain on an assignment, and an assignment
        // as another's value.
        assert_eq!(receiver("(@x = \"s\").~"), Receiver::literal("String"));
        assert_eq!(
            exits("    outer = inner = \"s\""),
            vec![Receiver::literal("String")]
        );
        // Each branch is read through the assignment it ends in, so two that agree are an answer
        // and two that do not are declined.
        assert_eq!(
            exits("    if plain?\n      @a = \"x\"\n    else\n      @b = \"y\"\n    end"),
            vec![Receiver::literal("String"), Receiver::literal("String")]
        );
    }

    #[test]
    fn a_method_parameter_travels_as_its_def_and_its_slot_because_no_write_introduces_one() {
        /// The receiver a `~` marks, inside one `def` written as `header`.
        fn inside(header: &str, body: &str) -> Receiver {
            receiver(&format!(
                "class Shelf\n  def {header}\n    {body}\n  end\nend\n"
            ))
        }
        fn parameter(
            at: u32,
            method: &str,
            slot: ParameterSlot,
            default: Option<Receiver>,
        ) -> Receiver {
            Receiver::Spelled {
                was: Box::new(Receiver::Parameter {
                    at,
                    method: method.to_owned(),
                    slot,
                    default: default.map(Box::new),
                }),
                name: "held".to_owned(),
            }
        }
        // The `def` starts at byte 14: `class Shelf\n  ` is fourteen characters.
        //
        // `Finder::locals` is filled by local and multiple writes, and a parameter is neither, so
        // without this variant the name would fall to `Receiver::Named`.
        assert_eq!(
            inside("show(held)", "held.~"),
            parameter(14, "show", ParameterSlot::Positional(0), None)
        );
        // Counted from the left, required and optional alike.
        assert_eq!(
            inside("show(first, held)", "held.~"),
            parameter(14, "show", ParameterSlot::Positional(1), None)
        );
        assert_eq!(
            inside("show(first, held = 1)", "held.~"),
            parameter(
                14,
                "show",
                ParameterSlot::Positional(1),
                Some(Receiver::literal("Integer"))
            )
        );
        // **A destructured positional holds its place without a name.** `def f((a, b), held)` binds
        // `a` and `b`, which this does not name, and `held` is still at index one, since a caller
        // counts the destructure as one argument.
        assert_eq!(
            inside("show((first, second), held)", "held.~"),
            parameter(14, "show", ParameterSlot::Positional(1), None)
        );
        // A keyword is called by name, never counted, because Ruby binds it by name; a slot
        // numbered across both would answer `held` with whatever sits at position one.
        assert_eq!(
            inside("show(first, held:)", "held.~"),
            parameter(14, "show", ParameterSlot::Keyword("held".to_owned()), None)
        );
        // **The one refused default.** `= nil` means *optional, type unstated*, so carrying it
        // would put `NilClass` on the commonest optional parameter.
        assert_eq!(
            inside("show(held = nil)", "held.~"),
            parameter(14, "show", ParameterSlot::Positional(0), None)
        );
        // A default that is a shape is carried as that shape, unresolved like every value here.
        assert_eq!(
            inside("show(held = Story.new)", "held.~"),
            parameter(
                14,
                "show",
                ParameterSlot::Positional(0),
                Some(Receiver::Instance(35))
            )
        );
        // **`*rest`, `**rest` and `&block` get no slot**, nor does a positional after a rest: its
        // position depends on how many arguments the call wrote.
        //
        // Asserted as *no slot handed out*, not as one exact shape: what a refused name falls
        // through to belongs to the older arms (a bare word is also a call on `self`), and these
        // lines must not pin it.
        fn slotted(receiver: &Receiver) -> bool {
            match receiver {
                Receiver::Parameter { .. } => true,
                Receiver::Spelled { was, .. } | Receiver::Assigned { was, .. } => slotted(was),
                Receiver::Returned { on, .. } | Receiver::Yielded { on, .. } => slotted(on),
                Receiver::Destructured { of, .. } => slotted(of),
                Receiver::Shortcut { left, right, .. } => slotted(left) || slotted(right),
                _ => false,
            }
        }
        assert!(!slotted(&inside("show(*held)", "held.~")));
        assert!(!slotted(&inside("show(**held)", "held.~")));
        assert!(!slotted(&inside("show(&held)", "held.~")));
        assert!(!slotted(&inside("show(*rest, held)", "held.~")));
        // **A write beats the header**, which is why this is asked last: the name was reassigned,
        // so the header no longer says what it holds.
        assert_eq!(
            inside("show(held)", "held = \"s\"\n    held.~"),
            Receiver::Spelled {
                was: Box::new(Receiver::literal("String")),
                name: "held".to_owned(),
            }
        );
        // A read outside every `def` is nobody's parameter.
        assert!(!slotted(&receiver("class Shelf\n  held.~\nend\n")));
    }

    #[test]
    fn a_write_that_only_relays_a_parameter_does_not_displace_an_older_write_that_named_a_type() {
        /// One `def` written as `header`, with `body` as its body, and the `~` receiver in it.
        fn inside(header: &str, body: &str) -> (String, Receiver) {
            let source = format!("class Shelf\n  def {header}\n    {body}\n  end\nend\n");
            let answered = receiver(&source);
            (source, answered)
        }
        fn spelled(was: Receiver) -> Receiver {
            Receiver::Spelled {
                was: Box::new(was),
                name: "held".to_owned(),
            }
        }
        /// The parameter as it arrives through `held = passed`: a local read is always wrapped in
        /// its own `Spelled`, so the two names nest.
        fn slot(source: &str, default: Option<Receiver>) -> Receiver {
            Receiver::Spelled {
                was: Box::new(Receiver::Parameter {
                    at: source.find("  def stow").unwrap() as u32 + 2,
                    method: "stow".to_owned(),
                    slot: ParameterSlot::Positional(0),
                    default: default.map(Box::new),
                }),
                name: "passed".to_owned(),
            }
        }

        // **The older write wins.** The newer one only relays a parameter, and a parameter without
        // a default answers only where something *declares* its type, which application code rarely
        // does. So it is a disguised fallback, like a block parameter or a `super`, and shares
        // their slot.
        //
        // This is solidus reduced: `preference_store_class = Spree::Config` in one branch and
        // `= prefs_or_conf_class` in the other. Trusting the second would lose the `Hash` the first
        // has right.
        let (_, answered) = inside(
            "stow(passed)",
            "held = \"s\"\n    held = passed\n    held.~",
        );
        assert_eq!(answered, spelled(Receiver::literal("String")));

        // With no older write the parameter is still taken: a *precedence*, never a refusal.
        let (source, answered) = inside("stow(passed)", "held = passed\n    held.~");
        assert_eq!(answered, spelled(slot(&source, None)));

        // **A parameter with a default is not relayed**: the default is a shape, which cannot end
        // at nothing, so its write is solid and displaces the older one.
        let (source, answered) = inside(
            "stow(passed = 1)",
            "held = \"s\"\n    held = passed\n    held.~",
        );
        assert_eq!(
            answered,
            spelled(slot(&source, Some(Receiver::literal("Integer"))))
        );
    }

    #[test]
    fn a_shortcut_travels_as_both_its_operands_because_only_a_class_can_say_which_one_it_is() {
        /// The exits of the one `def` in a body written as `body`.
        fn exits(body: &str) -> Vec<Receiver> {
            let source = format!("class Story\n  def title\n{body}\n  end\nend\n");
            returns_in(&source)[&def_span(&source, "title")].clone()
        }
        fn shortcut(left: Receiver, right: Receiver, and: bool) -> Vec<Receiver> {
            vec![Receiver::Shortcut {
                left: Box::new(left),
                right: Box::new(right),
                and,
            }]
        }
        // Both nodes are read as shapes. Otherwise `receiver_of` would fall through to
        // `returned_by`, which answers `Unknown` for anything Prism does not call a `CallNode`, and
        // one `Unknown` exit declines the whole `def`.
        assert_eq!(
            exits("    \"a\" && 1"),
            shortcut(
                Receiver::literal("String"),
                Receiver::literal("Integer"),
                true
            )
        );
        assert_eq!(
            exits("    \"a\" || 1"),
            shortcut(
                Receiver::literal("String"),
                Receiver::literal("Integer"),
                false
            )
        );
        // `and` and `or` are the *same two Prism nodes*; they differ only in precedence, so the
        // same two lines cover both.
        assert_eq!(exits("    \"a\" and 1"), exits("    \"a\" && 1"));
        assert_eq!(exits("    \"a\" or 1"), exits("    \"a\" || 1"));
        // An unreadable operand is carried, not a refusal of the pair: the untaken side is never
        // read, and `nil && whatever` is `nil`.
        assert_eq!(
            exits("    nil && @count += 1"),
            shortcut(Receiver::literal("NilClass"), Receiver::Unknown, true)
        );
        // Not a method-body rule: the same arm answers a chain on a shortcut, and a shortcut as an
        // assignment's value. One question, asked once, which is why it lives in `receiver_of`, not
        // `Exits::tail`.
        assert_eq!(
            receiver("(\"a\" || 1).~"),
            Receiver::Shortcut {
                left: Box::new(Receiver::literal("String")),
                right: Box::new(Receiver::literal("Integer")),
                and: false,
            }
        );
        assert_eq!(
            exits("    held = \"a\" && 1"),
            shortcut(
                Receiver::literal("String"),
                Receiver::literal("Integer"),
                true
            )
        );
        // They nest on the side Ruby nests them: `a && b && c` is `(a && b) && c`.
        assert_eq!(
            exits("    \"a\" && 1 && []"),
            shortcut(
                Receiver::Shortcut {
                    left: Box::new(Receiver::literal("String")),
                    right: Box::new(Receiver::literal("Integer")),
                    and: true,
                },
                holding("Array", &[None]),
                true
            )
        );
    }

    #[test]
    fn a_write_through_a_setter_hands_back_the_argument_and_never_the_body() {
        /// The exits of the one `def` in a body written as `body`.
        fn exits(body: &str) -> Vec<Receiver> {
            let source = format!("class Story\n  def title\n{body}\n  end\nend\n");
            returns_in(&source)[&def_span(&source, "title")].clone()
        }
        // Ruby discards `def name=`'s own return: `obj.name = "x"` evaluates to `"x"`.
        assert_eq!(
            exits("    story.name = \"x\""),
            vec![Receiver::literal("String")]
        );
        // The index spelling is the same node with the name `[]=`. `report[key] = v` evaluates to
        // `v` too; the subscripts are the arguments before it.
        assert_eq!(
            exits("    report[:a] = 1"),
            vec![Receiver::literal("Integer")]
        );
        assert_eq!(
            exits("    grid[1, 2] = [\"x\"]"),
            vec![holding("Array", &[Some("String")])]
        );
        // Assignment syntax either way, per Prism's own flag, not the name: `obj.x=(v)` is an
        // assignment and returns `v`.
        assert_eq!(
            receiver("(story.name=(\"x\")).~"),
            Receiver::literal("String")
        );
        // Not a method-body rule, like the assignments above: the same arm answers a chain on one,
        // and one as another's value.
        assert_eq!(
            receiver("(story.name = \"x\").~"),
            Receiver::literal("String")
        );
        assert_eq!(
            exits("    held = story.name = \"x\""),
            vec![Receiver::literal("String")]
        );
        // **Safe navigation is a union, declined.** `story&.name = "x"` returns `nil` where `story`
        // is `nil`, so it is `String | nil`, the same shape as `&&=`.
        assert_eq!(exits("    story&.name = \"x\""), vec![Receiver::Unknown]);
        // An ordinary call to the setter is not assignment syntax and really returns the body's
        // value, so it is left to the call rung.
        assert!(matches!(
            exits("    story.send(:name=, \"x\")").as_slice(),
            [Receiver::Returned { method, .. }] if method == "send"
        ));
    }

    #[test]
    fn super_is_read_as_the_enclosing_defs_name_and_the_arguments_it_wrote() {
        /// The exits of the one `def` in a body written as `body`.
        fn exits(body: &str) -> Vec<Receiver> {
            let source = format!("class Story\n  def title\n{body}\n  end\nend\n");
            returns_in(&source)[&def_span(&source, "title")].clone()
        }
        /// The one exit's shape, minus the offset (a position in this fixture).
        fn shape(body: &str) -> Option<(String, bool, Arity)> {
            match exits(body).first()? {
                Receiver::Super {
                    method,
                    block,
                    arity,
                    ..
                } => Some((method.clone(), *block, *arity)),
                _ => None,
            }
        }
        // The name is the enclosing `def`'s, which is where Ruby takes it from: the keyword carries
        // none.
        assert_eq!(
            shape("    super"),
            Some(("title".to_owned(), false, Arity::Unknown))
        );
        // `super(a, b)` writes its own arguments and is counted like a call; a bare `super`
        // forwards whatever the caller got, which this file cannot count.
        assert_eq!(
            shape("    super(1, 2)"),
            Some(("title".to_owned(), false, Arity::Exactly(2)))
        );
        assert_eq!(
            shape("    super()"),
            Some(("title".to_owned(), false, Arity::Exactly(0)))
        );
        // A splat gives up on the count rather than guessing low: `arity_of`'s rule, through the
        // one copy both spellings share.
        assert_eq!(
            shape("    super(*args)"),
            Some(("title".to_owned(), false, Arity::Unknown))
        );
        // Either spelling can carry its own block, and RBS may declare a method differently with
        // and without one.
        assert_eq!(
            shape("    super { |x| x }"),
            Some(("title".to_owned(), true, Arity::Unknown))
        );
        assert_eq!(
            shape("    super(1) { |x| x }"),
            Some(("title".to_owned(), true, Arity::Exactly(1)))
        );
        // Read as a receiver too, not only as an exit: `super.foo` is an ordinary chain, and one
        // arm answers both.
        assert!(matches!(
            receiver("class Story\n  def title\n    super.~\n  end\nend\n"),
            Receiver::Super { method, .. } if method == "title"
        ));
        // A `super` outside every `def` has no name to take. It is legal in `define_method`, where
        // the name is the macro's symbol; answering from the last walked `def` would read an
        // unrelated method.
        let loose = concat!(
            "class Story\n  def title\n    1\n  end\n\n",
            "  define_method(:other) do\n    super.~\n  end\nend\n"
        );
        assert_eq!(receiver(loose), Receiver::Unknown);
    }

    #[test]
    fn a_write_of_super_does_not_displace_one_that_produced_a_type() {
        // `ActionController::Instrumentation#render` in miniature, and why [`reaches_a_super`]
        // exists: the newer write is a `super` in a **module**, which resolves through the
        // including class's ancestry and so to nothing. Taking it on position would lose the label
        // for every Rails action ending in `render`.
        let source = concat!(
            "class Story\n  def title\n    out = nil\n",
            "    [1].each { out = super }\n    out.~\n  end\nend\n"
        );
        assert_eq!(
            receiver(source),
            Receiver::Spelled {
                was: Box::new(Receiver::literal("NilClass")),
                name: "out".to_owned(),
            }
        );
        // **Both assignment loops**, because `@out = super` is the same sentence: instance-variable
        // writes use the same three slots as locals.
        let ivar = concat!(
            "class Story\n  def title\n    @out = \"s\"\n",
            "    [1].each { @out = super }\n    @out.~\n  end\nend\n"
        );
        assert!(matches!(
            receiver(ivar),
            Receiver::Spelled { was, .. }
                if matches!(*was, Receiver::Assigned { ref was, .. }
                    if matches!(**was, Receiver::Literal { class: "String", .. }))
        ));
        // With no write above it, the `super` is still taken: a precedence, not a refusal, like the
        // block-parameter slot.
        let alone = "class Story\n  def title\n    out = super\n    out.~\n  end\nend\n";
        assert!(matches!(
            receiver(alone),
            Receiver::Spelled { was, .. } if matches!(*was, Receiver::Super { .. })
        ));
    }

    #[test]
    fn a_return_inside_a_lambda_belongs_to_the_lambda_and_not_the_method() {
        /// The exits of the one `def` in a body written as `body`.
        fn exits(body: &str) -> Vec<Receiver> {
            let source = format!("class Story\n  def title\n{body}\n  end\nend\n");
            returns_in(&source)[&def_span(&source, "title")].clone()
        }
        // Solidus' `code_column` in miniature, and why the fence exists: a `return` inside `->`
        // returns from the **lambda**. Filing it would put a `nil` beside the method's real `Hash`
        // and decline a method that answers fine.
        assert_eq!(
            exits("    { a: ->(x) do\n      return if x.nil?\n      \"y\"\n    end }"),
            vec![holding("Hash", &[Some("Symbol"), Some("Proc")])]
        );
        // An ordinary block is not fenced: a `return` in one *does* leave the method. That is the
        // difference between a block and a lambda.
        let block = exits("    [1].each do |x|\n      return if x\n    end");
        assert_eq!(block.len(), 2, "the call's own value, and the `return`");
        assert_eq!(block[1], Receiver::literal("NilClass"));
        // A `def` inside a lambda owns its `return`s again.
        let nested = "class Story\n  def title\n    -> do\n      def inner\n        return \"x\"\n      end\n    end\n  end\nend\n";
        assert_eq!(
            returns_in(nested)[&def_span(nested, "inner")],
            vec![Receiver::literal("String")]
        );
    }

    #[test]
    fn a_chain_is_followed_far_past_where_a_shared_bound_stopped_it() {
        // Twelve links. Each is one question with no candidate list, so the twentieth costs what
        // the first did, and Rails' query interface produces nine- and ten-link chains routinely.
        let long = format!("\"hi\"{}.~", ".upcase".repeat(12));
        let mut at = &receiver(&long);
        let mut links = 0;
        while let Receiver::Returned { on, .. } = at {
            links += 1;
            at = on;
        }
        assert_eq!(links, 12);
        assert_eq!(*at, Receiver::literal("String"));
    }

    #[test]
    fn asking_which_assignment_a_name_came_from_is_bounded_the_way_a_link_is() {
        /// `a = "x"` and then `hops` aliases of it, read at the last one.
        fn aliased(hops: usize) -> Receiver {
            let mut source = String::from("a = \"x\"\n");
            let mut previous = "a".to_owned();
            for step in 0..hops {
                source.push_str(&format!("v{step} = {previous}\n"));
                previous = format!("v{step}");
            }
            source.push_str(&format!("{previous}.~\n"));
            receiver(&source)
        }
        // Each hop visits **every** write of the name it reads, so a hop multiplies where a link
        // adds. The bound is [`MAX_WIDTH`], and within it the chain resolves.
        assert!(matches!(aliased(1), Receiver::Spelled { .. }));
        assert!(matches!(aliased(3), Receiver::Spelled { .. }));
        assert!(matches!(aliased(6), Receiver::Spelled { .. }));
        assert!(matches!(aliased(18), Receiver::Spelled { .. }));
        // Past the bound, only the name is left: the last rung, not a wrong answer. That property
        // must survive the bound moving.
        assert_eq!(aliased(19), Receiver::Named("v18".to_owned()));
        assert_eq!(aliased(40), Receiver::Named("v39".to_owned()));
        // The two axes share the limit but count different things: twenty *links* type fine, one
        // question each.
        assert!(matches!(
            receiver("\"hi\".upcase.upcase.upcase.upcase.upcase.upcase.~"),
            Receiver::Returned { .. }
        ));
    }

    #[test]
    fn a_question_already_answered_is_not_asked_again_and_the_answer_does_not_move() {
        /// `levels` instance variables, each written `writes` times from the one below.
        ///
        /// Nothing at the bottom types, so no write fills `solid` and every level visits every
        /// candidate: the only shape that re-asks a question, and what [`Finder::memo`] is for.
        /// Every `@v{k}` below the top is reached once per write of the level above, at the same
        /// budget each time.
        fn stacked(levels: usize, writes: usize) -> String {
            let mut source = String::new();
            for level in 0..levels {
                let below = match level {
                    0 => "@nothing".to_owned(),
                    _ => format!("@v{}", level - 1),
                };
                for _ in 0..writes {
                    source.push_str(&format!("@v{level} = {below}\n"));
                }
            }
            source.push_str(&format!("@v{}.~\n", levels - 1));
            source
        }
        // The answer is the last rung and stays there however wide the stack gets. That is the
        // property the memo must keep: it stores one answer per *(span, budget)*, and handing a
        // shallower walk's answer to a deeper one would show up here as a moved name.
        for width in [2, 4] {
            for levels in [2, 4, 6] {
                assert_eq!(
                    receiver(&stacked(levels, width)),
                    Receiver::Named(format!("@v{}", levels - 1)),
                    "{levels} levels {width} wide"
                );
            }
        }
        // The loop stops on the same precedence as before. Candidates arrive newest first, so the
        // first write to fill a slot is its answer and a solid write ends the loop. An *older*
        // typing write must still beat a newer one rooted in a call on `self`; stopping one
        // candidate early would get that wrong.
        assert_eq!(
            typed("@x = \"s\"\n@x = whatever\n@x.~"),
            Receiver::Assigned {
                at: 0,
                was: Box::new(Receiver::literal("String")),
            },
            "an instance variable assigned a literal above a receiverless call"
        );
    }
}
