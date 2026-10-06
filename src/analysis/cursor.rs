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
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use ruby_prism::{
    AssocNode, BlockNode, CallAndWriteNode, CallNode, CallOperatorWriteNode, CallOrWriteNode,
    ClassNode, ConstantAndWriteNode, ConstantOperatorWriteNode, ConstantOrWriteNode,
    ConstantPathAndWriteNode, ConstantPathNode, ConstantPathOperatorWriteNode,
    ConstantPathOrWriteNode, ConstantPathWriteNode, ConstantReadNode, ConstantWriteNode, DefNode,
    InstanceVariableAndWriteNode, InstanceVariableOrWriteNode, InstanceVariableWriteNode,
    ItLocalVariableReadNode, LambdaNode, LocalVariableAndWriteNode, LocalVariableOperatorWriteNode,
    LocalVariableOrWriteNode, LocalVariableReadNode, LocalVariableTargetNode,
    LocalVariableWriteNode, Location, MatchLastLineNode, ModuleNode, MultiWriteNode, Node,
    ParseResult, RegularExpressionNode, SingletonClassNode, StatementsNode, StringNode, SymbolNode,
    Visit, XStringNode,
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
            Receiver::Instance {
                at,
                arity,
                arguments,
                keywords,
            } => Receiver::Instance {
                at: rebase.to_graph(*at)?,
                arity: *arity,
                // All or none, as for `Returned` below.
                arguments: arguments
                    .iter()
                    .map(|written| written.rebased(rebase))
                    .collect::<Option<Vec<_>>>()
                    .unwrap_or_default(),
                keywords: keywords.as_ref().and_then(|written| {
                    written
                        .iter()
                        .map(|(name, value)| Some((name.clone(), value.rebased(rebase)?)))
                        .collect()
                }),
            },
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
                keywords,
                safe,
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
                // All or none too: a keyword that cannot be placed is no claim about the rest.
                keywords: keywords.as_ref().and_then(|written| {
                    written
                        .iter()
                        .map(|(name, value)| Some((name.clone(), value.rebased(rebase)?)))
                        .collect()
                }),
                safe: *safe,
            },
            Receiver::Yielded {
                on,
                method,
                index,
                safe,
                arity,
                arguments,
                keywords,
                spreads,
                default,
            } => Receiver::Yielded {
                on: Box::new(on.rebased(rebase)?),
                method: method.clone(),
                index: *index,
                safe: *safe,
                arity: *arity,
                // All or none, for [`Receiver::Returned`]'s reason.
                arguments: arguments
                    .iter()
                    .map(|written| written.rebased(rebase))
                    .collect::<Option<Vec<_>>>()
                    .unwrap_or_default(),
                keywords: keywords.as_ref().and_then(|written| {
                    written
                        .iter()
                        .map(|(name, value)| Some((name.clone(), value.rebased(rebase)?)))
                        .collect()
                }),
                spreads: *spreads,
                default: match default {
                    Some(held) => Some(Box::new(held.rebased(rebase)?)),
                    None => None,
                },
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
            // Every branch must translate, because the value may be any of them.
            Receiver::Either(arms) => Receiver::Either(
                arms.iter()
                    .map(|arm| arm.rebased(rebase))
                    .collect::<Option<Vec<_>>>()?,
            ),
            // Graph keys, like `Constant`'s: each is where a constant is resolved.
            Receiver::Rescued(classes) => Receiver::Rescued(
                classes
                    .iter()
                    .map(|at| rebase.to_graph(*at))
                    .collect::<Option<Vec<_>>>()?,
            ),
            Receiver::Shortcut { left, right, and } => Receiver::Shortcut {
                left: Box::new(left.rebased(rebase)?),
                right: Box::new(right.rebased(rebase)?),
                and: *and,
            },
            // The body this offset falls in decides the answer, so it is a position in the graph's
            // text like `Constant`'s. A `self` captured above the cursor is exactly what a
            // keystroke moves.
            Receiver::SelfObject(offset) => Receiver::SelfObject(rebase.to_graph(*offset)?),
            // The offset moves for [`Receiver::SelfObject`]'s reason.
            Receiver::Parameter { at, method, slot } => Receiver::Parameter {
                at: rebase.to_graph(*at)?,
                method: method.clone(),
                slot: slot.clone(),
            },
            // The literal's offset is a key into its document's [`Shapes::procs`], like a `def`'s.
            Receiver::Proc { at, call } => Receiver::Proc {
                at: rebase.to_graph(*at)?,
                call: match call {
                    Some(call) => Some(Box::new(call.rebased(rebase)?)),
                    None => None,
                },
            },
            Receiver::ProcParameter { at, index } => Receiver::ProcParameter {
                at: rebase.to_graph(*at)?,
                index: *index,
            },
            // The `def`'s offset moves for [`Receiver::Parameter`]'s reason; the values handed
            // over walk all or none, like a call's arguments.
            Receiver::Yield {
                at,
                method,
                arguments,
            } => Receiver::Yield {
                at: rebase.to_graph(*at)?,
                method: method.clone(),
                arguments: arguments
                    .iter()
                    .map(|handed| handed.rebased(rebase))
                    .collect::<Option<Vec<_>>>()
                    .unwrap_or_default(),
            },
            // The `def`'s offset moves for [`Receiver::Parameter`]'s reason, and the value walks.
            Receiver::BlockGiven {
                at,
                method,
                given,
                value,
            } => Receiver::BlockGiven {
                at: rebase.to_graph(*at)?,
                method: method.clone(),
                given: *given,
                value: Box::new(value.rebased(rebase)?),
            },
            Receiver::Stored(value) => Receiver::Stored(Box::new(value.rebased(rebase)?)),
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
            // Nothing to move: a name, a literal, `::`, the two dead ends, and a read, whose offset
            // names a row of its own text's table, not a place in the graph.
            Receiver::Literal { .. }
            | Receiver::TopLevel
            | Receiver::Named(_)
            | Receiver::Variable(_)
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
            symbol: None,
            text: Text::default(),
            names: Literals::default(),
        }
    }

    /// The value under every [`Receiver::BlockGiven`] around it: what it is wherever no call
    /// decides whether a block was passed.
    #[must_use]
    pub fn unguarded(&self) -> &Self {
        match self {
            Self::BlockGiven { value, .. } | Self::Stored(value) => value.unguarded(),
            other => other,
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ParameterSlot {
    /// Counted from the left, over required then optional positionals.
    Positional(usize),
    /// Called by name, without its colon, as RBS spells it.
    Keyword(String),
}

/// A string literal's text ([`Receiver::Literal`]), which no comparison of two receivers reads: a
/// string's shape is its class, and its text matters only to the one rung that looks a key up.
#[derive(Debug, Clone, Default)]
pub struct Text(pub Option<Box<str>>);

impl PartialEq for Text {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for Text {}

/// What an array or a hash literal spells, where every element is a name or one more such literal
/// (`[:title, { tags: [] }]`), for [`Receiver::Literal`]: a list of names written as data, which a
/// knowledge module reads as a filter (`types`' `shaped`). No comparison of two receivers reads it,
/// as for [`Text`].
#[derive(Debug, Clone, Default)]
pub struct Literals(pub Option<Rc<Names>>);

impl PartialEq for Literals {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for Literals {}

/// One level of [`Literals`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Names {
    /// A symbol's name.
    Symbol(Box<str>),
    /// A plain string's text.
    Text(Box<str>),
    /// An array literal, a braceless hash in it included as a [`Names::Pairs`].
    List(Box<[Names]>),
    /// A hash literal whose every key is a symbol or a plain string, by the key's text.
    Pairs(Box<[(Box<str>, Names)]>),
}

/// How deep [`Names`] reads a literal: a guard against a stack overflow on a pathological one, far
/// above any filter written by hand.
const NAMES_DEEP: usize = 16;

impl Names {
    /// What a literal spells, or `None` where some element is anything else.
    fn of(node: &Node<'_>, depth: usize) -> Option<Self> {
        if depth > NAMES_DEEP {
            return None;
        }
        if let Some(symbol) = node.as_symbol_node() {
            return Some(Self::Symbol(
                String::from_utf8_lossy(symbol.unescaped()).into(),
            ));
        }
        if let Some(string) = node.as_string_node() {
            return Some(Self::Text(
                String::from_utf8_lossy(string.unescaped()).into(),
            ));
        }
        if let Some(array) = node.as_array_node() {
            return array
                .elements()
                .iter()
                .map(|element| Self::of(&element, depth + 1))
                .collect::<Option<Box<[Self]>>>()
                .map(Self::List);
        }
        let elements = match (node.as_hash_node(), node.as_keyword_hash_node()) {
            (Some(hash), _) => hash.elements(),
            (None, Some(hash)) => hash.elements(),
            (None, None) => return None,
        };
        elements
            .iter()
            .map(|element| {
                let pair = element.as_assoc_node()?;
                let key = match Self::of(&pair.key(), depth + 1)? {
                    Self::Symbol(key) | Self::Text(key) => key,
                    Self::List(_) | Self::Pairs(_) => return None,
                };
                Some((key, Self::of(&pair.value(), depth + 1)?))
            })
            .collect::<Option<Box<[(Box<str>, Self)]>>>()
            .map(Self::Pairs)
    }
}

/// The thing to the left of the `.` or the `::`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Receiver {
    /// A constant path. The offset is inside its last segment, where the graph files the resolved
    /// reference: `HR::Person.` points into `Person`, not `HR`.
    Constant(u32),
    /// An *instance* of a constant: `Foo.new.`, or a local holding one, and what `new` was passed.
    ///
    /// - **`at` means what `Constant`'s offset does**; only the side of the class differs.
    /// - **The arguments ride along for one reader**: [`types`](super::types) binds them to the
    ///   `initialize` the instance runs, so this object's instance variables hold what this call
    ///   passed. The same three fields, by the same rules, as [`Returned`](Self::Returned)'s.
    Instance {
        at: u32,
        arity: Arity,
        arguments: Vec<Receiver>,
        keywords: Option<Vec<(String, Receiver)>>,
    },
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
        /// **A symbol's own name**: `title` for `:title` and `:"title"`. `None` for every other
        /// literal, and for an interpolated symbol, whose name only running Ruby knows.
        ///
        /// A signature can name one symbol as a parameter's type (`(:title) -> String?`), and only
        /// the name tells two such arms apart ([`types`](super::types)' `pick_by_literal`).
        symbol: Option<Box<str>>,
        /// **A string's own text**: `a.b` for `"a.b"` and `'a.b'`. `None` for every other literal,
        /// and for an interpolated string, whose text only running Ruby knows. What a key a
        /// knowledge module keeps names (`generated::KEYED`) is read off it.
        text: Text,
        /// **What an array or a hash literal spells** ([`Literals`]), where it is names alone:
        /// `[:title, tags: []]`. A filter a knowledge module reads is written that way
        /// (`generated::SHAPED`). `None` for every other literal.
        names: Literals,
    },
    /// A literal `self`, or the implicit one a receiverless call has, **and where it was written**.
    ///
    /// - **This offset is not a graph key.** It is the position whose *enclosing body* decides what
    ///   `self` means, which [`types::method_receiver`](super::types::method_receiver) answers
    ///   against the graph.
    /// - **A `self` is not always read where it is written.** `held = self` above a
    ///   `Class.new(base) do … end` block, and `held.` inside it, are two `self`s: rubydex records
    ///   the block body as an anonymous class. Resolving against the *cursor's* scope would give
    ///   the anonymous class, with none of the instance's members and no name a card can print.
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
        ///   read: a splat, a spent width budget, or more arguments than [`MAX_ARGUMENTS`]. The
        ///   consumer requires one shape per counted argument, so an empty list can only cost an
        ///   answer, never invent one. A keyword hash is skipped, as it is not counted.
        arguments: Vec<Receiver>,
        /// The **keywords** the call wrote without braces, by name, in written order.
        ///
        /// - **What `arguments` skips**, read by [`types`](super::types) only to bind a method's
        ///   keyword parameters for one call.
        /// - **`None` is *no claim***: a `**` splat, a key that is not a plain symbol, or a spent
        ///   budget, where which keywords were passed cannot be read. `Some` of an empty list is a
        ///   call that wrote none.
        keywords: Option<Vec<(String, Receiver)>>,
        /// The call was written with `&.`, so on a `nil` receiver it is skipped and answers `nil`.
        ///
        /// - **Syntax, like `block` and `arity`.** What it means for a type is
        ///   [`types`](super::types)' question: `a&.m` is `M?` only where `a` can be `nil`.
        /// - **`&.` skips this one call, not the rest of the chain.** `nil&.foo.bar` raises, so the
        ///   next link is an ordinary call on whatever this one answered, and carries `false`.
        /// - **Never set on an implicit `self`**, which has no operator to write.
        safe: bool,
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
        /// The call was written with `&.`, so the block never runs on a `nil` receiver. Without it
        /// a `T?` receiver hands the block `nil` too, where `nil` has the method (`then`, `tap`).
        safe: bool,
        /// The call's own arguments, as [`Receiver::Returned`] carries them: where no signature
        /// says what the method yields, its Ruby `yield`s are read with them bound.
        arity: Arity,
        arguments: Vec<Receiver>,
        keywords: Option<Vec<(String, Receiver)>>,
        /// The block has more than one parameter, or one and a `*rest` (or a trailing comma), so
        /// Ruby unpacks one `Array` handed to it: one value handed over says nothing positional
        /// ([`BlockParameter::spreads`]).
        spreads: bool,
        /// An optional parameter's default, which it holds where a `yield` hands fewer values
        ///. Read where the block is written, like the block itself.
        default: Option<Box<Receiver>>,
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
    /// The value of a **conditional read as a value**: whichever branch ran handed back its last
    /// statement (`x = if … else … end`, `c ? a : b`, `case`, `begin … rescue … end`, `a rescue b`).
    ///
    /// - **A shape like [`Receiver::Shortcut`], without an operator's rule.** Which branch runs
    ///   depends on a condition nothing here evaluates, so every branch counts, and
    ///   [`types`](super::types) joins them.
    /// - **A branch nobody wrote is `nil`**, the same fact a method's exits file (`Exit::Nil`): an
    ///   `if` or a `case` without `else`, an empty branch. A `case … in` without `else` raises
    ///   instead, so it adds nothing.
    /// - **A branch that never hands a value back adds nothing**: one ending in `raise`, `fail`,
    ///   `return`, `next`, `break`, `redo` or `retry`. A conditional whose every branch does so is
    ///   no value at all ([`Receiver::Unknown`]).
    Either(Vec<Receiver>),
    /// The exception a `rescue` caught (`rescue A, B => e`): an instance of one of the classes it
    /// names, each by the offset its constant's last segment ends at, as [`Receiver::Constant`]
    /// records one. Empty is what Ruby rescues when no class is named, `StandardError`.
    ///
    /// Not a [`Receiver::Instance`]: nothing here called `new`, so a class's own `self.new` and
    /// what `new` passed say nothing about this object.
    Rescued(Vec<u32>),
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
    /// - **No default.** A default says what the parameter holds when the caller passed nothing, and
    ///   a caller may pass anything, so it is not the parameter's type.
    Parameter {
        /// Where the `def` was written; see [`Receiver::Super::at`].
        at: u32,
        /// The enclosing `def`'s name, as the source spells it.
        method: String,
        /// Which parameter of it this is.
        slot: ParameterSlot,
    },
    /// What the enclosing `def`'s own block hands back, where the method `yield`s, or calls its
    /// `&block` (`block.call`, `block.()`, `block.yield`, `block[]`) and never reassigns it
    ///.
    ///
    /// - **A shape like [`Receiver::Parameter`]**: the block is what the *call* passed, so only a
    ///   body read for one call can answer it ([`types`](super::types)' `yielded_to_block`), and
    ///   the `def`'s own label cannot.
    /// - **`arguments` are what was handed over**, which a `&:name` block calls `name` on.
    Yield {
        /// Where the enclosing `def` was written; see [`Receiver::Super::at`].
        at: u32,
        /// The enclosing `def`'s name, as the source spells it.
        method: String,
        /// Each positional value handed to the block, in written order; empty where they could
        /// not be read.
        arguments: Vec<Receiver>,
    },
    /// The value of an assignment read as a value (`x = (@y = v)`, a `def` whose last statement is
    /// `@y = v`): `v`'s, and also held by what it was written to.
    ///
    /// - **Its type is `v`'s**, which [`types`](super::types) answers as for `v`.
    /// - **What the object held when it was made is not kept** (`types`' `Typed::shaped`): code may
    ///   write into it through the other name before the read.
    Stored(Box<Receiver>),
    /// A value only one kind of call reaches: one that passed the enclosing `def` a block, or one
    /// that passed none. The `def` asked, with `block_given?` or a read of its own
    /// `&block`, and this value is on one side of the answer: an exit, or a conditional's branch.
    ///
    /// - **A shape like [`Receiver::Yield`]**: whether a block was passed is the *call*'s fact,
    ///   so only a body read for one call leaves the other side out ([`types`](super::types)'
    ///   `unreached`). Everywhere else it is `value`.
    /// - **Where the path asked twice, the innermost answer is kept.** Either is true wherever the
    ///   value is reached.
    BlockGiven {
        /// Where the enclosing `def` was written; see [`Receiver::Super::at`].
        at: u32,
        /// The enclosing `def`'s name, as the source spells it.
        method: String,
        /// Reached only where a block was passed (`true`), or only where none was (`false`).
        given: bool,
        value: Box<Receiver>,
    },
    /// A proc or lambda written as a literal (`->(x) { }`, `lambda { }`, `proc { }`,
    /// `Proc.new { }`), by where it starts: a `Proc` that remembers which literal it is,
    /// so a call of it, or a block it is passed as, reads that literal's body ([`Shapes::procs`]).
    Proc {
        at: u32,
        /// What the literal is as a call, `None` for `->`, which is syntax. `lambda { }` and
        /// `proc { }` are calls of `Kernel`'s methods, which a class can define for itself, so the
        /// call is typed as any other and is this literal only where Ruby's method answered.
        call: Option<Box<Receiver>>,
    },
    /// A positional parameter of a proc or lambda literal, by where the literal starts and the
    /// parameter's position: what one call of the literal passed there.
    ProcParameter { at: u32, index: usize },
    /// A read of a local or an instance variable, answered by every write that can reach it.
    ///
    /// - **A reference, not a copy.** The offset is where the read's name starts, and
    ///   [`Variables`], built from the same text, says which writes reach it and whether `nil` or a
    ///   parameter's own value can. [`types`](super::types) folds them. Copying the writes' shapes
    ///   in instead nests a copy per read: `q = q.map { |v| v } if c`, written twenty times, holds
    ///   a million of them.
    /// - **Never rebased.** Like [`Receiver::Assigned`]'s offset it is no graph key: it names a
    ///   read in the text the table is read from, and [`types`](super::types) reads the table from
    ///   the same text the shape was parsed from.
    Variable(u32),
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
///   body in this file (`&:upcase`, a passed `&blk`, a forwarded `&`). A held `Unknown` is a body that *was* read
///   but whose value cannot be named. [`returns_of`] draws the same line for a `def`; dropping
///   unreadable exits is how the rung would lie.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Block {
    /// No block written, in any spelling.
    #[default]
    None,
    /// One written, and the shapes its body hands back: its tail, and every `next`'s value.
    Written(Box<[Receiver]>),
    /// One written whose body `break`s: what it hands back, and what each `break` makes the
    /// **call** return instead. A `break` ends the method the block was passed to, so
    /// its value is the call's, never the block's. Boxed, since it is rare and every call's
    /// [`Receiver`] carries a `Block`'s room.
    Breaking(Box<Breaks>),
    /// `&:name`: the method `name`, called on what the block is handed. Ruby's
    /// `Symbol#to_proc` calls it publicly, with the rest of what was handed as its arguments.
    Symbol(String),
    /// `&value`: a proc passed as the block. Its shape, which a proc literal in reach answers
    /// ([`Receiver::Proc`]).
    Passed(Box<Receiver>),
    /// An anonymous `&`: the enclosing method's own block, passed on. A block slot is
    /// written, so the block arm applies as it always did, but whether the block is there is the
    /// enclosing call's fact, not this one's.
    Forwarded,
}

/// A block that `break`s ([`Block::Breaking`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breaks {
    /// What it hands back.
    pub exits: Box<[Receiver]>,
    /// What each `break` makes the call return.
    pub breaks: Box<[Receiver]>,
}

impl Block {
    /// Whether a block was written at all: the arity-like fact, and all the overload table reads.
    #[must_use]
    pub fn written(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// The shapes the block returns; empty where none was written or none could be read.
    #[must_use]
    pub fn exits(&self) -> &[Receiver] {
        match self {
            Self::Written(exits) => exits,
            Self::Breaking(breaking) => &breaking.exits,
            Self::None | Self::Symbol(_) | Self::Passed(_) | Self::Forwarded => &[],
        }
    }

    /// What each `break` in the block makes the call return; empty where none is written.
    #[must_use]
    pub fn breaks(&self) -> &[Receiver] {
        match self {
            Self::Breaking(breaking) => &breaking.breaks,
            Self::None | Self::Written(_) | Self::Symbol(_) | Self::Passed(_) | Self::Forwarded => {
                &[]
            }
        }
    }

    /// [`Receiver::rebased`], down the exits.
    ///
    /// **An exit that will not translate becomes [`Receiver::Unknown`] instead of refusing the
    /// whole receiver**, unlike every other arm of that walk. Refusing would take the chain's
    /// *head* down with it, to say nothing about an element. An `Unknown` exit says exactly what is
    /// true: the block's value cannot be named here.
    fn rebased(&self, rebase: &Rebase) -> Option<Self> {
        let each = |shapes: &[Receiver]| -> Box<[Receiver]> {
            shapes
                .iter()
                .map(|shape| shape.rebased(rebase).unwrap_or(Receiver::Unknown))
                .collect()
        };
        Some(match self {
            Self::None => Self::None,
            Self::Written(exits) => Self::Written(each(exits)),
            // A `break` that will not translate is an `Unknown` value of the call, which refuses
            // the call: the same rule as an exit, one level up.
            Self::Breaking(breaking) => Self::Breaking(Box::new(Breaks {
                exits: each(&breaking.exits),
                breaks: each(&breaking.breaks),
            })),
            Self::Symbol(name) => Self::Symbol(name.clone()),
            Self::Passed(value) => {
                Self::Passed(Box::new(value.rebased(rebase).unwrap_or(Receiver::Unknown)))
            }
            Self::Forwarded => Self::Forwarded,
        })
    }
}

/// How many positional arguments a call wrote, and whether it wrote keywords.
///
/// Positional only: a keyword hash is not counted, and RBS counts the two apart too. An uncountable
/// arity is a value, not an absence, because they answer differently: a call with no arguments
/// reaches only arms that take none, while an uncountable one reaches what every arm agrees on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    /// Exactly this many, every one of them written out, and no keywords.
    Exactly(u32),
    /// This many positionals and a keyword hash (`where(title: "x")`). An arm that takes keywords
    /// gets the hash as keywords; any other gets it as one more positional, as Ruby passes it.
    Keyed(u32),
    /// This many positionals and a keyword hash of `**` splats alone (`f(a, **opts)`). An empty
    /// one passes nothing, so the call is either [`Self::Exactly`] or [`Self::Keyed`] at this count.
    /// A hash with a pair written in it is never empty, whatever its keys.
    Spread(u32),
    /// A splat or argument forwarding: `foo(*args)` writes a number of arguments nothing can know.
    Unknown,
}

/// How wide a receiver walk may go, on either axis: the chain's *width*.
///
/// **One number for two axes:**
///
/// - **Links.** `Post.where(x).order(y).first.title` is one question per `.`, and each costs the
///   same, so this can be generous. The longest chain the six reference corpora write is 24 links
///   (a command-line task); at the old 20 it was cut. **A guard, not a limit real code
///   meets**: 64 is headroom, and raising it from 20 to 80 changed no answer and no time.
/// - **Fan-out.** A rule that branches (a shortcut's two operands, a call's arguments, a block's
///   value) asks several questions where a link asks one.
///
/// **A variable read spends neither.** It is a [`Receiver::Variable`], a reference into
/// [`Variables`], and [`types`](super::types) answers each read once per request. Walking every
/// write of a name from every read is what this bound used to hold back, and it grew as 2^n.
///
/// [`Budget`] keeps separate counters because they count different things; only the limit is
/// shared.
const MAX_WIDTH: usize = 64;

/// How many arguments, or keywords, one call may write before its list is no claim.
///
/// - **Not [`MAX_WIDTH`].** A list's length is not a walk's depth: every argument is one fan-out
///   step from the call, so a long list costs its length and nothing more.
/// - **A guard against generated code, not a limit real code meets.** The six reference corpora
///   write at most 89 positionals and 132 keywords in one call, and 72 calls
///   pass 20 or more, which the old bound of [`MAX_WIDTH`] emptied.
const MAX_ARGUMENTS: usize = 1024;

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
    /// The buffer with the half-typed `.` blanked out, where Prism took its message from a later
    /// token.
    ///
    /// **Which variable a read is depends on the file's structure**, and a dangling `.` above an
    /// `end` makes Prism read the `end` as a method name and re-nest everything below it. So the
    /// variables a completion's receiver reads are looked up in this text, not the buffer (see
    /// [`Finder::without_the_half_typed_call`]). Every offset is the buffer's: only the operator
    /// changed, to a space. `None` where the line parses as written, the usual case.
    pub repaired: Option<String>,
}

/// What the cursor at `offset` is completing.
///
/// `None` where Ruby cannot be written: inside a comment, or a string, symbol or regexp literal.
/// Constants in the middle of an error message are worse than nothing, and the server, unlike a
/// client word list, can tell.
#[must_use]
pub fn at(text: &Parsed<'_>, offset: u32) -> Option<Cursor> {
    let (source, result) = (text.source(), text.result());
    if in_comment(result, offset) {
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
    let repaired = match finder.without_the_half_typed_call() {
        Cow::Owned(repaired) => Some(repaired),
        Cow::Borrowed(_) => None,
    };
    Some(Cursor {
        repaired,
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
    let result = parse(source);
    let mut finder = Finder::new(source, offset);
    finder.visit(&result.node());
    finder.arguments
}

/// The first argument of the call whose method name starts at `name`, where it is a Symbol:
/// `create(:user, admin: true)` is `user`. What a literal-picked member's place is looked up by.
#[must_use]
pub fn first_symbol(source: &str, name: u32) -> Option<String> {
    struct Found<'s> {
        name: u32,
        symbol: Option<String>,
        source: std::marker::PhantomData<&'s ()>,
    }
    impl<'pr> Visit<'pr> for Found<'_> {
        fn visit_call_node(&mut self, node: &ruby_prism::CallNode<'pr>) {
            if node
                .message_loc()
                .is_some_and(|message| message.start_offset() as u32 == self.name)
            {
                self.symbol = node
                    .arguments()
                    .and_then(|arguments| arguments.arguments().iter().next())
                    .and_then(|first| {
                        String::from_utf8(first.as_symbol_node()?.unescaped().to_vec()).ok()
                    });
                return;
            }
            ruby_prism::visit_call_node(self, node);
        }
    }
    let result = parse(source);
    let mut found = Found {
        name,
        symbol: None,
        source: std::marker::PhantomData,
    };
    found.visit(&result.node());
    found.symbol
}

/// What the constant whose name is written at `name` was assigned, as a shape.
///
/// The syntax half of the constant-assignment rung, and an entry point that reads a file the cursor
/// is not in. An application's config constant is typed by `CONFIG = Settings.new` in an
/// initializer, and [`types`](super::types) knows which file.
///
/// - **`name` is a span, not a name**, which makes this exact. rubydex files a
///   `Definition::Constant` under the span of its last segment (`Foo::BAR = x` at the `BAR`), and
///   the caller takes the span from that definition. Same-spelled constants in two namespaces are
///   two spans, and a mere mention is never a recorded span.
/// - **`None`** where the span names no assignment in this text, or the assignment is a shape
///   nothing could come of. The first is normal for a buffer
///   edited since indexing: the span no longer names the same bytes, and this refuses rather than
///   reading whatever constant is there now.
#[must_use]
pub fn constant_assignment(source: &str, name: (u32, u32)) -> Option<Receiver> {
    let result = parse(source);
    // No cursor in this file: `u32::MAX` is past every offset, so nothing is being typed and the
    // walk only collects assignments.
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    let write = finder
        .constant_writes
        .iter()
        .find(|write| write.name == name)?;
    let receiver = finder.receiver_of(Some(&write.value), Budget::default());
    (!matches!(receiver, Receiver::Unknown)).then_some(receiver)
}

/// What one text writes into its constants, as a list of names held in one ([`frozen_constants`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FrozenConstants {
    /// Each constant assigned a literal of names frozen as written (`KEYS = %i[a b].freeze`), by
    /// the span of its name's last segment, where rubydex files its `Definition::Constant`.
    pub frozen: HashMap<(u32, u32), Rc<Names>>,
    /// Each constant written again with an operator (`KEYS += [:c]`, `KEYS &&= x`), by the same
    /// span: rubydex records these as a reference to the constant, not as a definition of it.
    pub rewritten: HashSet<(u32, u32)>,
}

/// Every constant written as a value in one text, by its name's span (a path's last segment, where
/// rubydex places the reference): a class handed to code that may build it where nothing here
/// shows (`mount_uploader :cover, CoverUploader`, `serializer: AccountSerializer`, `FOO = Bar`).
///
/// **Not a value:** a call's receiver (`Bar.new`), a path's namespace (`Bar::Baz`), a class or
/// module's name and a superclass, a `rescue`'s classes, a `when`'s, and what `is_a?`, `kind_of?`,
/// `instance_of?`, `include`, `extend` and `prepend` are given: each asks about the class or mixes
/// it in, and builds nothing. **Nor is a constant whose value only ever becomes a receiver**:
/// through parentheses, a conditional's branches or `||` (`(a ? Foo : Bar).new`), or held in a
/// local of one `def` every read of which is a call's receiver (`klass = Foo; klass.new`).
#[must_use]
pub fn constants_handed_on(source: &str) -> HashSet<(u32, u32)> {
    const ASKING: [&[u8]; 6] = [
        b"is_a?",
        b"kind_of?",
        b"instance_of?",
        b"include",
        b"extend",
        b"prepend",
    ];
    // A constant's name span: the node's for a name, the last segment's for a path.
    fn named(node: &Node<'_>) -> Option<(u32, u32)> {
        if let Some(read) = node.as_constant_read_node() {
            return Some(span_of(&read.as_node()));
        }
        let path = node.as_constant_path_node()?;
        let name = path.name_loc();
        Some((name.start_offset() as u32, name.end_offset() as u32))
    }
    // What a value can be, through parentheses, a conditional's branches and `||`/`&&`.
    fn flowing<'pr>(node: Node<'pr>, out: &mut Vec<Node<'pr>>) {
        if let Some(parentheses) = node.as_parentheses_node() {
            if let Some(body) = parentheses.body() {
                flowing(body, out);
            }
        } else if let Some(statements) = node.as_statements_node() {
            if let Some(last) = statements.body().iter().last() {
                flowing(last, out);
            }
        } else if let Some(branch) = node.as_if_node() {
            if let Some(statements) = branch.statements() {
                flowing(statements.as_node(), out);
            }
            if let Some(other) = branch.subsequent() {
                flowing(other, out);
            }
        } else if let Some(other) = node.as_else_node() {
            if let Some(statements) = other.statements() {
                flowing(statements.as_node(), out);
            }
        } else if let Some(either) = node.as_or_node() {
            flowing(either.left(), out);
            flowing(either.right(), out);
        } else if let Some(both) = node.as_and_node() {
            flowing(both.left(), out);
            flowing(both.right(), out);
        } else {
            out.push(node);
        }
    }
    /// The constants written to one local, and whether a read of it is anything but a receiver.
    type Local = (Vec<(u32, u32)>, bool);
    struct Walk {
        all: Vec<(u32, u32)>,
        not: HashSet<(u32, u32)>,
        /// Where each enclosing `def` starts, innermost last: a local's scope.
        defs: Vec<u32>,
        /// Where each local read that is a call's receiver starts.
        receiving: HashSet<u32>,
        /// Each local, by its `def` and name: the constants written to it, and whether any read
        /// of it is something other than a receiver.
        locals: HashMap<(u32, Vec<u8>), Local>,
    }
    impl Walk {
        fn not(&mut self, node: &Node<'_>) {
            if let Some(span) = named(node) {
                self.not.insert(span);
            }
        }
        fn local(&mut self, name: &[u8]) -> &mut Local {
            let scope = self.defs.last().copied().unwrap_or(u32::MAX);
            self.locals.entry((scope, name.to_vec())).or_default()
        }
        fn written(&mut self, name: &[u8], value: Node<'_>) {
            let mut leaves = Vec::new();
            flowing(value, &mut leaves);
            let constants: Vec<(u32, u32)> = leaves.iter().filter_map(named).collect();
            if !constants.is_empty() {
                self.local(name).0.extend(constants);
            }
        }
    }
    impl<'pr> Visit<'pr> for Walk {
        fn visit_constant_read_node(&mut self, node: &ConstantReadNode<'pr>) {
            self.all.extend(named(&node.as_node()));
        }
        fn visit_def_node(&mut self, node: &DefNode<'pr>) {
            self.defs.push(node.location().start_offset() as u32);
            ruby_prism::visit_def_node(self, node);
            self.defs.pop();
        }
        fn visit_local_variable_write_node(&mut self, node: &LocalVariableWriteNode<'pr>) {
            self.written(node.name().as_slice(), node.value());
            ruby_prism::visit_local_variable_write_node(self, node);
        }
        fn visit_local_variable_or_write_node(&mut self, node: &LocalVariableOrWriteNode<'pr>) {
            self.written(node.name().as_slice(), node.value());
            ruby_prism::visit_local_variable_or_write_node(self, node);
        }
        fn visit_local_variable_read_node(&mut self, node: &LocalVariableReadNode<'pr>) {
            if !self
                .receiving
                .contains(&(node.location().start_offset() as u32))
            {
                self.local(node.name().as_slice()).1 = true;
            }
        }
        fn visit_constant_path_node(&mut self, node: &ConstantPathNode<'pr>) {
            self.all.extend(named(&node.as_node()));
            if let Some(parent) = node.parent() {
                self.not(&parent);
            }
            ruby_prism::visit_constant_path_node(self, node);
        }
        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            if let Some(receiver) = node.receiver() {
                let mut leaves = Vec::new();
                flowing(receiver, &mut leaves);
                for leaf in &leaves {
                    self.not(leaf);
                    if let Some(read) = leaf.as_local_variable_read_node() {
                        self.receiving.insert(read.location().start_offset() as u32);
                    }
                }
            }
            if ASKING.contains(&node.name().as_slice())
                && let Some(arguments) = node.arguments()
            {
                for argument in arguments.arguments().iter() {
                    self.not(&argument);
                }
            }
            ruby_prism::visit_call_node(self, node);
        }
        fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
            self.not(&node.constant_path());
            if let Some(superclass) = node.superclass() {
                self.not(&superclass);
            }
            ruby_prism::visit_class_node(self, node);
        }
        fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
            self.not(&node.constant_path());
            ruby_prism::visit_module_node(self, node);
        }
        fn visit_rescue_node(&mut self, node: &ruby_prism::RescueNode<'pr>) {
            for exception in node.exceptions().iter() {
                self.not(&exception);
            }
            ruby_prism::visit_rescue_node(self, node);
        }
        fn visit_when_node(&mut self, node: &ruby_prism::WhenNode<'pr>) {
            for condition in node.conditions().iter() {
                self.not(&condition);
            }
            ruby_prism::visit_when_node(self, node);
        }
    }
    let result = parse(source);
    let mut walk = Walk {
        all: Vec::new(),
        not: HashSet::new(),
        defs: Vec::new(),
        receiving: HashSet::new(),
        locals: HashMap::new(),
    };
    walk.visit(&result.node());
    for (constants, read_otherwise) in walk.locals.into_values() {
        if !read_otherwise {
            walk.not.extend(constants);
        }
    }
    walk.all
        .into_iter()
        .filter(|span| !walk.not.contains(span))
        .collect()
}

/// [`FrozenConstants`] for one text, from one parse.
///
/// - **Frozen as written, nothing less.** `.freeze` with no argument and no block on an array or a
///   hash literal of names ([`Names`]): an unfrozen list changes under any `KEYS << :x`, anywhere.
///   What `.freeze` leaves open is a literal nested inside, which only code reaching into the
///   constant by index changes.
/// - **The four definitions rubydex files** (`=` and `||=`, on a name or a path), and the two
///   operator writes it does not (`op=` and `&&=`), whose value is a new object under the old name.
#[must_use]
pub fn frozen_constants(source: &str) -> FrozenConstants {
    struct Walk(FrozenConstants);
    fn span_of_location(location: &Location<'_>) -> (u32, u32) {
        (location.start_offset() as u32, location.end_offset() as u32)
    }
    impl Walk {
        fn assigned(&mut self, name: &Location<'_>, value: &Node<'_>) {
            let frozen = value.as_call_node().filter(|call| {
                call.name().as_slice() == b"freeze"
                    && call.arguments().is_none()
                    && call.block().is_none()
            });
            if let Some(names) = frozen
                .and_then(|call| call.receiver())
                .filter(|literal| {
                    literal.as_array_node().is_some() || literal.as_hash_node().is_some()
                })
                .and_then(|literal| Names::of(&literal, 0))
            {
                self.0.frozen.insert(span_of_location(name), Rc::new(names));
            }
        }
    }
    impl<'pr> Visit<'pr> for Walk {
        fn visit_constant_write_node(&mut self, node: &ConstantWriteNode<'pr>) {
            self.assigned(&node.name_loc(), &node.value());
            ruby_prism::visit_constant_write_node(self, node);
        }

        fn visit_constant_or_write_node(&mut self, node: &ConstantOrWriteNode<'pr>) {
            self.assigned(&node.name_loc(), &node.value());
            ruby_prism::visit_constant_or_write_node(self, node);
        }

        fn visit_constant_path_write_node(&mut self, node: &ConstantPathWriteNode<'pr>) {
            self.assigned(&node.target().name_loc(), &node.value());
            ruby_prism::visit_constant_path_write_node(self, node);
        }

        fn visit_constant_path_or_write_node(&mut self, node: &ConstantPathOrWriteNode<'pr>) {
            self.assigned(&node.target().name_loc(), &node.value());
            ruby_prism::visit_constant_path_or_write_node(self, node);
        }

        fn visit_constant_operator_write_node(&mut self, node: &ConstantOperatorWriteNode<'pr>) {
            self.0.rewritten.insert(span_of_location(&node.name_loc()));
            ruby_prism::visit_constant_operator_write_node(self, node);
        }

        fn visit_constant_and_write_node(&mut self, node: &ConstantAndWriteNode<'pr>) {
            self.0.rewritten.insert(span_of_location(&node.name_loc()));
            ruby_prism::visit_constant_and_write_node(self, node);
        }

        fn visit_constant_path_operator_write_node(
            &mut self,
            node: &ConstantPathOperatorWriteNode<'pr>,
        ) {
            self.0
                .rewritten
                .insert(span_of_location(&node.target().name_loc()));
            ruby_prism::visit_constant_path_operator_write_node(self, node);
        }

        fn visit_constant_path_and_write_node(&mut self, node: &ConstantPathAndWriteNode<'pr>) {
            self.0
                .rewritten
                .insert(span_of_location(&node.target().name_loc()));
            ruby_prism::visit_constant_path_and_write_node(self, node);
        }
    }
    let result = parse(source);
    let mut walk = Walk(FrozenConstants::default());
    walk.visit(&result.node());
    walk.0
}

/// The instance variable spanned by `span`, as a shape a type can be looked up from.
///
/// - **[`at`] reaches an instance variable only as the thing left of a `.`.** This asks about the
///   variable itself (the cursor on `@story`, nothing after it) and returns the same [`Receiver`],
///   so cards on `@story` and `@story.title` cannot disagree on the type or its rung.
/// - **`span` is the name span [`scopes`] returns, `@` included.** Which `@foo` this is is not a
///   syntax question ([`scopes`] answers it for [`Variables`]);
///   [`locator::variable_at`](super::locator::variable_at) has already placed the cursor.
#[must_use]
pub fn instance_variable(source: &str, span: (u32, u32)) -> Receiver {
    // A read like any other: the table built from this text says what reaches it. A write's own
    // name is a row too, so a card on `@story = …` answers what `@story` holds.
    Receiver::Spelled {
        was: Box::new(Receiver::Variable(span.0)),
        name: source[span.0 as usize..span.1 as usize].to_owned(),
    }
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
    shapes(source).returns
}

/// One `def`'s optional parameters, each with the span its default expression is written at.
pub type WrittenDefaults = Vec<(ParameterSlot, (u32, u32))>;

/// Everything the type side reads out of one text, from one parse: what every `def` returns, what
/// can reach every variable read, every optional parameter's default, and which `def`s raise.
///
/// **One value, because the parts refer to each other.** An exit that reads a local is a
/// [`Receiver::Variable`], a row of this text's [`Variables`]. Built apart, they could describe
/// different texts.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Shapes {
    /// Every `def`'s exits, by the `def`'s span; see [`returns_in`].
    pub returns: HashMap<(u32, u32), Vec<Receiver>>,
    pub variables: Variables,
    /// Every `def`'s optional parameters' defaults, by the `def`'s span and the parameter's slot.
    ///
    /// Read only for a call that left the argument out: at that call the parameter
    /// holds exactly its default, so the default's type is its type there, and nowhere else.
    pub defaults: HashMap<(u32, u32), Vec<(ParameterSlot, Receiver)>>,
    /// The same defaults as written: where each expression's text is, for a card to print
    /// `limit = 10` where rubydex records only that `limit` is optional.
    pub written_defaults: HashMap<(u32, u32), WrittenDefaults>,
    /// Every `def` whose own body writes `raise` or `fail` ([`raises_in`]), by the `def`'s span.
    pub raising: HashSet<(u32, u32)>,
    /// Every block and lambda passed to a call, with the call ([`BlockSite`]), in source order.
    pub blocks: Vec<BlockSite>,
    /// Every body a `self` belongs to (a `def`, a `class`, a `module`, a `class << self`, and the
    /// file itself), for [`Shapes::rebinding`] to tell whose `self` an offset has.
    pub bodies: Vec<(u32, u32)>,
    /// Every `def`'s `yield`s, by the `def`'s span: what each hands its block, positionally
    /// ([`YieldSites`]). `None` where one cannot be read (a splat, keywords, a block
    /// argument), which leaves no position certain.
    pub yields: HashMap<(u32, u32), Option<Vec<Vec<Receiver>>>>,
    /// Every proc and lambda literal, by where it starts ([`Receiver::Proc`]).
    pub procs: HashMap<u32, ProcShape>,
    /// Every `def` that asks whether it was passed a block (`block_given?`, or a read of its own
    /// `&block` in a condition), by the `def`'s span: the ones a call's block changes what is
    /// reached ([`Receiver::BlockGiven`]).
    pub asks: HashSet<(u32, u32)>,
    /// Every call of a writer (`Current.account = x`, `self.account ||= x`, `config.x = y`), in
    /// source order ([`Setter`]): what an accessor is given.
    pub setters: Vec<Setter>,
    /// Every call that calls a method by a name it is handed (`public_send(:"#{k}=", v)`), in
    /// source order ([`Sent`]): a writer's call no `x.name =` spells, and an action's
    /// no `show` spells.
    pub sends: Vec<Sent>,
}

/// One call of a writer on a receiver: `Current.account = x`, `Current.account ||= x`,
/// `self.account = x` or `config.dispatcher = x`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setter {
    /// Where the call starts: whose scope the receiver and the value are read in.
    pub at: u32,
    /// The member written, without its `=`.
    pub name: String,
    /// What it is called on.
    pub on: Receiver,
    /// What it is given.
    pub value: Receiver,
    /// Written as a keyword of `set(name: value)`, `**` for a double splat: the
    /// writes `ActiveSupport::CurrentAttributes.set` makes for its block.
    pub set: bool,
}

/// One call that calls a method by name: `send`, `public_send`, `__send__`, `try` or `try!`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    /// Where the call starts: whose scope the receiver is read in.
    pub at: u32,
    /// The names it can send: a literal's, an interpolation's, or those of a local every write in
    /// its `def` spells. Anything else can be any name.
    pub name: scopes::Spelled,
    /// How many arguments it passes after the name, or `None` where a splat or `...` can be any
    /// number.
    pub values: Option<usize>,
    /// What it is sent to: the [`Receiver::SelfObject`] at the call for a receiverless one.
    pub on: Receiver,
}

impl Sent {
    /// Whether it can call a writer: a name that can end in `=`, and one value.
    #[must_use]
    pub fn may_write(&self) -> bool {
        self.name.may_name_a_writer() && self.values.is_none_or(|values| values == 1)
    }
}

/// One proc or lambda literal: how it binds what a call passes, and what it hands back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcShape {
    /// A lambda binds strictly: a count its parameters do not take raises. A proc drops extras
    /// and fills missing ones with `nil` or their default.
    pub lambda: bool,
    /// `None` where the list binds in a way positions cannot follow: a destructured parameter,
    /// a required one after a `*rest` or an optional one, or keywords.
    pub parameters: Option<ProcParameters>,
    /// What it hands back: its tail and every `next`, and for a lambda every `return` and `break`
    /// too. One `Unknown` where a proc `return`s or `break`s (which leaves the method that wrote
    /// it) or anything else cannot be read.
    pub exits: Vec<Receiver>,
}

/// A proc literal's positional parameters, as Ruby binds them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcParameters {
    /// The leading required ones.
    pub required: usize,
    /// Each optional one's default, after them.
    pub defaults: Vec<Receiver>,
    /// A `*rest` takes what is left, so a lambda accepts any count above `required`.
    pub rest: bool,
    /// Ruby unpacks one `Array` handed to it ([`BlockParameter::spreads`]'s rule, for a proc).
    pub spreads: bool,
}

impl Shapes {
    /// The blocks and lambdas around `offset` that a call might run with another `self`, innermost
    /// first.
    ///
    /// Only those inside the innermost body around `offset`: a `def` written inside a block has
    /// its own `self`, which no block outside it changes.
    #[must_use]
    pub fn rebinding(&self, offset: u32) -> Vec<&BlockSite> {
        let inside = |span: &(u32, u32)| span.0 <= offset && offset < span.1;
        let Some(body) = self
            .bodies
            .iter()
            .filter(|body| inside(body))
            .min_by_key(|body| body.1 - body.0)
        else {
            return Vec::new();
        };
        let mut found: Vec<&BlockSite> = self
            .blocks
            .iter()
            .filter(|site| site.body == *body && inside(&site.span))
            .collect();
        found.sort_by_key(|site| site.span.1 - site.span.0);
        found
    }
}

/// A block or lambda passed to a call: the call, and which of its arguments the block is, so the
/// type side can ask the call's signature what `self` is inside it (`types::Types::selves`).
///
/// - **A shape, like everything here.** Whether `configure` runs its block against the
///   application is a signature's fact, and signatures are the type side's.
/// - **A lambda counts where it is an argument** (`scope :recent, -> { … }`, `if: -> { … }`), and
///   so does a `proc { … }` or `lambda { … }` written there. A lambda anywhere else runs wherever it
///   is called, which the text does not say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSite {
    /// The block's or lambda's own span.
    pub span: (u32, u32),
    /// What the call was written on: the [`Receiver::SelfObject`] at the call for a receiverless
    /// one.
    pub on: Receiver,
    /// The method's name as written.
    pub method: String,
    /// Which argument the block or lambda is.
    pub slot: BlockSlot,
    /// Where the call starts, which is where its receiver is resolved.
    pub call: u32,
    /// The innermost body around the call ([`Shapes::bodies`]).
    pub body: (u32, u32),
}

/// One name a sending call builds ([`sent_patterns`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SentPattern {
    /// The literal start and end of every name it can build.
    pub head: String,
    pub tail: String,
    /// How many positional arguments it passes after the name: `None` where any.
    pub passed: Option<u8>,
    /// Where the call starts, where it is sent to `self`.
    pub on_self: Option<u32>,
}

/// Every name a call of one of `senders` builds around an interpolation in its
/// first argument (`send("handle_#{event}", payload)`, `try(:"#{name}_was")`), as the literal head
/// and tail it keeps, and how many positional arguments it passes after the name: `None` where a
/// splat or a keyword could pass any. A name with no literal part is left out: it could be any
/// name.
#[must_use]
pub fn sent_patterns(source: &str, senders: &[&str]) -> Vec<SentPattern> {
    struct Walk<'s> {
        senders: &'s [&'s str],
        found: Vec<SentPattern>,
    }
    impl<'pr> Visit<'pr> for Walk<'_> {
        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            let name = String::from_utf8_lossy(node.name().as_slice());
            if self.senders.contains(&name.as_ref())
                && let Some(written) = node.arguments()
                && let Some(first) = written.arguments().iter().next()
                && let scopes::Spelled::Like { head, tail } = scopes::spelled_by(&first)
                && (!head.is_empty() || !tail.is_empty())
            {
                let rest: Vec<Node<'_>> = written.arguments().iter().skip(1).collect();
                let plain = rest.iter().all(|argument| {
                    argument.as_splat_node().is_none()
                        && argument.as_keyword_hash_node().is_none()
                        && argument.as_forwarding_arguments_node().is_none()
                });
                // A `Method` looked up by name is called later, with whatever its caller passes.
                let looked_up = name.ends_with("method");
                let passed = (plain && !looked_up)
                    .then(|| u8::try_from(rest.len()).ok())
                    .flatten();
                // Sent to `self` (no receiver, or `self.`): only `self`'s own methods can answer.
                let on_self = node
                    .receiver()
                    .is_none_or(|receiver| receiver.as_self_node().is_some());
                self.found.push(SentPattern {
                    head,
                    tail,
                    passed,
                    on_self: on_self.then_some(node.location().start_offset() as u32),
                });
            }
            ruby_prism::visit_call_node(self, node);
        }
    }
    let result = parse(source);
    let mut walk = Walk {
        senders,
        found: Vec::new(),
    };
    walk.visit(&result.node());
    walk.found.sort();
    walk.found.dedup();
    walk.found
}

/// One literal that spells a method's name, and where it is written ([`spelled_uses`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpelledUse {
    /// Where the literal starts.
    pub at: u32,
    /// A string, not a symbol.
    pub text: bool,
    pub how: Spelling,
}

/// Where a literal spelling a method's name is written. What that does with the name is
/// [`types`](super::types)' question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spelling {
    /// An argument of a call written with a name: a positional, a hash's value under `key`, or an
    /// element of an array that is one. `first` is where the call's name starts, where the literal
    /// is the call's first positional itself (`send(:greet, x)`).
    Argument {
        call: String,
        key: Option<String>,
        first: Option<u32>,
    },
    /// Passed as the block: `map(&:greet)`.
    BlockPass,
    /// Compared or named, never sent: a `when` condition, a hash's key, an `alias` or `undef`.
    Compared,
    /// Anywhere else: assigned, returned, a receiver, an element of a collection held.
    Held,
}

/// Every symbol, `%i[]` word and one-word string in one text that spells a method's name, by the
/// name, and where each is written, from one parse: what the callers rung reads in a document the
/// indexer saw spell a name ([`super::indexer::named`]). A part of an interpolation is no literal.
#[must_use]
pub fn spelled_uses(source: &str) -> HashMap<String, Vec<SpelledUse>> {
    #[derive(Default)]
    struct Walk {
        found: HashMap<String, Vec<SpelledUse>>,
        claimed: HashSet<u32>,
    }
    impl Walk {
        fn note(&mut self, node: &Node<'_>, how: impl FnOnce() -> Spelling) {
            let at = node.location().start_offset() as u32;
            let Some(name) = spelled_name(node) else {
                return;
            };
            if self.claimed.insert(at) {
                let text = node.as_string_node().is_some();
                self.found.entry(name).or_default().push(SpelledUse {
                    at,
                    text,
                    how: how(),
                });
            }
        }

        fn claim(&mut self, node: &Node<'_>) {
            self.claimed.insert(node.location().start_offset() as u32);
        }

        fn argument(&mut self, node: &Node<'_>, call: &str, key: Option<&str>, first: Option<u32>) {
            if let Some(array) = node.as_array_node() {
                for element in array.elements().iter() {
                    self.argument(&element, call, key, None);
                }
            } else if let Some(hash) = node.as_hash_node() {
                self.pairs(hash.elements().iter(), call);
            } else {
                self.note(node, || Spelling::Argument {
                    call: call.to_owned(),
                    key: key.map(str::to_owned),
                    first,
                });
            }
        }

        fn pairs<'pr>(&mut self, elements: impl Iterator<Item = Node<'pr>>, call: &str) {
            for element in elements {
                let Some(pair) = element.as_assoc_node() else {
                    continue;
                };
                let key = pair.key();
                self.note(&key, || Spelling::Compared);
                let key = key
                    .as_symbol_node()
                    .map(|symbol| String::from_utf8_lossy(symbol.unescaped()).into_owned());
                self.argument(&pair.value(), call, key.as_deref(), None);
            }
        }

        fn parts<'pr>(&mut self, parts: impl Iterator<Item = Node<'pr>>) {
            for part in parts.filter(|part| part.as_string_node().is_some()) {
                self.claim(&part);
            }
        }
    }
    impl<'pr> Visit<'pr> for Walk {
        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            let call = String::from_utf8_lossy(node.name().as_slice()).into_owned();
            let first = node.message_loc().map(|name| name.start_offset() as u32);
            if let Some(arguments) = node.arguments() {
                for (index, argument) in arguments.arguments().iter().enumerate() {
                    if let Some(hash) = argument.as_keyword_hash_node() {
                        self.pairs(hash.elements().iter(), &call);
                    } else {
                        self.argument(&argument, &call, None, first.filter(|_| index == 0));
                    }
                }
            }
            if let Some(passed) = node
                .block()
                .and_then(|block| block.as_block_argument_node())
                && let Some(expression) = passed.expression()
            {
                self.note(&expression, || Spelling::BlockPass);
            }
            ruby_prism::visit_call_node(self, node);
        }

        fn visit_when_node(&mut self, node: &ruby_prism::WhenNode<'pr>) {
            for condition in node.conditions().iter() {
                self.note(&condition, || Spelling::Compared);
            }
            ruby_prism::visit_when_node(self, node);
        }

        fn visit_assoc_node(&mut self, node: &AssocNode<'pr>) {
            self.note(&node.key(), || Spelling::Compared);
            ruby_prism::visit_assoc_node(self, node);
        }

        fn visit_alias_method_node(&mut self, node: &ruby_prism::AliasMethodNode<'pr>) {
            self.note(&node.new_name(), || Spelling::Compared);
            self.note(&node.old_name(), || Spelling::Compared);
            ruby_prism::visit_alias_method_node(self, node);
        }

        fn visit_undef_node(&mut self, node: &ruby_prism::UndefNode<'pr>) {
            for name in node.names().iter() {
                self.note(&name, || Spelling::Compared);
            }
            ruby_prism::visit_undef_node(self, node);
        }

        fn visit_interpolated_string_node(
            &mut self,
            node: &ruby_prism::InterpolatedStringNode<'pr>,
        ) {
            self.parts(node.parts().iter());
            ruby_prism::visit_interpolated_string_node(self, node);
        }

        fn visit_interpolated_symbol_node(
            &mut self,
            node: &ruby_prism::InterpolatedSymbolNode<'pr>,
        ) {
            self.parts(node.parts().iter());
            ruby_prism::visit_interpolated_symbol_node(self, node);
        }

        fn visit_interpolated_x_string_node(
            &mut self,
            node: &ruby_prism::InterpolatedXStringNode<'pr>,
        ) {
            self.parts(node.parts().iter());
            ruby_prism::visit_interpolated_x_string_node(self, node);
        }

        fn visit_interpolated_regular_expression_node(
            &mut self,
            node: &ruby_prism::InterpolatedRegularExpressionNode<'pr>,
        ) {
            self.parts(node.parts().iter());
            ruby_prism::visit_interpolated_regular_expression_node(self, node);
        }

        fn visit_symbol_node(&mut self, node: &SymbolNode<'pr>) {
            self.note(&node.as_node(), || Spelling::Held);
        }

        fn visit_string_node(&mut self, node: &StringNode<'pr>) {
            self.note(&node.as_node(), || Spelling::Held);
        }
    }
    let result = parse(source);
    let mut walk = Walk::default();
    walk.visit(&result.node());
    walk.found
}

/// The method name a symbol or a plain string literal spells: a word, with an optional `?`, `!`
/// or `=` at its end, as the indexer's scan reads one.
fn spelled_name(node: &Node<'_>) -> Option<String> {
    let text: Vec<u8> = if let Some(symbol) = node.as_symbol_node() {
        symbol.unescaped().to_vec()
    } else {
        node.as_string_node()?.unescaped().to_vec()
    };
    let (&head, rest) = text.split_first()?;
    let body = match rest.last() {
        Some(b'?' | b'!' | b'=') => &rest[..rest.len() - 1],
        _ => rest,
    };
    let word = (head.is_ascii_alphabetic() || head == b'_')
        && body
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
    word.then(|| String::from_utf8_lossy(&text).into_owned())
}

/// A module a hook mixes into what it is handed ([`hook_mixins`]):
/// `def self.included(base) = base.extend(ClassMethods)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookMixin {
    /// Where the hook's `def` starts.
    pub hook_at: u32,
    /// `included`, `extended` or `prepended`.
    pub hook: String,
    /// `include`, `prepend` or `extend`.
    pub mixer: String,
    /// The constant mixed in, by where its path ends, as [`Receiver::Constant`] places one; `None`
    /// for `self`, the hook's own module.
    pub mixed: Option<u32>,
}

/// Every module a hook mixes into what it is handed, in one text, from one parse: in a
/// `def self.included(base)`, `self.extended(base)` or `self.prepended(base)` with that one
/// parameter, outside a nested `def`, a call of `include`, `prepend` or `extend` on the parameter
/// (or of a sender whose first argument spells one), one row per constant or `self` it is handed.
#[must_use]
pub fn hook_mixins(source: &str) -> Vec<HookMixin> {
    struct Walk {
        found: Vec<HookMixin>,
        /// The hook being walked: where its `def` starts, its name and its parameter's.
        hook: Option<(u32, String, Vec<u8>)>,
    }
    fn only_parameter(node: &DefNode<'_>) -> Option<Vec<u8>> {
        let parameters = node.parameters()?;
        let others = parameters.optionals().iter().next().is_some()
            || parameters.rest().is_some()
            || parameters.posts().iter().next().is_some()
            || parameters.keywords().iter().next().is_some()
            || parameters.keyword_rest().is_some();
        let mut requireds = parameters.requireds().iter();
        let one = requireds.next()?;
        if others || requireds.next().is_some() {
            return None;
        }
        Some(one.as_required_parameter_node()?.name().as_slice().to_vec())
    }
    impl<'pr> Visit<'pr> for Walk {
        fn visit_def_node(&mut self, node: &DefNode<'pr>) {
            let name = String::from_utf8_lossy(node.name().as_slice()).into_owned();
            let hook = node
                .receiver()
                .is_some_and(|receiver| receiver.as_self_node().is_some())
                && matches!(name.as_str(), "included" | "extended" | "prepended");
            let inner = hook
                .then(|| only_parameter(node))
                .flatten()
                .map(|parameter| (node.location().start_offset() as u32, name, parameter));
            let outer = std::mem::replace(&mut self.hook, inner);
            ruby_prism::visit_def_node(self, node);
            self.hook = outer;
        }

        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            if let Some((at, hook, parameter)) = &self.hook
                && node
                    .receiver()
                    .and_then(|receiver| receiver.as_local_variable_read_node())
                    .is_some_and(|read| read.name().as_slice() == parameter.as_slice())
            {
                let name = String::from_utf8_lossy(node.name().as_slice()).into_owned();
                let arguments: Vec<Node<'pr>> = node
                    .arguments()
                    .map(|arguments| arguments.arguments().iter().collect())
                    .unwrap_or_default();
                let mixer = match name.as_str() {
                    "include" | "prepend" | "extend" => Some((name, 0)),
                    "send" | "public_send" | "__send__" => arguments
                        .first()
                        .and_then(spelled_name)
                        .filter(|named| matches!(named.as_str(), "include" | "prepend" | "extend"))
                        .map(|named| (named, 1)),
                    _ => None,
                };
                if let Some((mixer, skip)) = mixer {
                    for argument in arguments.iter().skip(skip) {
                        let mixed = if argument.as_self_node().is_some() {
                            None
                        } else if argument.as_constant_read_node().is_some()
                            || argument.as_constant_path_node().is_some()
                        {
                            Some(argument.location().end_offset() as u32)
                        } else {
                            continue;
                        };
                        self.found.push(HookMixin {
                            hook_at: *at,
                            hook: hook.clone(),
                            mixer: mixer.clone(),
                            mixed,
                        });
                    }
                }
            }
            ruby_prism::visit_call_node(self, node);
        }
    }
    let result = parse(source);
    let mut walk = Walk {
        found: Vec::new(),
        hook: None,
    };
    walk.visit(&result.node());
    walk.found
}

/// Every call with a written name in one text, as its shape, by where the name starts, from one
/// parse: what the callers rung reads a caller document for.
#[must_use]
pub fn every_call(source: &str) -> HashMap<u32, Receiver> {
    let result = parse(source);
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    calls_of(&result.node(), &finder)
}

/// One `super` written in a `def`: the call it makes of the method above ([`supers`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuperSite {
    /// Where the keyword starts: the scope its arguments are read in.
    pub at: u32,
    /// The enclosing `def`'s span.
    pub def: (u32, u32),
    /// What it passes, counted as a call's arguments are ([`Arity`]).
    pub arity: Arity,
    /// Each positional it passes, as [`Receiver::Returned`]'s `arguments`.
    pub arguments: Vec<Receiver>,
    /// The keywords it passes by name, as [`Receiver::Returned`]'s `keywords`.
    pub keywords: Option<Vec<(String, Receiver)>>,
}

/// Every `super` written in a `def` of one text, as the call it makes, from one parse: what an
/// object's `initialize` above an override is handed.
///
/// - **`super(a, b)` passes what it writes**, read as a call's arguments are.
/// - **A bare `super` passes the `def`'s parameters as they are where it runs**, in the `def`'s
///   own shape: each required and optional positional in order, each keyword by name, as
///   [`Receiver::Parameter`]. A parameter the `def` writes anywhere may hold something else by
///   then, so it passes [`Receiver::Unknown`]. A `*rest`, a positional after it, a `**rest`, `...`
///   or a destructured parameter passes a count nothing knows ([`Arity::Unknown`]).
/// - **A `super` in a nested `def` is that `def`'s**, and one in a block the `def`'s around it.
///   One outside every `def` (a `define_method` block) is left out: its name is the macro's.
#[must_use]
pub fn supers(source: &str) -> Vec<SuperSite> {
    // The `def`s by index, the ones open around the walk, and each `super` with its `def`'s.
    struct Walk<'pr> {
        defs: Vec<Node<'pr>>,
        open: Vec<usize>,
        found: Vec<(Node<'pr>, usize)>,
    }
    impl<'pr> Visit<'pr> for Walk<'pr> {
        fn visit_def_node(&mut self, node: &DefNode<'pr>) {
            self.open.push(self.defs.len());
            self.defs.push(node.as_node());
            ruby_prism::visit_def_node(self, node);
            self.open.pop();
        }
        fn visit_super_node(&mut self, node: &ruby_prism::SuperNode<'pr>) {
            if let Some(def) = self.open.last() {
                self.found.push((node.as_node(), *def));
            }
            ruby_prism::visit_super_node(self, node);
        }
        fn visit_forwarding_super_node(&mut self, node: &ruby_prism::ForwardingSuperNode<'pr>) {
            if let Some(def) = self.open.last() {
                self.found.push((node.as_node(), *def));
            }
            ruby_prism::visit_forwarding_super_node(self, node);
        }
    }
    let result = parse(source);
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    let mut walk = Walk {
        defs: Vec::new(),
        open: Vec::new(),
        found: Vec::new(),
    };
    walk.visit(&result.node());
    walk.found
        .iter()
        .filter_map(|(node, def)| {
            let def = walk.defs.get(*def)?.as_def_node()?;
            let span = span_of(&def.as_node());
            let at = node.location().start_offset() as u32;
            if let Some(written) = node.as_super_node() {
                let arguments = written.arguments();
                return Some(SuperSite {
                    at,
                    def: span,
                    arity: written_arity(arguments.as_ref()),
                    arguments: finder.listed_arguments(arguments.as_ref(), Budget::default()),
                    keywords: finder.listed_keywords(arguments.as_ref(), Budget::default()),
                });
            }
            let (arity, arguments, keywords) = forwarded_by(&finder, &def, span);
            Some(SuperSite {
                at,
                def: span,
                arity,
                arguments,
                keywords,
            })
        })
        .collect()
}

/// What a bare `super` in `def` passes: its parameters, as [`supers`] reads them.
type Forwarded = (Arity, Vec<Receiver>, Option<Vec<(String, Receiver)>>);

fn forwarded_by(finder: &Finder<'_, '_>, def: &DefNode<'_>, span: (u32, u32)) -> Forwarded {
    let unknown = (Arity::Unknown, Vec::new(), None);
    let Some(parameters) = def.parameters() else {
        return (Arity::Exactly(0), Vec::new(), Some(Vec::new()));
    };
    // A positional after a rest comes with the rest; `...` is a keyword rest.
    if parameters.rest().is_some() || parameters.keyword_rest().is_some() {
        return unknown;
    }
    let name = String::from_utf8_lossy(def.name().as_slice()).into_owned();
    let held = |slot: ParameterSlot, spelled: &[u8]| {
        let spelled = String::from_utf8_lossy(spelled);
        let written = finder
            .defs
            .iter()
            .find(|found| found.span == span)
            .is_none_or(|found| finder.written_in(found, &spelled));
        if written {
            Receiver::Unknown
        } else {
            Receiver::Parameter {
                at: span.0,
                method: name.clone(),
                slot,
            }
        }
    };
    let mut arguments = Vec::new();
    for (index, parameter) in parameters
        .requireds()
        .iter()
        .chain(parameters.optionals().iter())
        .enumerate()
    {
        let spelled = match (
            parameter.as_required_parameter_node(),
            parameter.as_optional_parameter_node(),
        ) {
            (Some(required), _) => required.name(),
            (_, Some(optional)) => optional.name(),
            // `def f((a, b))`: the destructured value is passed whole, which nothing names.
            _ => return unknown,
        };
        arguments.push(held(ParameterSlot::Positional(index), spelled.as_slice()));
    }
    // A keyword parameter is required or optional; a `**rest` was turned away above.
    let keywords: Vec<(String, Receiver)> = parameters
        .keywords()
        .iter()
        .filter_map(|parameter| {
            parameter
                .as_required_keyword_parameter_node()
                .map(|required| required.name())
                .or_else(|| {
                    parameter
                        .as_optional_keyword_parameter_node()
                        .map(|optional| optional.name())
                })
        })
        .map(|spelled| {
            let key = String::from_utf8_lossy(spelled.as_slice()).into_owned();
            let value = held(ParameterSlot::Keyword(key.clone()), spelled.as_slice());
            (key, value)
        })
        .collect();
    let count = arguments.len() as u32;
    let arity = if keywords.is_empty() {
        Arity::Exactly(count)
    } else {
        Arity::Keyed(count)
    };
    (arity, arguments, Some(keywords))
}

/// [`every_call`] of a tree and a cursor-less [`Finder`] walk of it.
fn calls_of(node: &Node<'_>, finder: &Finder<'_, '_>) -> HashMap<u32, Receiver> {
    struct Found<'pr> {
        calls: Vec<(u32, Node<'pr>)>,
    }
    impl<'pr> Visit<'pr> for Found<'pr> {
        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            if let Some(message) = node.message_loc() {
                self.calls
                    .push((message.start_offset() as u32, node.as_node()));
            }
            ruby_prism::visit_call_node(self, node);
        }
    }
    let mut found = Found { calls: Vec::new() };
    found.visit(node);
    found
        .calls
        .into_iter()
        .map(|(at, node)| (at, finder.receiver_of(Some(&node), Budget::default())))
        .collect()
}

/// [`every_call`] and [`shapes`] from one parse and one [`Finder`] walk: a document the callers
/// rung reads call shapes from is mostly one whose receivers and arguments it reads next.
#[must_use]
pub fn every_call_and_shapes(source: &str) -> (HashMap<u32, Receiver>, Shapes) {
    let result = parse(source);
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    (
        calls_of(&result.node(), &finder),
        shapes_of(source, &result.node(), &finder),
    )
}

/// [`Shapes`] for one text.
#[must_use]
pub fn shapes(source: &str) -> Shapes {
    let result = parse(source);
    // No cursor in this file: `u32::MAX` is past every offset, so nothing is being typed.
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    shapes_of(source, &result.node(), &finder)
}

/// What each expression written at one of `spans` is, as a shape, from one parse: the values a
/// render call hands a partial. A span no expression starts and ends at has no row.
///
/// A span that is a whole `k:` shorthand's value (`{ story: }`) is the local or call the key
/// names, which Prism wraps in an implicit node of the same span.
#[must_use]
pub fn values_at(source: &str, spans: &[(u32, u32)]) -> HashMap<(u32, u32), Receiver> {
    struct Spanned<'pr> {
        wanted: HashSet<(u32, u32)>,
        found: HashMap<(u32, u32), Node<'pr>>,
    }
    impl<'pr> Spanned<'pr> {
        fn record(&mut self, node: Node<'pr>) {
            let span = span_of(&node);
            if self.wanted.contains(&span)
                && node.as_implicit_node().is_none()
                && !self.found.contains_key(&span)
            {
                self.found.insert(span, node);
            }
        }
    }
    impl<'pr> Visit<'pr> for Spanned<'pr> {
        fn visit_branch_node_enter(&mut self, node: Node<'pr>) {
            self.record(node);
        }

        fn visit_leaf_node_enter(&mut self, node: Node<'pr>) {
            self.record(node);
        }
    }
    let result = parse(source);
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    let mut spanned = Spanned {
        wanted: spans.iter().copied().collect(),
        found: HashMap::new(),
    };
    spanned.visit(&result.node());
    spanned
        .found
        .into_iter()
        .map(|(span, node)| (span, finder.receiver_of(Some(&node), Budget::default())))
        .collect()
}

/// What the block written on each call hands back, by where the call starts: the body of a method a
/// call defines from its block, as `define_method(:x) { … }` does.
///
/// - **The block's own exits** ([`Finder::handed_back`]): its tail and every `next`.
/// - **A block with a `break` is left out.** In a method made from a block it raises, so no value
///   of the method would be right.
/// - **Every written block, in one walk**, because a reader asks about several calls of one text
///   and a parse per call would cost more than the ones nobody asks about.
#[must_use]
pub fn blocks_handed_back(source: &str) -> HashMap<u32, Box<[Receiver]>> {
    let result = parse(source);
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    let mut blocks = HandedBack {
        finder: &finder,
        found: HashMap::new(),
    };
    blocks.visit(&result.node());
    blocks.found
}

/// The one proc literal a call is passed as a positional argument ([`lambdas_handed_back`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandedLambda {
    /// Where the literal starts, as [`Receiver::ProcParameter`] names it.
    pub at: u32,
    /// What it hands back.
    pub exits: Box<[Receiver]>,
}

/// What the one proc literal each call is passed as a positional argument hands back, by where
/// the call starts: the body of a member a call defines from its lambda, as
/// `scope :recent, -> { … }` does.
///
/// - **The literal's own exits** ([`Finder::proc_shape`]): its tail, and a lambda's `next`,
///   `break` and `return`. A proc whose `return` or `break` leaves the method is unreadable.
/// - **Only a call passed exactly one**, since two leave which one is meant to the method.
#[must_use]
pub fn lambdas_handed_back(source: &str) -> HashMap<u32, HandedLambda> {
    struct Lambdas<'f, 'a, 'b> {
        finder: &'f Finder<'a, 'b>,
        found: HashMap<u32, HandedLambda>,
    }
    impl<'pr> Visit<'pr> for Lambdas<'_, '_, '_> {
        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            let literals: Vec<Node<'pr>> = node
                .arguments()
                .map(|arguments| {
                    arguments
                        .arguments()
                        .iter()
                        .filter(|argument| proc_literal(argument).is_some())
                        .collect()
                })
                .unwrap_or_default();
            if let [literal] = literals.as_slice()
                && let Some(at) = proc_literal(literal)
            {
                self.found.insert(
                    node.location().start_offset() as u32,
                    HandedLambda {
                        at,
                        exits: self.finder.proc_shape(literal).exits.into_boxed_slice(),
                    },
                );
            }
            ruby_prism::visit_call_node(self, node);
        }
    }
    let result = parse(source);
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    let mut lambdas = Lambdas {
        finder: &finder,
        found: HashMap::new(),
    };
    lambdas.visit(&result.node());
    lambdas.found
}

/// [`blocks_handed_back`]'s walk.
struct HandedBack<'f, 'a, 'b> {
    finder: &'f Finder<'a, 'b>,
    found: HashMap<u32, Box<[Receiver]>>,
}

impl<'pr> Visit<'pr> for HandedBack<'_, '_, '_> {
    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        if let Some(block) = node.block().as_ref().and_then(Node::as_block_node) {
            let (exits, breaks) = self.finder.handed_back(&block, Budget::default());
            if breaks.is_empty() {
                self.found
                    .insert(node.location().start_offset() as u32, exits);
            }
        }
        ruby_prism::visit_call_node(self, node);
    }
}

/// Which argument of a call a block or lambda was passed as: what a signature's `[self: T]` is
/// keyed by ([`BlockSite`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockSlot {
    /// The call's own block: `do … end` or `{ … }`.
    Block,
    /// A positional argument, counted from zero: `scope :recent, -> { … }` is `Positional(1)`.
    Positional(usize),
    /// A keyword argument, by name without the colon: `if: -> { … }`.
    Keyword(Box<str>),
}

/// One `def`'s place in the margin: its name, where its return label goes, and whether its own
/// code raises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefSite {
    /// The name's span: what the graph files the method under.
    pub name: (u32, u32),
    /// Where the label is drawn: past the closing parenthesis if there is one, past the parameters
    /// if written without any, and past the name if there are none. `def title(a)`, `def title a`
    /// and `def title` all end their signature differently, and a label at the name would land
    /// inside the first one's parameter list.
    pub at: u32,
    /// Its own code writes `raise` or `fail` ([`raises_in`]).
    pub raises: bool,
}

/// Everything the margin reads out of one text, from **one parse and one walk**.
pub struct Margin {
    /// The bindings in the window ([`bindings_in`]'s answer).
    pub bindings: Vec<Bound>,
    /// Every `def` in the window whose label could be drawn.
    pub defs: Vec<DefSite>,
    /// The text's [`Shapes`], where the caller asked: the type side reads them next, and a caller
    /// that holds them already for this text asks for none.
    pub shapes: Option<Shapes>,
}

/// The margin's reading of `source` for the window `within`: [`bindings_in`], every `def`'s label
/// place, and [`shapes`] where `with_shapes`, from one parse and one [`Finder`] walk. Each used to
/// parse the text on its own, four times for one cold request.
#[must_use]
pub fn margin(source: &str, within: (u32, u32), with_shapes: bool) -> Margin {
    let result = parse(source);
    // No cursor in this file: the one walk every part below reads.
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    let bindings = finder.bindings(within);
    let mut defs = DefSites {
        within,
        found: Vec::new(),
    };
    defs.visit(&result.node());
    let shapes = with_shapes.then(|| shapes_of(source, &result.node(), &finder));
    Margin {
        bindings,
        defs: defs.found,
        shapes,
    }
}

/// [`margin`]'s `def`s: every one whose span from name to label overlaps the window.
struct DefSites {
    within: (u32, u32),
    found: Vec<DefSite>,
}

impl<'pr> Visit<'pr> for DefSites {
    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        let name = node.name_loc();
        let name = (name.start_offset() as u32, name.end_offset() as u32);
        let at = node
            .rparen_loc()
            .map(|paren| paren.end_offset() as u32)
            .or_else(|| {
                node.parameters()
                    .map(|parameters| parameters.location().end_offset() as u32)
            })
            .unwrap_or(name.1);
        // From the name to where the label goes: the span this `def` occupies as far as a hint is
        // concerned. Testing the name alone would miss a `def` whose label is in the window and
        // whose name is one line above it.
        if overlaps((name.0, at), self.within) {
            self.found.push(DefSite {
                name,
                at,
                raises: raises_in(node),
            });
        }
        ruby_prism::visit_def_node(self, node);
    }
}

/// [`Shapes`] from a tree and a cursor-less [`Finder`] walk of it that a caller already has, so a
/// request reading one text for several things parses and walks it once ([`margin`]).
fn shapes_of(source: &str, node: &Node<'_>, finder: &Finder<'_, '_>) -> Shapes {
    // The variable table asks [`scopes`] about this text next; hand it the tree.
    scopes::seed(source, node);
    let mut exits = Exits::new(Vec::new(), None);
    exits.visit(node);
    let returns = exits
        .found
        .iter()
        .map(|(span, nodes)| {
            let name = exits.names.get(span).map_or("", String::as_str);
            (
                *span,
                nodes
                    .iter()
                    .map(|(exit, given)| {
                        let value = match exit {
                            Exit::Written(node) => {
                                finder.receiver_of(Some(node), Budget::default())
                            }
                            // The class of a `nil` the parser never saw, named as `literal_class`
                            // names one it did: a written `nil` and an unwritten branch are the
                            // same value.
                            Exit::Nil => Receiver::literal("NilClass"),
                            Exit::Unknown => Receiver::Unknown,
                        };
                        guarded(*given, span.0, name, value)
                    })
                    .collect(),
            )
        })
        .collect();
    let mut gathered = Gathered::new(node);
    gathered.visit(node);
    let Gathered {
        defaults,
        raising,
        sites,
        yields: yield_sites,
        writers,
        procs: literals,
        uses,
        mut ends,
    } = gathered;
    ends.ends.sort_unstable();
    let written_defaults = defaults
        .found
        .iter()
        .map(|(span, held)| {
            let held = held
                .iter()
                .map(|(slot, value)| (slot.clone(), span_of(value)))
                .collect();
            (*span, held)
        })
        .collect();
    let defaults = defaults
        .found
        .into_iter()
        .map(|(span, held)| {
            let held = held
                .into_iter()
                .map(|(slot, value)| (slot, finder.receiver_of(Some(&value), Budget::default())))
                .collect();
            (span, held)
        })
        .collect();
    let blocks = sites
        .found
        .into_iter()
        .map(|found| BlockSite {
            on: found
                .receiver
                .map_or(Receiver::SelfObject(found.call), |receiver| {
                    finder.receiver_of(Some(&receiver), Budget::default())
                }),
            span: found.span,
            method: found.method,
            slot: found.slot,
            call: found.call,
            body: found.body,
        })
        .collect();
    let yields = yield_sites
        .found
        .into_iter()
        .map(|(span, sites)| {
            let rewritten = finder.block_rewritten(span);
            let handed = sites
                .into_iter()
                // A `block.call` on a `&block` the `def` writes to is no `yield`.
                .filter(|site| !(site.called && rewritten))
                .map(|site| {
                    site.handed.map(|handed| {
                        handed
                            .iter()
                            .map(|value| finder.receiver_of(Some(value), Budget::default()))
                            .collect()
                    })
                })
                .collect::<Option<Vec<Vec<Receiver>>>>();
            (span, handed)
        })
        .collect();
    let setters = writers
        .found
        .into_iter()
        .map(|(at, name, on, value, set)| Setter {
            at,
            name,
            on: finder.receiver_of(Some(&on), Budget::default()),
            value: finder.receiver_of(Some(&value), Budget::default()),
            set,
        })
        .collect();
    let sends = writers
        .sends
        .into_iter()
        .map(|(at, (name, values), on)| Sent {
            at,
            name,
            values,
            on: on.map_or(Receiver::SelfObject(at), |on| {
                finder.receiver_of(Some(&on), Budget::default())
            }),
        })
        .collect();
    let procs = literals
        .found
        .iter()
        .map(|literal| {
            (
                literal.location().start_offset() as u32,
                finder.proc_shape(literal),
            )
        })
        .collect();
    Shapes {
        returns,
        variables: finder.variables(&ends, &uses.0, &uses.1),
        defaults,
        written_defaults,
        raising: raising.0,
        blocks,
        bodies: sites.bodies,
        yields,
        procs,
        asks: exits.asks,
        setters,
        sends,
    }
}

/// Every writer called on a receiver, for [`Shapes::setters`]: `(where the call starts, the member
/// without its `=`, the receiver, the value)`. And every call that can send one by name, for
/// [`Shapes::sends`]: `(where the call starts, the names, the receiver)`.
#[derive(Default)]
struct Writers<'pr> {
    found: Vec<Written<'pr>>,
    sends: Vec<(u32, Sending, Option<Node<'pr>>)>,
    /// Each `def` being walked, innermost last: its body, and the locals it writes
    /// ([`LocalNames`]), read the first time a sending call in it names one by a local.
    locals: Vec<(Option<Node<'pr>>, Option<LocalSpellings>)>,
}

/// What each local one `def` writes can spell ([`LocalNames`]).
type LocalSpellings = HashMap<Vec<u8>, scopes::Spelled>;

/// What [`Writers::sent`] reads of one sending call: the names, and how many values follow.
type Sending = (scopes::Spelled, Option<usize>);

/// One writer's call for [`Shapes::setters`]: where it starts, the member without its `=`, the
/// receiver, the value, and whether `set(name: value)` wrote it.
type Written<'pr> = (u32, String, Node<'pr>, Node<'pr>, bool);

/// The methods that call whatever method the name they are handed names ([`Shapes::sends`]).
pub const SENDERS: [&str; 5] = ["send", "public_send", "__send__", "try", "try!"];

impl<'pr> Writers<'pr> {
    /// Keep a writer's call. `name` is the writer's, with its `=`.
    fn push(&mut self, at: u32, name: &[u8], on: Option<Node<'pr>>, value: Node<'pr>) {
        let Some(on) = on else {
            return;
        };
        let name = String::from_utf8_lossy(name);
        let name = name.strip_suffix('=').unwrap_or(&name).to_owned();
        self.found.push((at, name, on, value, false));
    }

    /// Keep each keyword of a `set(name: value)` call on a receiver, as a write of `name`
    ///; a double splat is a write of any name, `**`.
    fn set(&mut self, call: &CallNode<'pr>) {
        if call.receiver().is_none() {
            return;
        }
        let at = call.location().start_offset() as u32;
        let elements: Vec<Node<'pr>> = call
            .arguments()
            .into_iter()
            .flat_map(|written| written.arguments().iter())
            .flat_map(|argument| {
                if let Some(hash) = argument.as_keyword_hash_node() {
                    hash.elements().iter().collect()
                } else if let Some(hash) = argument.as_hash_node() {
                    hash.elements().iter().collect()
                } else {
                    Vec::new()
                }
            })
            .collect();
        let receivers = std::iter::repeat_with(|| call.receiver()).flatten();
        for (element, on) in elements.into_iter().zip(receivers) {
            let written = element.as_assoc_node().and_then(|pair| {
                let key = pair.key();
                let name = key.as_symbol_node()?.unescaped().to_vec();
                Some((String::from_utf8_lossy(&name).into_owned(), pair.value()))
            });
            let (name, value) = written.unwrap_or_else(|| ("**".to_owned(), element));
            self.found.push((at, name, on, value, true));
        }
    }

    /// The names a sending call can send, and how many arguments it passes after the name (`None`
    /// where a splat or `...` can be any number, and one first can be any name). `None` for a call
    /// with no argument, which raises.
    fn sent(&mut self, call: &CallNode<'_>) -> Option<Sending> {
        let arguments: Vec<Node<'_>> = call
            .arguments()
            .map(|written| written.arguments().iter().collect())
            .unwrap_or_default();
        let spreads = |argument: &Node<'_>| {
            argument.as_splat_node().is_some() || argument.as_forwarding_arguments_node().is_some()
        };
        let (name, values) = arguments.split_first()?;
        if spreads(name) {
            return Some((scopes::Spelled::everything(), None));
        }
        let values = (!values.iter().any(spreads)).then_some(values.len());
        let spelled = match name.as_local_variable_read_node() {
            Some(local) => self
                .locals_here()
                .and_then(|written| written.get(local.name().as_slice()))
                .cloned()
                .unwrap_or_else(scopes::Spelled::everything),
            None => scopes::spelled_by(name),
        };
        Some((spelled, values))
    }

    /// The locals the innermost `def` being walked writes, read on first use: most `def`s send
    /// nothing by a local, and reading every one's was a walk of most of the text.
    fn locals_here(&mut self) -> Option<&LocalSpellings> {
        let (body, held) = self.locals.last_mut()?;
        Some(held.get_or_insert_with(|| {
            let mut locals = LocalNames::default();
            if let Some(body) = body {
                locals.visit(body);
            }
            locals.found
        }))
    }

    /// A `def` opens: its locals are its own.
    fn enter_def(&mut self, node: &DefNode<'pr>) {
        self.locals.push((node.body(), None));
    }

    /// The innermost `def` closes.
    fn leave_def(&mut self) {
        self.locals.pop();
    }

    /// A call: a sending one, a `set(name: value)` on a receiver, or a writer's.
    fn call(&mut self, node: &CallNode<'pr>) {
        if SENDERS
            .iter()
            .any(|sender| sender.as_bytes() == node.name().as_slice())
            && let Some(name) = self.sent(node)
        {
            self.sends
                .push((node.location().start_offset() as u32, name, node.receiver()));
        }
        if node.name().as_slice() == b"set" {
            self.set(node);
        }
        // `x.name = v` is a call of `name=` with one argument; Prism marks it an attribute write.
        // `x[k] = v` is one too, with two, and writes no accessor.
        let mut written = node
            .arguments()
            .filter(|_| node.is_attribute_write())
            .into_iter()
            .flat_map(|arguments| arguments.arguments().iter());
        if let (Some(value), None) = (written.next(), written.next()) {
            self.push(
                node.location().start_offset() as u32,
                node.name().as_slice(),
                node.receiver(),
                value,
            );
        }
    }
}

/// How one read of a local is used, for [`Finder::fill`].
pub(super) enum Use<'pr> {
    /// The receiver of a statement that adds to it: the values added, and whether they are a key
    /// and a value (`[]=`, `store`) rather than elements.
    Adds(Vec<Node<'pr>>, bool),
    /// The receiver of a member that hands back something else and changes nothing (`size`, `map`,
    /// `join`), of an `each` whose value is dropped, the value of a `return`, or a `def`'s last
    /// statement: the method's value is what it built.
    Reads,
    /// Anything else: an argument, a value of another variable or literal, `yield`, any other
    /// member. It may be changed where this cannot see.
    Escapes,
}

/// The members of an `Array` or a `Hash` that hand back something other than the receiver and
/// change nothing.
const READS: [&str; 44] = [
    "size",
    "length",
    "count",
    "empty?",
    "any?",
    "none?",
    "all?",
    "one?",
    "include?",
    "member?",
    "first",
    "last",
    "join",
    "map",
    "flat_map",
    "collect",
    "select",
    "filter",
    "reject",
    "find",
    "detect",
    "sum",
    "min",
    "max",
    "min_by",
    "max_by",
    "sort",
    "sort_by",
    "uniq",
    "compact",
    "group_by",
    "partition",
    "each_with_object",
    "index",
    "dig",
    "[]",
    "take",
    "drop",
    "reverse",
    "keys",
    "values",
    "key?",
    "fetch",
    "values_at",
];

/// The members that hand back the receiver: read-only as a statement, an alias of it anywhere
/// else.
const SELF_RETURNING: [&str; 6] = [
    "each",
    "each_with_index",
    "each_pair",
    "each_key",
    "each_value",
    "reverse_each",
];

/// The class of an empty container literal a local may start as, or `None`.
fn empty_container(node: &Node<'_>) -> Option<&'static str> {
    if node
        .as_array_node()
        .is_some_and(|array| array.elements().iter().next().is_none())
    {
        return Some("Array");
    }
    if node
        .as_hash_node()
        .is_some_and(|hash| hash.elements().iter().next().is_none())
    {
        return Some("Hash");
    }
    let call = node.as_call_node()?;
    let class = call.receiver()?.as_constant_read_node()?;
    let empty =
        call.name().as_slice() == b"new" && call.arguments().is_none() && call.block().is_none();
    match class.name().as_slice() {
        b"Array" if empty => Some("Array"),
        b"Hash" if empty => Some("Hash"),
        _ => None,
    }
}

/// Every local read's [`Use`], by where it starts: the first use the walk meets is the read's.
/// Beside them, every local write whose value nothing takes, by where it starts: a statement of a
/// list that is not its last ([`Reaching::kept`]).
#[derive(Default)]
struct Uses<'pr>(HashMap<u32, Use<'pr>>, HashSet<u32>);

impl<'pr> Uses<'pr> {
    fn local(node: Option<Node<'pr>>) -> Option<u32> {
        let node = node?;
        let node = unparenthesised(&node).unwrap_or(node);
        Some(
            node.as_local_variable_read_node()?
                .location()
                .start_offset() as u32,
        )
    }

    /// What a statement-level call on a local adds to it ([`Use::Adds`]).
    fn added(call: &CallNode<'pr>) -> Option<(Vec<Node<'pr>>, bool)> {
        let arguments: Vec<Node<'pr>> = call
            .arguments()
            .map(|written| written.arguments().iter().collect())
            .unwrap_or_default();
        let added = match call.name().as_slice() {
            b"<<" | b"push" | b"append" | b"unshift" | b"prepend" => (arguments, false),
            b"insert" => (arguments.into_iter().skip(1).collect(), false),
            b"[]=" | b"store" if arguments.len() == 2 => (arguments, true),
            _ => return None,
        };
        (call.block().is_none()).then_some(added)
    }

    fn note(&mut self, at: u32, used: Use<'pr>) {
        self.0.entry(at).or_insert(used);
    }

    /// A statement list, before its statements: each call on a local written as a statement, and
    /// each local write whose value the list drops.
    fn statements(&mut self, node: &StatementsNode<'pr>) {
        let body = node.body();
        for statement in body.iter().take(body.len().saturating_sub(1)) {
            if let Some(write) = statement.as_local_variable_write_node() {
                self.1.insert(write.location().start_offset() as u32);
            }
        }
        for statement in node.body().iter() {
            let Some(call) = statement.as_call_node() else {
                continue;
            };
            let Some(at) = Self::local(call.receiver()) else {
                continue;
            };
            if let Some((values, keyed)) = Self::added(&call) {
                self.note(at, Use::Adds(values, keyed));
            } else if SELF_RETURNING
                .iter()
                .any(|name| name.as_bytes() == call.name().as_slice())
            {
                self.note(at, Use::Reads);
            }
        }
    }

    /// A call, before its receiver: a member called on a local.
    fn call(&mut self, node: &CallNode<'pr>) {
        if let Some(at) = Self::local(node.receiver()) {
            let reads = READS
                .iter()
                .any(|name| name.as_bytes() == node.name().as_slice());
            self.note(at, if reads { Use::Reads } else { Use::Escapes });
        }
    }

    /// A `return` of one local.
    fn returned(&mut self, node: &ruby_prism::ReturnNode<'pr>) {
        let mut arguments = node
            .arguments()
            .into_iter()
            .flat_map(|written| written.arguments().iter());
        if let (Some(only), None) = (arguments.next(), arguments.next())
            && let Some(at) = Self::local(Some(only))
        {
            self.note(at, Use::Reads);
        }
    }

    /// A `def`, before its body: a local its last statement reads.
    fn def(&mut self, node: &DefNode<'pr>) {
        let last = node
            .body()
            .and_then(|body| body.as_statements_node())
            .and_then(|statements| statements.body().iter().last());
        if let Some(at) = Self::local(last) {
            self.note(at, Use::Reads);
        }
    }

    /// Any other read of a local.
    fn read(&mut self, node: &LocalVariableReadNode<'pr>) {
        self.note(node.location().start_offset() as u32, Use::Escapes);
    }
}

/// One text's [`CallEnds`], from the walk [`shapes_of`] makes ([`Gathered`]).
#[cfg(test)]
fn call_ends(node: &Node<'_>) -> CallEnds {
    let mut gathered = Gathered::new(node);
    gathered.visit(node);
    gathered.ends.ends.sort_unstable();
    gathered.ends
}

/// Where every construct in a text that may run a method ends, sorted: the moment it runs is at
/// its end, after its receiver and arguments. A call, an operator write, `super`,
/// `yield`, an interpolation (`to_s`), a multiple assignment (`to_ary`), a splat, a block
/// argument (`to_proc`), a range (`<=>`), a `case` and a pattern (`===`, `deconstruct`), a `for`
/// (`each`), backticks, a `def`, `alias`, `undef` and a class body (their hooks), and a constant
/// write (`const_added`).
///
/// **A modifier's condition runs first**: in `@user.save if valid?`, `valid?` runs before `@user`
/// is read though it is written after, and in `touch if @label.upcase` `touch` runs after the read
/// though it is written before. So [`CallEnds::modifiers`] holds every modifier `if`, `unless`,
/// `while` and `until`'s body and condition, for [`CallEnds::between`] to put in order.
#[derive(Debug, Default)]
struct CallEnds {
    /// Where every construct that may run a method ends, sorted once the walk is done.
    ends: Vec<u32>,
    /// Every modifier's body and condition, the body written first and run last.
    modifiers: Vec<((u32, u32), (u32, u32))>,
}

impl CallEnds {
    /// A construct that may run a method, ending where `location` does.
    fn end(&mut self, location: &Location<'_>) {
        self.ends.push(location.end_offset() as u32);
    }

    /// A conditional or loop, kept where its `statements` are written before its `predicate`: a
    /// modifier.
    fn modifier<'pr>(&mut self, predicate: &Node<'pr>, statements: Option<StatementsNode<'pr>>) {
        if let Some(statements) = statements.filter(|statements| {
            statements.location().start_offset() < predicate.location().start_offset()
        }) {
            self.modifiers
                .push((span_of(&statements.as_node()), span_of(predicate)));
        }
    }

    /// Whether a construct that may run a method runs after the write that takes effect at
    /// `write` (its name at `name`) and before the read at `read`.
    ///
    /// By where each ends, except around a modifier: its condition's constructs run before a read
    /// in its body (and before a write there), and its body's run after a read in its condition.
    fn between(&self, name: u32, write: u32, read: u32) -> bool {
        let inside = |span: (u32, u32), at: u32| span.0 <= at && at < span.1;
        let lexical = self.ends[self.ends.partition_point(|end| *end <= write)..]
            .iter()
            .take_while(|end| **end <= read)
            .any(|end| {
                !self
                    .modifiers
                    .iter()
                    .any(|(body, condition)| inside(*condition, read) && inside(*body, *end - 1))
            });
        lexical
            || self.modifiers.iter().any(|(body, condition)| {
                inside(*body, read)
                    && !inside(*body, name)
                    && self
                        .ends
                        .iter()
                        .any(|end| condition.0 < *end && *end <= condition.1)
            })
    }
}

/// Whether a statement may `return` from the method it is written in: a `return` anywhere in it,
/// a block's and a lambda's included (a lambda's leaves only the lambda, which this does not tell
/// apart), a nested `def`'s not.
fn may_return(statement: &Node<'_>) -> bool {
    #[derive(Default)]
    struct Returns(bool);
    impl<'pr> Visit<'pr> for Returns {
        fn visit_return_node(&mut self, _: &ruby_prism::ReturnNode<'pr>) {
            self.0 = true;
        }

        fn visit_def_node(&mut self, _: &DefNode<'pr>) {}
    }
    let mut found = Returns::default();
    found.visit(statement);
    found.0
}

/// The locals one `def`'s body writes, for [`Writers::sent`]: the names each can hold where every
/// write of it is a literal or an interpolation ([`scopes::spelled_by`]), and any name where one
/// is anything else. Two spellings that both name writers are any writer. A nested `def` has its
/// own locals.
#[derive(Default)]
struct LocalNames {
    found: HashMap<Vec<u8>, scopes::Spelled>,
}

impl LocalNames {
    fn add(&mut self, name: &[u8], spelled: scopes::Spelled) {
        let joined = match self.found.remove(name) {
            None => spelled,
            Some(held) if held == spelled => held,
            Some(held) if held.names_a_writer() && spelled.names_a_writer() => {
                scopes::Spelled::writer()
            }
            Some(_) => scopes::Spelled::everything(),
        };
        self.found.insert(name.to_vec(), joined);
    }
}

impl<'pr> Visit<'pr> for LocalNames {
    fn visit_def_node(&mut self, _: &DefNode<'pr>) {}

    fn visit_local_variable_write_node(&mut self, node: &LocalVariableWriteNode<'pr>) {
        let value = node.value();
        let spelled = if value.as_symbol_node().is_some()
            || value.as_string_node().is_some()
            || value.as_interpolated_symbol_node().is_some()
            || value.as_interpolated_string_node().is_some()
        {
            scopes::spelled_by(&value)
        } else {
            scopes::Spelled::everything()
        };
        self.add(node.name().as_slice(), spelled);
        ruby_prism::visit_local_variable_write_node(self, node);
    }

    fn visit_local_variable_target_node(&mut self, node: &LocalVariableTargetNode<'pr>) {
        self.add(node.name().as_slice(), scopes::Spelled::everything());
    }

    fn visit_local_variable_or_write_node(&mut self, node: &LocalVariableOrWriteNode<'pr>) {
        self.add(node.name().as_slice(), scopes::Spelled::everything());
        ruby_prism::visit_local_variable_or_write_node(self, node);
    }

    fn visit_local_variable_and_write_node(&mut self, node: &LocalVariableAndWriteNode<'pr>) {
        self.add(node.name().as_slice(), scopes::Spelled::everything());
        ruby_prism::visit_local_variable_and_write_node(self, node);
    }

    fn visit_local_variable_operator_write_node(
        &mut self,
        node: &LocalVariableOperatorWriteNode<'pr>,
    ) {
        self.add(node.name().as_slice(), scopes::Spelled::everything());
        ruby_prism::visit_local_variable_operator_write_node(self, node);
    }
}

/// Every proc and lambda literal in a text ([`proc_literal`]), for [`Shapes::procs`].
#[derive(Default)]
struct ProcLiterals<'pr> {
    found: Vec<Node<'pr>>,
}

impl<'pr> ProcLiterals<'pr> {
    fn lambda(&mut self, node: &LambdaNode<'pr>) {
        self.found.push(node.as_node());
    }

    /// `proc {}`, `lambda {}` and `Proc.new {}`.
    fn call(&mut self, node: &CallNode<'pr>) {
        if proc_literal(&node.as_node()).is_some() {
            self.found.push(node.as_node());
        }
    }
}

/// Every `return` of a lambda's own body, blocks inside it included (a `return` there leaves the
/// lambda), but not a nested lambda or `def`.
#[derive(Default)]
struct Returning<'pr> {
    found: Vec<Option<Node<'pr>>>,
    unreadable: bool,
}

impl<'pr> Visit<'pr> for Returning<'pr> {
    fn visit_return_node(&mut self, node: &ruby_prism::ReturnNode<'pr>) {
        match node.arguments() {
            None => self.found.push(None),
            Some(written) => match exactly_one(&written) {
                Some(one) => self.found.push(Some(one)),
                None => self.unreadable = true,
            },
        }
    }

    fn visit_lambda_node(&mut self, _: &LambdaNode<'pr>) {}

    fn visit_def_node(&mut self, _: &DefNode<'pr>) {}
}

/// Every `def`'s `yield`s, and calls of its own `&block`, with what each hands over.
///
/// - **Every one in the `def`'s body**, inside its blocks and lambdas too: those still hand
///   values to the method's block. A `def` inside it has its own.
/// - **A splat, keywords, a block argument or `...` leave no position certain**, so that site is
///   `None`, and so is the whole `def`'s entry ([`Shapes::yields`]).
/// - **Every `def` gets an entry**, an empty one where it never yields.
#[derive(Default)]
struct YieldSites<'pr> {
    /// The `def`s open around the walk: their span and the name of their `&block`, if any.
    open: Vec<OpenDef>,
    found: HashMap<(u32, u32), Vec<YieldSite<'pr>>>,
}

/// A `def` [`YieldSites`] is inside: its span, and its `&block`'s name.
type OpenDef = ((u32, u32), Option<Vec<u8>>);

/// One `yield` (or `block.call`) and what it hands over; `None` where that cannot be read.
struct YieldSite<'pr> {
    handed: Option<Vec<Node<'pr>>>,
    /// Written as a call of the `&block`, which only counts while the `def` never writes to it.
    called: bool,
}

impl<'pr> YieldSites<'pr> {
    /// A site in the innermost open `def` whose values cannot be read.
    fn unreadable(&mut self, called: bool) {
        if let Some((span, _)) = self.open.last() {
            self.found.entry(*span).or_default().push(YieldSite {
                handed: None,
                called,
            });
        }
    }

    fn note(&mut self, arguments: Option<ruby_prism::ArgumentsNode<'pr>>, called: bool) {
        let Some((span, _)) = self.open.last() else {
            return;
        };
        let handed = match arguments {
            None => Some(Vec::new()),
            Some(written) => written
                .arguments()
                .iter()
                .map(|argument| {
                    // A `&b` is never among the arguments: Prism holds it as the call's block.
                    let plain = argument.as_splat_node().is_none()
                        && argument.as_keyword_hash_node().is_none()
                        && argument.as_forwarding_arguments_node().is_none();
                    plain.then_some(argument)
                })
                .collect(),
        };
        self.found
            .entry(*span)
            .or_default()
            .push(YieldSite { handed, called });
    }

    /// A `def` opens, with its `&block`'s name.
    fn enter_def(&mut self, node: &DefNode<'pr>) {
        let span = span_of(&node.as_node());
        let block = node
            .parameters()
            .and_then(|written| written.block())
            .and_then(|block| block.name())
            .map(|name| name.as_slice().to_vec());
        self.found.entry(span).or_default();
        self.open.push((span, block));
    }

    /// The innermost `def` closes.
    fn leave_def(&mut self) {
        self.open.pop();
    }

    fn yielded(&mut self, node: &ruby_prism::YieldNode<'pr>) {
        self.note(node.arguments(), false);
    }

    /// A call of the innermost `def`'s own `&block`.
    fn call(&mut self, node: &CallNode<'pr>) {
        let named = self.open.last().and_then(|(_, block)| block.as_deref());
        if let Some(block) = named
            && matches!(node.name().as_slice(), b"call" | b"yield" | b"[]")
            && node
                .receiver()
                .and_then(|receiver| receiver.as_local_variable_read_node())
                .is_some_and(|read| read.name().as_slice() == block)
        {
            // A block passed to the block's own call leaves no position certain.
            if node.block().is_some() {
                self.unreadable(true);
            } else {
                self.note(node.arguments(), true);
            }
        }
    }
}

/// [`Shapes::blocks`]' walk: every block and lambda argument, with its call, before the call's
/// receiver is shaped ([`shapes_of`]).
struct Sites<'pr> {
    /// Every body seen, the file's included.
    bodies: Vec<(u32, u32)>,

    /// The bodies around the walk, innermost last.
    open: Vec<(u32, u32)>,
    found: Vec<Site<'pr>>,
}

/// One [`BlockSite`] before its receiver is a shape.
struct Site<'pr> {
    span: (u32, u32),
    receiver: Option<Node<'pr>>,
    method: String,
    slot: BlockSlot,
    call: u32,
    body: (u32, u32),
}

impl<'pr> Sites<'pr> {
    /// A body opens: a `def`, a class, a module, `class << self`, or a block that makes a class.
    fn open(&mut self, span: (u32, u32)) {
        self.bodies.push(span);
        self.open.push(span);
    }

    /// The innermost body closes.
    fn close(&mut self) {
        self.open.pop();
    }

    /// The block a lambda or a `proc { … }` / `lambda { … }` argument runs: its span, or `None` for
    /// any other argument.
    fn lambda_span(argument: &Node<'pr>) -> Option<(u32, u32)> {
        if let Some(lambda) = argument.as_lambda_node() {
            return Some(span_of(&lambda.as_node()));
        }
        let call = argument.as_call_node()?;
        if call.receiver().is_some() || !matches!(call.name().as_slice(), b"proc" | b"lambda") {
            return None;
        }
        Some(span_of(&call.block()?.as_block_node()?.as_node()))
    }

    /// A call's blocks and lambda arguments, and the block it runs as a class's body, if any: the
    /// walk opens that body around the block alone ([`Gathered::visit_call_node`]).
    fn call(&mut self, node: &CallNode<'pr>) -> Option<BlockNode<'pr>> {
        // Each site's span and slot; the rest is the call's, spelled only where it has one.
        let mut found: Vec<((u32, u32), BlockSlot)> = Vec::new();
        // `proc { }` and `lambda { }` make a block into a value; they never run it against
        // anything, and where one is an argument it is filed as that argument, below.
        let literal =
            node.receiver().is_none() && matches!(node.name().as_slice(), b"proc" | b"lambda");
        if let Some(block) = node.block().and_then(|block| block.as_block_node())
            && !literal
        {
            found.push((span_of(&block.as_node()), BlockSlot::Block));
        }
        let mut position = Some(0usize);
        for argument in node
            .arguments()
            .iter()
            .flat_map(|written| written.arguments().iter())
        {
            if let Some(keywords) = argument.as_keyword_hash_node() {
                for element in keywords.elements().iter() {
                    let Some(pair) = element.as_assoc_node() else {
                        continue;
                    };
                    let (Some(key), Some(span)) = (
                        pair.key().as_symbol_node(),
                        Self::lambda_span(&pair.value()),
                    ) else {
                        continue;
                    };
                    let name = String::from_utf8_lossy(key.unescaped()).into_owned();
                    found.push((span, BlockSlot::Keyword(name.into())));
                }
                continue;
            }
            // Past a splat no position is known.
            if argument.as_splat_node().is_some() {
                position = None;
                continue;
            }
            if let (Some(index), Some(span)) = (position, Self::lambda_span(&argument)) {
                found.push((span, BlockSlot::Positional(index)));
            }
            position = position.map(|index| index + 1);
        }
        if !found.is_empty() {
            let body = *self.open.last().expect("the file is always open");
            let call = node.location().start_offset() as u32;
            let method = String::from_utf8_lossy(node.name().as_slice()).into_owned();
            self.found
                .extend(found.into_iter().map(|(span, slot)| Site {
                    span,
                    receiver: node.receiver(),
                    method: method.clone(),
                    slot,
                    call,
                    body,
                }));
        }
        // **A block that makes a class is a body**, as rubydex records it (an anonymous class): its
        // `self` is that class, so no block around the call decides it, however that one rebinds.
        node.block()
            .and_then(|block| block.as_block_node())
            .filter(|_| makes_a_class(node))
    }
}

/// Whether a call makes a class and evaluates its block in it: `Class.new`, `Module.new`,
/// `Struct.new` and `Data.define`, on the constant itself.
fn makes_a_class(node: &CallNode<'_>) -> bool {
    let Some(receiver) = node.receiver() else {
        return false;
    };
    let named = if let Some(read) = receiver.as_constant_read_node() {
        read.name().as_slice().to_vec()
    } else if let Some(path) = receiver.as_constant_path_node()
        && path.parent().is_none()
    {
        path.name()
            .map(|name| name.as_slice().to_vec())
            .unwrap_or_default()
    } else {
        return false;
    };
    matches!(
        (named.as_slice(), node.name().as_slice()),
        (b"Class" | b"Module" | b"Struct", b"new") | (b"Data", b"define")
    )
}

/// Whether a `def`'s own code writes `raise` or `fail`: its body and its parameters' defaults,
/// blocks, lambdas and `rescue` clauses included, and a nested `def` left out, since that is
/// another method.
///
/// **A warning, not a proof** (decided 2026-09-25). A `raise` in a block that never runs, or one a
/// `rescue` in the same method catches, still counts: the reader is told the method has one.
/// Written syntax only, like [`never_returns`]: a class defining its own `raise` is not told apart.
#[must_use]
pub fn raises_in(node: &DefNode<'_>) -> bool {
    let mut walk = Raises(false);
    if let Some(parameters) = node.parameters() {
        walk.visit_parameters_node(&parameters);
    }
    if let Some(body) = node.body() {
        walk.visit(&body);
    }
    walk.0
}

/// Whether a call is Ruby's `raise` or `fail`: written with no receiver, or on `self`.
fn is_raise(call: &CallNode<'_>) -> bool {
    matches!(call.name().as_slice(), b"raise" | b"fail")
        && call
            .receiver()
            .is_none_or(|receiver| receiver.as_self_node().is_some())
        && !call.is_safe_navigation()
}

/// Whether a shape is a call to Ruby's `raise` or `fail`, which never returns.
///
/// For a `||` operand, which hands back no value ([`types`](super::types)' shortcut). An exit that
/// raises is never filed at all ([`Exits::push`]).
#[must_use]
pub fn never_returns(receiver: &Receiver) -> bool {
    // A bare `raise` is a bare name, which the name rung may read: [`Receiver::Spelled`].
    let receiver = match receiver {
        Receiver::Spelled { was, .. } => was,
        other => other,
    };
    matches!(
        receiver,
        Receiver::Returned { on, method, safe: false, .. }
            if matches!(**on, Receiver::SelfObject(_)) && matches!(method.as_str(), "raise" | "fail")
    )
}

/// [`raises_in`]'s walk. It stops looking once one is found, and never enters another `def`.
struct Raises(bool);

impl<'pr> Visit<'pr> for Raises {
    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        if self.0 || is_raise(node) {
            self.0 = true;
            return;
        }
        ruby_prism::visit_call_node(self, node);
    }

    fn visit_def_node(&mut self, _: &DefNode<'pr>) {}
}

/// Every `def` in a text that [`raises_in`] says writes one, for [`Shapes::raising`].
#[derive(Default)]
struct Raising(HashSet<(u32, u32)>);

impl Raising {
    fn def(&mut self, node: &DefNode<'_>) {
        if raises_in(node) {
            self.0.insert((
                node.location().start_offset() as u32,
                node.location().end_offset() as u32,
            ));
        }
    }
}

/// Every `def`'s optional parameters' default expressions, by the `def`'s span ([`Shapes::defaults`]).
///
/// Slots as [`bound_by`] counts them: required and optional positionals in one sequence, and
/// keywords by name.
#[derive(Default)]
struct Defaults<'pr> {
    found: HashMap<(u32, u32), Vec<(ParameterSlot, Node<'pr>)>>,
}

impl<'pr> Defaults<'pr> {
    fn def(&mut self, node: &DefNode<'pr>) {
        if let Some(written) = node.parameters() {
            let mut held = Vec::new();
            for (index, parameter) in written
                .requireds()
                .iter()
                .chain(written.optionals().iter())
                .enumerate()
            {
                if let Some(optional) = parameter.as_optional_parameter_node() {
                    held.push((ParameterSlot::Positional(index), optional.value()));
                }
            }
            for parameter in written.keywords().iter() {
                if let Some(optional) = parameter.as_optional_keyword_parameter_node() {
                    let name = String::from_utf8_lossy(optional.name().as_slice()).into_owned();
                    held.push((ParameterSlot::Keyword(name), optional.value()));
                }
            }
            if !held.is_empty() {
                let here = (
                    node.location().start_offset() as u32,
                    node.location().end_offset() as u32,
                );
                self.found.insert(here, held);
            }
        }
    }
}

/// Every part of [`Shapes`] a plain walk of the whole tree reads, **from one walk**.
///
/// - **Why:** each part used to walk the tree on its own, eight walks a text. A walk costs about
///   11 ns a node however little it reads, more than most parts' own work.
/// - **Each part keeps its own state and hears what the walk meets in the order its own walk met
///   it**: before a node's children where it acted on the way down, after them where it closed
///   something (a `def`, a body). So nothing any part finds changes, and their order among
///   themselves at one node does not matter.
/// - **The walk is Prism's own order everywhere**, but around a block that makes a class
///   ([`Sites::call`]), whose body is opened around the block alone.
struct Gathered<'pr> {
    defaults: Defaults<'pr>,
    raising: Raising,
    sites: Sites<'pr>,
    yields: YieldSites<'pr>,
    writers: Writers<'pr>,
    procs: ProcLiterals<'pr>,
    uses: Uses<'pr>,
    ends: CallEnds,
}

impl<'pr> Gathered<'pr> {
    /// Ready to walk `node`, a text's tree: its own body is open.
    fn new(node: &Node<'_>) -> Self {
        Self {
            defaults: Defaults::default(),
            raising: Raising::default(),
            sites: Sites {
                bodies: vec![span_of(node)],
                open: vec![span_of(node)],
                found: Vec::new(),
            },
            yields: YieldSites::default(),
            writers: Writers::default(),
            procs: ProcLiterals::default(),
            uses: Uses::default(),
            ends: CallEnds::default(),
        }
    }

    /// A body that is not a `def`: a class, a module, `class << self`.
    fn body(&mut self, node: &Node<'pr>, walk: impl FnOnce(&mut Self)) {
        self.ends.end(&node.location());
        self.sites.open(span_of(node));
        walk(self);
        self.sites.close();
    }
}

/// Where a construct may run a method and nothing else here reads it ([`CallEnds`]).
macro_rules! gathered_ends {
    ($($visit:ident, $node:ident;)*) => {
        impl<'pr> Visit<'pr> for Gathered<'pr> {
            $(
                fn $visit(&mut self, node: &ruby_prism::$node<'pr>) {
                    self.ends.end(&node.location());
                    ruby_prism::$visit(self, node);
                }
            )*

            fn visit_def_node(&mut self, node: &DefNode<'pr>) {
                self.ends.end(&node.location());
                self.defaults.def(node);
                self.raising.def(node);
                self.uses.def(node);
                self.yields.enter_def(node);
                self.writers.enter_def(node);
                self.sites.open(span_of(&node.as_node()));
                ruby_prism::visit_def_node(self, node);
                self.sites.close();
                self.writers.leave_def();
                self.yields.leave_def();
            }

            fn visit_class_node(&mut self, node: &ruby_prism::ClassNode<'pr>) {
                self.body(&node.as_node(), |walk| ruby_prism::visit_class_node(walk, node));
            }

            fn visit_module_node(&mut self, node: &ruby_prism::ModuleNode<'pr>) {
                self.body(&node.as_node(), |walk| ruby_prism::visit_module_node(walk, node));
            }

            fn visit_singleton_class_node(&mut self, node: &ruby_prism::SingletonClassNode<'pr>) {
                self.body(&node.as_node(), |walk| {
                    ruby_prism::visit_singleton_class_node(walk, node);
                });
            }

            fn visit_call_node(&mut self, node: &CallNode<'pr>) {
                self.ends.end(&node.location());
                self.yields.call(node);
                self.writers.call(node);
                self.procs.call(node);
                self.uses.call(node);
                let Some(block) = self.sites.call(node) else {
                    ruby_prism::visit_call_node(self, node);
                    return;
                };
                if let Some(receiver) = node.receiver() {
                    self.visit(&receiver);
                }
                if let Some(arguments) = node.arguments() {
                    self.visit_arguments_node(&arguments);
                }
                self.sites.open(span_of(&block.as_node()));
                self.visit_block_node(&block);
                self.sites.close();
            }

            fn visit_call_or_write_node(&mut self, node: &CallOrWriteNode<'pr>) {
                self.ends.end(&node.location());
                self.writers.push(
                    node.location().start_offset() as u32,
                    node.write_name().as_slice(),
                    node.receiver(),
                    node.value(),
                );
                ruby_prism::visit_call_or_write_node(self, node);
            }

            fn visit_call_and_write_node(&mut self, node: &CallAndWriteNode<'pr>) {
                self.ends.end(&node.location());
                self.writers.push(
                    node.location().start_offset() as u32,
                    node.write_name().as_slice(),
                    node.receiver(),
                    node.value(),
                );
                ruby_prism::visit_call_and_write_node(self, node);
            }

            // `Current.count += 1` writes what `+` answers, which is not the value written: nothing
            // here reads it, and the accessor it writes answers nothing.
            fn visit_call_operator_write_node(&mut self, node: &CallOperatorWriteNode<'pr>) {
                self.ends.end(&node.location());
                self.writers.push(
                    node.location().start_offset() as u32,
                    node.write_name().as_slice(),
                    node.receiver(),
                    node.as_node(),
                );
                ruby_prism::visit_call_operator_write_node(self, node);
            }

            fn visit_yield_node(&mut self, node: &ruby_prism::YieldNode<'pr>) {
                self.ends.end(&node.location());
                self.yields.yielded(node);
                ruby_prism::visit_yield_node(self, node);
            }

            fn visit_lambda_node(&mut self, node: &LambdaNode<'pr>) {
                self.procs.lambda(node);
                ruby_prism::visit_lambda_node(self, node);
            }

            fn visit_statements_node(&mut self, node: &StatementsNode<'pr>) {
                self.uses.statements(node);
                ruby_prism::visit_statements_node(self, node);
            }

            fn visit_return_node(&mut self, node: &ruby_prism::ReturnNode<'pr>) {
                self.uses.returned(node);
                ruby_prism::visit_return_node(self, node);
            }

            fn visit_local_variable_read_node(&mut self, node: &LocalVariableReadNode<'pr>) {
                self.uses.read(node);
            }

            fn visit_if_node(&mut self, node: &ruby_prism::IfNode<'pr>) {
                self.ends.modifier(&node.predicate(), node.statements());
                ruby_prism::visit_if_node(self, node);
            }

            fn visit_unless_node(&mut self, node: &ruby_prism::UnlessNode<'pr>) {
                self.ends.modifier(&node.predicate(), node.statements());
                ruby_prism::visit_unless_node(self, node);
            }

            fn visit_while_node(&mut self, node: &ruby_prism::WhileNode<'pr>) {
                self.ends.modifier(&node.predicate(), node.statements());
                ruby_prism::visit_while_node(self, node);
            }

            fn visit_until_node(&mut self, node: &ruby_prism::UntilNode<'pr>) {
                self.ends.modifier(&node.predicate(), node.statements());
                ruby_prism::visit_until_node(self, node);
            }
        }
    };
}

gathered_ends! {
    visit_index_operator_write_node, IndexOperatorWriteNode;
    visit_index_and_write_node, IndexAndWriteNode;
    visit_index_or_write_node, IndexOrWriteNode;
    visit_instance_variable_operator_write_node, InstanceVariableOperatorWriteNode;
    visit_local_variable_operator_write_node, LocalVariableOperatorWriteNode;
    visit_class_variable_operator_write_node, ClassVariableOperatorWriteNode;
    visit_global_variable_operator_write_node, GlobalVariableOperatorWriteNode;
    visit_constant_operator_write_node, ConstantOperatorWriteNode;
    visit_constant_path_operator_write_node, ConstantPathOperatorWriteNode;
    visit_super_node, SuperNode;
    visit_forwarding_super_node, ForwardingSuperNode;
    visit_interpolated_string_node, InterpolatedStringNode;
    visit_interpolated_symbol_node, InterpolatedSymbolNode;
    visit_interpolated_regular_expression_node, InterpolatedRegularExpressionNode;
    visit_interpolated_x_string_node, InterpolatedXStringNode;
    visit_x_string_node, XStringNode;
    visit_match_write_node, MatchWriteNode;
    visit_match_predicate_node, MatchPredicateNode;
    visit_match_required_node, MatchRequiredNode;
    visit_case_node, CaseNode;
    visit_case_match_node, CaseMatchNode;
    visit_for_node, ForNode;
    visit_multi_write_node, MultiWriteNode;
    visit_splat_node, SplatNode;
    visit_assoc_splat_node, AssocSplatNode;
    visit_block_argument_node, BlockArgumentNode;
    visit_range_node, RangeNode;
    visit_alias_method_node, AliasMethodNode;
    visit_undef_node, UndefNode;
    visit_constant_write_node, ConstantWriteNode;
    visit_constant_path_write_node, ConstantPathWriteNode;
}

/// Every variable read in one text, and what can reach each: the table [`Receiver::Variable`]
/// points into.
///
/// - **Writes are stored once and read by index.** Every read of an instance variable is reached
///   by every write of it, so a copy per read would be a copy per pair.
/// - **Which variable a read is comes from [`scopes`]**, never re-derived: Prism's `depth` for a
///   local, what `self` is for an instance variable.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Variables {
    writes: Vec<Assignment>,
    reads: HashMap<u32, Reaching>,
    /// Every write to an instance variable a namespace in this text owns, by the variable's name:
    /// what a read in *another* text asks when this one is written in a class its object has.
    instances: HashMap<String, Vec<InstanceWrite>>,
    /// Every receiverless `def initialize`, by where the `def` starts: the span rubydex files the
    /// method under.
    initializers: HashMap<u32, Initializer>,
    /// Every other receiverless `def`'s opening writes, by where it starts.
    openings: HashMap<u32, Vec<String>>,
    /// What each empty container a local starts as is filled with in its own method, by the
    /// write's index.
    fills: HashMap<usize, Fill>,
    /// Every reflective write on `self` whose name is a pattern, not one spelling
    /// (`instance_variable_set(:"@#{key}", v)`).
    patterns: Vec<(scopes::Spelled, InstanceWrite)>,
    /// The names a reflective write on **another** object can reach: any object's variable of
    /// that spelling.
    foreign: Vec<scopes::Spelled>,
    /// The names a reflective write on the top level's `self` can reach.
    main: Vec<scopes::Spelled>,
    /// Every declared reader ([`scopes::readers`]), by where its name starts: the span rubydex
    /// files the method under.
    readers: HashMap<u32, scopes::Accessor>,
}

impl Variables {
    /// What can reach the read whose name starts at `read`, or `None` where nothing in the text
    /// writes or binds it: an instance variable some other file assigns, or a numbered parameter.
    #[must_use]
    pub fn reaching(&self, read: u32) -> Option<&Reaching> {
        self.reads.get(&read)
    }

    /// One write, by the index [`Reaching::writes`] holds.
    #[must_use]
    pub fn assignment(&self, write: usize) -> Option<&Assignment> {
        self.writes.get(write)
    }

    /// Every write to the instance variable `name` in this text, with the level it is written at:
    /// the spelled ones, and every reflective one whose name can be `name`.
    #[must_use]
    pub fn instance_writes(&self, name: &str) -> Vec<InstanceWrite> {
        let mut writes = self.instances.get(name).cloned().unwrap_or_default();
        writes.extend(
            self.patterns
                .iter()
                .filter(|(spelled, _)| spelled.matches(name))
                .map(|(_, write)| write.clone()),
        );
        writes
    }

    /// The reader whose name starts at `at`: the variable it returns, and on which object
    /// ([`scopes::Accessor::level`]).
    ///
    /// Held here, in the table every document's reads are answered from, because a reader is read
    /// from *its* document, and [`scopes`]' own memo holds two texts.
    #[must_use]
    pub fn reader_at(&self, at: u32) -> Option<&scopes::Accessor> {
        self.readers.get(&at)
    }

    /// Whether a reflective write on the top level's `self` in this text can reach a variable
    /// spelled `name`.
    #[must_use]
    pub fn reaches_main(&self, name: &str) -> bool {
        self.main.iter().any(|spelled| spelled.matches(name))
    }

    /// Whether a reflective write in this text on another object can reach a variable spelled
    /// `name`.
    #[must_use]
    pub fn reaches_another(&self, name: &str) -> bool {
        self.foreign.iter().any(|spelled| spelled.matches(name))
    }

    /// The `def initialize` starting at `def`, or `None` where no such method starts there.
    #[must_use]
    pub fn initializer(&self, def: u32) -> Option<&Initializer> {
        self.initializers.get(&def)
    }

    /// What the empty container the write at index `write` starts is filled with in its method
    ///.
    #[must_use]
    pub fn fill(&self, write: usize) -> Option<&Fill> {
        self.fills.get(&write)
    }

    /// The instance variables the receiverless `def` starting at `def` (not `initialize`) writes as
    /// statements of its own body before any that may `return`.
    #[must_use]
    pub fn opening(&self, def: u32) -> Option<&[String]> {
        self.openings.get(&def).map(Vec::as_slice)
    }

    fn write(&mut self, at: u32, shape: Receiver) -> usize {
        self.writes.push(Assignment { at, shape });
        self.writes.len() - 1
    }
}

/// The values a method puts into an empty container a local starts as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fill {
    /// `Array` or `Hash`.
    pub class: &'static str,
    /// By the position of the class's type parameter (an `Array`'s element, a `Hash`'s key and
    /// value): each value added, and where it is written, which places the scope it is read in.
    pub held: Vec<Vec<(u32, Receiver)>>,
}

/// One write to an instance variable, as a read in another text finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceWrite {
    /// Its index: [`Variables::assignment`] says where it is and what it holds.
    pub write: usize,
    /// [`scopes::Group::level`] of the variable written. `None` where `self` is not known (a block
    /// straight in a namespace body), so it may be a write at any level.
    pub level: Option<i32>,
    /// Made by an `attr_writer` or `attr_accessor`: it holds whatever the writer's calls pass
    /// (`types::setter_values`).
    pub setter: bool,
}

/// What a `def initialize` does before any other method of its object can run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Initializer {
    /// The instance variables it writes as statements of its own body.
    pub writes: Vec<String>,
    /// Whether one of those statements is `super`, so the `initialize` above it runs too.
    pub supers: bool,
}

/// One write, as the type side needs it: where it is, and what it gives the variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    /// Where the variable's name is written, which places the scope its value is read in.
    pub at: u32,
    /// What the variable holds after the write. [`Receiver::Unknown`] for a write nothing can
    /// read, which is still a write that reaches.
    pub shape: Receiver,
}

/// Everything that can reach one read.
///
/// **One untyped member refuses the whole answer**, [`types`](super::types)' rule, so nothing here
/// is ever dropped for being unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaching {
    /// The writes, by index into [`Variables`]. Shared by every read of an instance variable,
    /// which each reach all of its writes.
    pub writes: Rc<[usize]>,
    /// `nil` can reach: no write has run on every path here, and no parameter binds the name.
    pub nil: bool,
    /// A parameter binds the name and no write has run on every path here, so the parameter's own
    /// value can reach, as this shape. Shared by every read of the parameter.
    pub bound: Option<Rc<Receiver>>,
    /// For an instance variable, what the type side needs to find the writes other texts make.
    /// `None` for a local, and boxed so a local's row does not carry its room.
    pub instance: Option<Box<InstanceRead>>,
    /// What the checks around the read say about each value that reaches it. Empty
    /// for an instance variable: a call between the check and the read may write it.
    pub narrowed: Box<[Narrowing]>,
    /// **Every read of the local leaves its object as it was and hands it nowhere** but out of its
    /// method ([`Use::Reads`]): no write into it, no argument, no other variable, no block it may
    /// be handed to. What the object held when it was made then still holds at the read
    /// (`types`' `Typed::shaped`). `false` for an instance variable, which any method may write
    /// into.
    pub kept: bool,
}

/// A read of an instance variable, which any method of its object's classes can write.
///
/// [`Reaching::writes`] holds this text's writes to the same variable, and [`Reaching::nil`] the
/// rule for this text alone. Where [`Self::level`] names the object, the type side folds the writes
/// of every class it can be an instance of instead ([`types`](super::types)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRead {
    /// As Ruby spells it, `@` included.
    pub name: String,
    /// [`scopes::Group::level`]: `None` at the top level or on an island, and for a
    /// [`Self::loose`] read.
    pub level: Option<i32>,
    /// The reading method has written the variable on every path to the read, so `nil` cannot
    /// reach it whatever other methods do.
    pub set_here: bool,
    /// In a block or lambda written straight in a namespace body ([`scopes::Group::loose`]), whose
    /// `self` only the block's call can say: the type side asks it. Its row holds a
    /// write nothing types, so this text alone refuses it.
    pub loose: bool,
    /// The name of the innermost `def` the read is written in, blocks included: what runs it, for
    /// a controller's callbacks.
    pub method: Option<String>,
    /// The one write that decides the read, by its index in [`Variables`]: the last
    /// plain `@x = value` in the reading `def` that runs on every path, with nothing between them
    /// that can run code. `None` leaves the read to every write of the object.
    pub decided: Option<usize>,
}

/// One write of the variable being answered, placed for [`reaching_local`].
struct Placed {
    /// Its index in [`Variables`].
    id: usize,
    /// Where its name starts.
    name: u32,
    /// Where it takes effect.
    at: u32,
    /// The blocks and lambdas around it inside the variable's own scope, innermost first.
    closures: Vec<(u32, u32)>,
}

/// The branching constructs of one text as a tree, so the ones around a position are a walk out,
/// not a scan.
struct Regions {
    /// Sorted by start, the wider first where two start together.
    regions: Vec<Region>,
    /// Each region's parent: the next one out, or `None` at the top.
    parents: Vec<Option<usize>>,
}

impl Regions {
    fn new(mut regions: Vec<Region>) -> Self {
        regions.sort_by_key(|region| (region.span.0, std::cmp::Reverse(region.span.1)));
        let mut parents = Vec::with_capacity(regions.len());
        let mut open: Vec<usize> = Vec::new();
        for (index, region) in regions.iter().enumerate() {
            while let Some(&outer) = open.last() {
                let around = &regions[outer];
                if around.span.0 <= region.span.0 && region.span.1 <= around.span.1 {
                    break;
                }
                open.pop();
            }
            parents.push(open.last().copied());
            open.push(index);
        }
        Self { regions, parents }
    }

    /// Every region holding `at`, innermost first.
    ///
    /// The innermost is the last region starting at or before `at`, or the first of its parents
    /// that holds `at`: regions nest, so any region holding `at` that starts before that one is one
    /// of its parents.
    fn around(&self, at: u32) -> impl Iterator<Item = &Region> + '_ {
        let mut first = self
            .regions
            .partition_point(|region| region.span.0 <= at)
            .checked_sub(1);
        while let Some(index) = first
            && !self.regions[index].holds(at)
        {
            first = self.parents[index];
        }
        std::iter::successors(first, |index| self.parents[*index]).map(|index| &self.regions[index])
    }
}

/// Whether the write at `write` has run on every path to `read`: every branch around the write is
/// around the read too.
///
/// Only the innermost branch is asked: branches nest, so each one further out holds that one.
fn dominates(regions: &Regions, write: u32, read: u32) -> bool {
    regions
        .around(write)
        .find(|region| region.branch)
        .is_none_or(|branch| branch.holds(read))
}

/// The blocks and lambdas around `at` that sit inside `scope`, innermost first.
fn closures_around(regions: &Regions, scope: (u32, u32), at: u32) -> Vec<(u32, u32)> {
    regions
        .around(at)
        .filter(|region| region.closure && strictly_inside(region.span, scope))
        .map(|region| region.span)
        .collect()
}

/// Whether `span` lies inside `scope` and is not all of it.
fn strictly_inside(span: (u32, u32), scope: (u32, u32)) -> bool {
    scope.0 <= span.0 && span.1 <= scope.1 && span != scope
}

/// Which writes of a local can reach one read of it, and whether `nil` or its parameter can.
///
/// 1. **Every write before the read, back to the last one that runs on every path.** That one
///    kills the writes above it; a write in a branch below it adds and kills nothing.
/// 2. **A loop around the read brings back the writes below it**, unless the variable is set again
///    on every turn before the read. A block counts as a loop. The variable's own scope does not:
///    a block-local starts fresh on every call.
/// 3. **A block or lambda may run later.** A read inside one sees every write below it in the
///    variable's scope, and a write inside one, made before the read, is never killed.
/// 4. **`nil` reaches where no write has run on every path and no parameter binds the name**,
///    because Ruby creates the local as soon as it parses a write to it.
fn reaching_local(
    regions: &Regions,
    scope: (u32, u32),
    writes: &[Placed],
    bound: Option<&Rc<Receiver>>,
    read: u32,
) -> Reaching {
    let before: Vec<&Placed> = writes.iter().filter(|write| write.at <= read).collect();
    let last = before
        .iter()
        .copied()
        .filter(|write| dominates(regions, write.name, read))
        .max_by_key(|write| write.at);
    let mut reaching: Vec<usize> = before
        .iter()
        .filter(|write| last.is_none_or(|last| write.at > last.at || write.id == last.id))
        .map(|write| write.id)
        .collect();
    let pinned = last.map(|write| write.name).or(bound.map(|_| scope.0));
    for region in regions.around(read).filter(|region| {
        region.repeat
            && strictly_inside(region.span, scope)
            && pinned.is_none_or(|at| !region.holds(at))
    }) {
        reaching.extend(
            writes
                .iter()
                // Takes effect after the read, so on this turn it has not run: `total += 1` reads
                // `total` where its name is, and its own write comes back next turn.
                .filter(|write| write.at > read && region.holds(write.name))
                .map(|write| write.id),
        );
    }
    if let Some(closure) = regions
        .around(read)
        .find(|region| region.closure && strictly_inside(region.span, scope))
    {
        reaching.extend(
            writes
                .iter()
                .filter(|write| write.name >= closure.span.1)
                .map(|write| write.id),
        );
    }
    reaching.extend(
        writes
            .iter()
            .filter(|write| {
                write
                    .closures
                    .iter()
                    .any(|closure| closure.0 < read && !(closure.0 <= read && read < closure.1))
            })
            .map(|write| write.id),
    );
    reaching.sort_unstable();
    reaching.dedup();
    let open = last.is_none();
    Reaching {
        writes: reaching.into(),
        nil: open && bound.is_none(),
        bound: if open { bound.cloned() } else { None },
        instance: None,
        narrowed: Box::default(),
        kept: false,
    }
}

/// What a check says about a local where it holds.
///
/// Each is about **one value**: it narrows what a write handed the local, never the read as a
/// whole ([`Narrowing::of`]), so a write below the check is not narrowed by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fact {
    /// Neither `nil` nor `false`: `if x`, `x && …`, `return unless x`.
    Truthy,
    /// `nil` or `false`: `unless x`, `x || …`.
    Falsy,
    /// `nil`: `x.nil?`, `x == nil`.
    Nil,
    /// Anything but `nil`: `!x.nil?`, `x != nil`.
    NotNil,
    /// An instance of one of these classes, each named by a constant ending at the offset held, or
    /// `nil` where `nil` was one of them: `x.is_a?(K)`, `case x when K, nil`.
    Is { classes: Box<[u32]>, nil: bool },
    /// An instance of none of them: the other side of an `is_a?`, a `case`'s `else`.
    IsNot { classes: Box<[u32]>, nil: bool },
}

/// One fact that holds at a read about one value that can reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Narrowing {
    /// The write it is about, by its index in [`Variables`]; `None` for what reaches with no write
    /// at all: `nil`, or the parameter's own value.
    pub of: Option<usize>,
    pub fact: Fact,
}

/// One fact about a local, with the check that makes it.
type Held<'c> = (&'c Check, &'c Checked);

/// A check the walk found: where its facts hold, and what they are.
struct Check {
    /// A branch the check guards, or the rest of a statement list after a guard.
    holds: (u32, u32),
    /// The conditional the check is made by, and its predicate. A write inside the one and
    /// outside the other runs after the check, wherever it is written: `x.foo if x`'s body comes
    /// first in the text.
    construct: (u32, u32),
    predicate: (u32, u32),
    facts: Vec<Checked>,
}

/// One fact a check makes about one local.
#[derive(Clone)]
struct Checked {
    /// Where the local is written in the check: an occurrence [`scopes`] reports.
    local: u32,
    /// Where the check reads the value. A value that takes effect after it was not checked.
    at: u32,
    fact: Fact,
}

/// A local read or written in a check, with what identifies it within one scope.
struct Named<'n> {
    name: &'n [u8],
    depth: u32,
}

/// What holds about which locals where `predicate` is truthy (`holds`) or falsy.
///
/// - **A local read**, or a local written (`if (user = find)`), is `Truthy` or `Falsy`.
/// - **`x.nil?`, `x == nil`, `x != nil`** are `Nil` and `NotNil`. **`x.is_a?(K)` and
///   `x.kind_of?(K)`** are `Is` and `IsNot` for a constant `K`; `x.instance_of?(K)` only `Is`,
///   since a subclass's instance is not one.
/// - **`!`, `not`, `&&`, `||` and parentheses** are Ruby's. A negated conjunction (`unless a && b`,
///   the `else` of `if a && b`) says nothing about either side, and neither does a disjunction
///   that holds.
/// - **`&.` says nothing**: `x&.nil?` is `nil` where `x` is.
fn facts_of<'pr>(predicate: &Node<'pr>, holds: bool, into: &mut Vec<(Checked, Named<'pr>)>) {
    if let Some(inner) = unparenthesised(predicate) {
        return facts_of(&inner, holds, into);
    }
    let truth = if holds { Fact::Truthy } else { Fact::Falsy };
    if let Some(read) = predicate.as_local_variable_read_node() {
        let location = read.location();
        into.push((
            Checked {
                local: location.start_offset() as u32,
                at: location.end_offset() as u32,
                fact: truth,
            },
            Named {
                name: read.name().as_slice(),
                depth: read.depth(),
            },
        ));
        return;
    }
    if let Some(write) = predicate.as_local_variable_write_node() {
        into.push((
            Checked {
                local: write.name_loc().start_offset() as u32,
                at: write.location().end_offset() as u32,
                fact: truth,
            },
            Named {
                name: write.name().as_slice(),
                depth: write.depth(),
            },
        ));
        return;
    }
    if let Some(both) = predicate.as_and_node() {
        if holds {
            facts_of(&both.left(), true, into);
            facts_of(&both.right(), true, into);
        }
        return;
    }
    if let Some(either) = predicate.as_or_node() {
        if !holds {
            facts_of(&either.left(), false, into);
            facts_of(&either.right(), false, into);
        }
        return;
    }
    let Some(call) = predicate.as_call_node() else {
        return;
    };
    if call.is_safe_navigation() {
        return;
    }
    let Some(receiver) = call.receiver() else {
        return;
    };
    let name = call.name().as_slice();
    let arguments: Vec<Node<'pr>> = call
        .arguments()
        .map(|written| written.arguments().iter().collect())
        .unwrap_or_default();
    if name == b"!" && arguments.is_empty() && call.block().is_none() {
        return facts_of(&receiver, !holds, into);
    }
    let receiver = unparenthesised(&receiver).unwrap_or(receiver);
    let Some(read) = receiver.as_local_variable_read_node() else {
        return;
    };
    if call.block().is_some() {
        return;
    }
    let fact = match (name, arguments.as_slice()) {
        (b"nil?", []) => Some(if holds { Fact::Nil } else { Fact::NotNil }),
        (b"==", [nil]) if nil.as_nil_node().is_some() => {
            Some(if holds { Fact::Nil } else { Fact::NotNil })
        }
        (b"!=", [nil]) if nil.as_nil_node().is_some() => {
            Some(if holds { Fact::NotNil } else { Fact::Nil })
        }
        (b"is_a?" | b"kind_of?" | b"instance_of?", [class]) if is_constant(class) => {
            let classes: Box<[u32]> = Box::new([class.location().end_offset() as u32]);
            if holds {
                Some(Fact::Is {
                    classes,
                    nil: false,
                })
            } else if name == b"instance_of?" {
                None
            } else {
                Some(Fact::IsNot {
                    classes,
                    nil: false,
                })
            }
        }
        _ => None,
    };
    if let Some(fact) = fact {
        into.push((
            Checked {
                local: read.location().start_offset() as u32,
                at: call.location().end_offset() as u32,
                fact,
            },
            Named {
                name: read.name().as_slice(),
                depth: read.depth(),
            },
        ));
    }
}

/// Whether a statement list always leaves it: its last statement is a `return`, `next`, `break`,
/// `raise` or `fail`.
fn stops(statements: Option<&StatementsNode<'_>>) -> bool {
    statements
        .and_then(|found| found.body().iter().last())
        .is_some_and(|last| stops_here(&last))
}

/// Whether one node always leaves the statement list it is in.
fn stops_here(node: &Node<'_>) -> bool {
    node.as_return_node().is_some()
        || node.as_next_node().is_some()
        || node.as_break_node().is_some()
        || node.as_call_node().is_some_and(|call| is_raise(&call))
}

/// Whether a statement list writes the local `named` as one of its own statements, so every path
/// through it does.
fn writes(statements: Option<&StatementsNode<'_>>, named: &Named<'_>) -> bool {
    statements.is_some_and(|found| {
        found
            .body()
            .iter()
            .any(|statement| writes_here(&statement, named))
    })
}

/// Whether one node is a write of the local `named`, in the scope the check reads it in.
fn writes_here(node: &Node<'_>, named: &Named<'_>) -> bool {
    let same = |name: &[u8], depth: u32| name == named.name && depth == named.depth;
    if let Some(write) = node.as_local_variable_write_node() {
        return same(write.name().as_slice(), write.depth());
    }
    if let Some(write) = node.as_local_variable_or_write_node() {
        return same(write.name().as_slice(), write.depth());
    }
    if let Some(write) = node.as_local_variable_and_write_node() {
        return same(write.name().as_slice(), write.depth());
    }
    if let Some(write) = node.as_local_variable_operator_write_node() {
        return same(write.name().as_slice(), write.depth());
    }
    false
}

/// Which values reaching `read` the checks around it narrow, and by what.
///
/// A value is narrowed by a check that holds at the read where it **took effect before the
/// check read it**, so every path from it to the read passed the check:
///
/// - **a write below the check is its own value**, and not narrowed by it: `if x; x = y if z;
///   x.foo` is the checked value or `y`;
/// - **a write inside the conditional and outside its predicate is below it too**, though a
///   modifier writes it above: `(x = nil) if x`, `x &&= 2 if x`;
/// - **a write in a block or lambda the check is not inside may run at any time**, the check's
///   included: it is never narrowed.
///
/// What reaches with no write, `nil` or the parameter's own value, is above every check.
fn narrowings(
    relevant: &[Held<'_>],
    written: &[Placed],
    reaching: &Reaching,
    read: u32,
) -> Box<[Narrowing]> {
    let mut found: Vec<Narrowing> = Vec::new();
    let inside = |span: (u32, u32), at: u32| span.0 <= at && at < span.1;
    for (check, checked) in relevant {
        if !inside(check.holds, read) {
            continue;
        }
        if reaching.nil || reaching.bound.is_some() {
            found.push(Narrowing {
                of: None,
                fact: checked.fact.clone(),
            });
        }
        for id in reaching.writes.iter() {
            let Some(write) = written.iter().find(|write| write.id == *id) else {
                continue;
            };
            let above = write.at <= checked.at
                && (!inside(check.construct, write.name) || inside(check.predicate, write.name));
            let settled = write
                .closures
                .iter()
                .all(|closure| closure.0 <= checked.at && checked.at < closure.1);
            if above && settled {
                found.push(Narrowing {
                    of: Some(*id),
                    fact: checked.fact.clone(),
                });
            }
        }
    }
    found.dedup();
    found.into_boxed_slice()
}

/// What holds after `statement` for the rest of its statement list, where it is a guard: a
/// conditional one branch of which never falls through to the next statement for the local a fact
/// is about.
///
/// A branch **blocks** a local where it always leaves the list (`return`, `next`, `break`, `raise`)
/// or writes the local as one of its own statements. A value from above that reaches past the
/// guard then came through the other branch, where that branch's facts hold:
/// `return unless user` and `user = create unless user` both leave `user` truthy for what was
/// there before. An `elsif` is a conditional of its own, and is not followed.
fn guard_facts(statement: &Node<'_>) -> Option<((u32, u32), Vec<Checked>)> {
    let mut kept = Vec::new();
    let mut keep = |predicate: &Node<'_>, holds: bool, blocks: &dyn Fn(&Named<'_>) -> bool| {
        let mut found = Vec::new();
        facts_of(predicate, holds, &mut found);
        kept.extend(
            found
                .into_iter()
                .filter(|(_, named)| blocks(named))
                .map(|(checked, _)| checked),
        );
    };
    let predicate = if let Some(found) = statement.as_if_node() {
        let otherwise = found.subsequent();
        if otherwise
            .as_ref()
            .is_some_and(|node| node.as_else_node().is_none())
        {
            return None;
        }
        let taken = found.statements();
        let otherwise = otherwise.and_then(|node| node.as_else_node()?.statements());
        let predicate = found.predicate();
        keep(&predicate, false, &|named| {
            stops(taken.as_ref()) || writes(taken.as_ref(), named)
        });
        keep(&predicate, true, &|named| {
            stops(otherwise.as_ref()) || writes(otherwise.as_ref(), named)
        });
        predicate
    } else if let Some(found) = statement.as_unless_node() {
        let taken = found.statements();
        let otherwise = found.else_clause().and_then(|node| node.statements());
        let predicate = found.predicate();
        keep(&predicate, true, &|named| {
            stops(taken.as_ref()) || writes(taken.as_ref(), named)
        });
        keep(&predicate, false, &|named| {
            stops(otherwise.as_ref()) || writes(otherwise.as_ref(), named)
        });
        predicate
    } else if let Some(found) = statement.as_and_node() {
        // `x.nil? and return`: the right side runs where the left holds.
        let right = found.right();
        keep(&found.left(), false, &|named| {
            stops_here(&right) || writes_here(&right, named)
        });
        found.left()
    } else {
        // `x or return`, `user || raise(…)`: the right side runs where the left fails.
        let found = statement.as_or_node()?;
        let right = found.right();
        keep(&found.left(), true, &|named| {
            stops_here(&right) || writes_here(&right, named)
        });
        found.left()
    };
    (!kept.is_empty()).then(|| (span_of(&predicate), kept))
}

/// The local a `case` asks about, read or written there: where its name starts, and where the
/// value is read.
fn cased(predicate: &Node<'_>) -> Option<(u32, u32)> {
    if let Some(inner) = unparenthesised(predicate) {
        return cased(&inner);
    }
    if let Some(read) = predicate.as_local_variable_read_node() {
        let location = read.location();
        return Some((location.start_offset() as u32, location.end_offset() as u32));
    }
    let write = predicate.as_local_variable_write_node()?;
    Some((
        write.name_loc().start_offset() as u32,
        write.location().end_offset() as u32,
    ))
}

/// Whether a `rescue` clause holds a `retry`, which runs its `begin` again.
struct Retries(bool);

impl<'pr> Visit<'pr> for Retries {
    fn visit_retry_node(&mut self, _: &ruby_prism::RetryNode<'pr>) {
        self.0 = true;
    }
}

/// A node's span.
fn span_of(node: &Node<'_>) -> (u32, u32) {
    let location = node.location();
    (location.start_offset() as u32, location.end_offset() as u32)
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

/// One exit, with what the path to it says about the block ([`Exits::given`]).
type Filed<'pr> = (Exit<'pr>, Option<bool>);

/// The expressions one `def` hands back, found by the span the graph filed it under.
struct Exits<'pr> {
    /// The `def`s the walk is inside, innermost last.
    ///
    /// A stack because [`returns_in`] reads every `def` in a document from one parse. An exit
    /// belongs to the innermost open `def`, so a `return` in a nested `def` is that method's, never
    /// the outer one's.
    open: Vec<(u32, u32)>,
    found: HashMap<(u32, u32), Vec<Filed<'pr>>>,
    /// Every `def`'s name, by its span, for the [`Receiver::BlockGiven`] its exits are filed in.
    names: HashMap<(u32, u32), String>,
    /// How many `->` lambdas the walk is inside.
    ///
    /// - **A `return` inside a lambda returns from the lambda**, so filing it as the method's exit
    ///   would decline a method that answers fine. One engine has
    ///   `def code_column; { header: :code, data: ->(x) do return if x.code.blank?; … end }; end`:
    ///   the method returns a `Hash`, and the `return` never leaves the proc.
    /// - **A counter, not a flag**, because lambdas nest. Zeroed across a `def`, which owns its
    ///   `return`s again.
    /// - **Only `->` is fenced.** `proc`, `Proc.new` and ordinary blocks keep the method's `return`
    ///   (that is the semantic difference). `lambda { … }` is a receiverless call this cannot tell
    ///   from another method named `lambda` without resolving it, so it is left alone: a wrongly
    ///   kept exit only declines a method, the safe direction.
    lambdas: usize,
    /// What the path to here says about the innermost open `def`'s block: each
    /// conditional on `block_given?` or on the `def`'s own `&block` that the walk is inside, and
    /// each guard above it in its statement list (`return to_enum unless block_given?`). An exit
    /// is filed with the last one ([`Receiver::BlockGiven`]).
    given: Vec<bool>,
    /// The innermost open `def`'s `&block` parameter, where its body never writes the name: a read
    /// of it is `block_given?` in another spelling.
    block: Option<Vec<u8>>,
    /// How many blocks and lambdas deep the walk is inside that `def`, which a read of `block`
    /// must reach up through to be the parameter and not a block's own local of that name.
    scopes: u32,
    /// Every `def` whose body asks whether it was passed a block, by its span: the `def`s a call
    /// must say so to ([`Shapes::asks`]).
    asks: HashSet<(u32, u32)>,
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
        self.names.insert(
            here,
            String::from_utf8_lossy(node.name().as_slice()).into_owned(),
        );
        self.open.push(here);
        // A `def` inside a lambda owns its `return`s, so the fence is lifted for this body and
        // restored after. Its block is its own too: what the `def` around it asked says nothing
        // about a call of this one.
        let fenced = std::mem::take(&mut self.lambdas);
        let given = std::mem::take(&mut self.given);
        let scopes = std::mem::take(&mut self.scopes);
        let block = std::mem::replace(&mut self.block, own_block(node));
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
        self.given = given;
        self.scopes = scopes;
        self.block = block;
        self.open.pop();
    }

    fn visit_lambda_node(&mut self, node: &ruby_prism::LambdaNode<'pr>) {
        // Everything inside is the lambda's exit, not this method's; see [`Exits::lambdas`]. The
        // lambda itself is still a value, so a `def` ending in one reaches [`Exits::tail`] and
        // answers `Proc`. This fences the `return` walk, not the tail read.
        self.lambdas += 1;
        self.scopes += 1;
        ruby_prism::visit_lambda_node(self, node);
        self.scopes -= 1;
        self.lambdas -= 1;
    }

    fn visit_block_node(&mut self, node: &BlockNode<'pr>) {
        self.scopes += 1;
        ruby_prism::visit_block_node(self, node);
        self.scopes -= 1;
    }

    // **A `return` below a conditional on the block is filed with what that side says.** The tail
    // is [`Exits::tail`]'s; these are for the `return`s the walk meets anywhere else.
    fn visit_if_node(&mut self, node: &ruby_prism::IfNode<'pr>) {
        let (holds, fails) = self.implied(&node.predicate());
        self.visit(&node.predicate());
        if let Some(statements) = node.statements() {
            self.under(holds, |exits| exits.visit_statements_node(&statements));
        }
        if let Some(otherwise) = node.subsequent() {
            self.under(fails, |exits| exits.visit(&otherwise));
        }
    }

    fn visit_unless_node(&mut self, node: &ruby_prism::UnlessNode<'pr>) {
        let (holds, fails) = self.implied(&node.predicate());
        self.visit(&node.predicate());
        if let Some(statements) = node.statements() {
            self.under(fails, |exits| exits.visit_statements_node(&statements));
        }
        if let Some(otherwise) = node.else_clause() {
            self.under(holds, |exits| exits.visit_else_node(&otherwise));
        }
    }

    // A guard says something about every statement after it in its list, and nothing about the
    // list around that one.
    fn visit_statements_node(&mut self, node: &StatementsNode<'pr>) {
        let held = self.given.len();
        for statement in node.body().iter() {
            self.visit(&statement);
            if let Some(fact) = self.guard(&statement) {
                self.given.push(fact);
            }
        }
        self.given.truncate(held);
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
/// - **A sanity guard, not a limit real code meets.** The deepest tail over the six reference
///   corpora is 4 levels, in two methods, so 10 leaves room: a method is never declined for how
///   its branches nest, only for what they hold. It was 4, which declined those two.
/// - **Depth is nesting, and only [`Exits::branch`] charges it.** An `elsif` is one more branch of
///   the same conditional, as a `when` is, and an `else` is a branch like the one before it. Charging
///   either again spent two levels on every `else` and one on every `elsif`, so a fourth `elsif` or
///   a ternary inside an `else` read as unreadable and declined methods whose every branch is a
///   `String` (`nesting_is_what_the_branch_bound_counts`).
const MAX_BRANCHES: usize = 10;

impl<'pr> Exits<'pr> {
    /// A walk with `open` already open, whose `&block` parameter is `block`.
    fn new(open: Vec<(u32, u32)>, block: Option<Vec<u8>>) -> Self {
        Self {
            open,
            found: HashMap::new(),
            names: HashMap::new(),
            lambdas: 0,
            given: Vec::new(),
            block,
            scopes: 0,
            asks: HashSet::new(),
        }
    }

    /// One exit, filed under the innermost open `def`.
    ///
    /// A `return` outside every `def` (a file's top level, a `class` body) is not a method's exit
    /// and is dropped, not filed against the last `def` passed.
    fn push(&mut self, exit: Exit<'pr>) {
        if self.lambdas > 0 {
            return;
        }
        // **A `raise` hands back nothing, so it is no exit**, here where exits are filed, not in
        // each reader: `return x if ok; raise` returns what `x` is. A `def` or block whose every
        // exit raises is left with none, which declines it, as an empty body with no readable exit
        // does.
        if let Exit::Written(node) = &exit
            && node.as_call_node().is_some_and(|call| is_raise(&call))
        {
            return;
        }
        let given = self.given.last().copied();
        if let Some(open) = self.open.last() {
            self.found.entry(*open).or_default().push((exit, given));
        }
    }

    /// Walk `inside` with `fact` known about the block, where there is one.
    fn under(&mut self, fact: Option<bool>, inside: impl FnOnce(&mut Self)) {
        let held = self.given.len();
        self.given.extend(fact);
        inside(self);
        self.given.truncate(held);
    }

    /// What each side of a conditional on `predicate` says about the innermost open `def`'s
    /// block ([`implied`]), with that `def` filed as one that asks where either side says
    /// something.
    fn implied(&mut self, predicate: &Node<'_>) -> (Option<bool>, Option<bool>) {
        let scopes = self.scopes;
        let block = self.block.as_deref();
        let sides = implied(predicate, &|read: &LocalVariableReadNode<'_>| {
            block == Some(read.name().as_slice()) && read.depth() == scopes
        });
        self.asks
            .extend(self.open.last().copied().filter(|_| sides != (None, None)));
        sides
    }

    /// What the statements after `statement` in its list say about the block, where `statement`
    /// is a conditional on it with one side that always leaves the method.
    fn guard(&mut self, statement: &Node<'_>) -> Option<bool> {
        if let Some(found) = statement.as_if_node() {
            let otherwise = found.subsequent();
            // An `elsif` is a conditional of its own, which this does not follow.
            if otherwise
                .as_ref()
                .is_some_and(|node| node.as_else_node().is_none())
            {
                return None;
            }
            let otherwise = otherwise.and_then(|node| node.as_else_node()?.statements());
            let (holds, fails) = self.implied(&found.predicate());
            return beyond(found.statements(), otherwise, holds, fails);
        }
        let found = statement.as_unless_node()?;
        let otherwise = found.else_clause().and_then(|node| node.statements());
        let (holds, fails) = self.implied(&found.predicate());
        beyond(found.statements(), otherwise, fails, holds)
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
            let (holds, fails) = self.implied(&found.predicate());
            self.under(holds, |exits| {
                exits.branch(found.statements().as_ref(), depth);
            });
            self.under(fails, |exits| match found.subsequent() {
                // An `elsif` or an `else`, at this conditional's depth: its branch charges the level.
                Some(otherwise) => exits.tail(otherwise, depth),
                // No `else`, including the modifier form (`x if y` and `if y then x end` are one
                // node). Ruby returns `nil` when the condition is false. A branchless conditional
                // is one of the two shapes that say so; a bare `return` is the other, filed by
                // [`Visit::visit_return_node`].
                None => exits.push(Exit::Nil),
            });
            return;
        }
        if let Some(found) = node.as_unless_node() {
            let (holds, fails) = self.implied(&found.predicate());
            self.under(fails, |exits| {
                exits.branch(found.statements().as_ref(), depth);
            });
            self.under(holds, |exits| {
                exits.branch(
                    found
                        .else_clause()
                        .and_then(|otherwise| otherwise.statements())
                        .as_ref(),
                    depth,
                );
            });
            return;
        }
        if let Some(found) = node.as_case_node() {
            // A `case` holds only `when` arms; `case … in` is its own node, below.
            for when in found
                .conditions()
                .iter()
                .filter_map(|arm| arm.as_when_node())
            {
                self.branch(when.statements().as_ref(), depth);
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
        // **`case … in` is one exit per arm**, as `case … when` is. With no `else`, a value no
        // pattern matches raises `NoMatchingPatternError`, so there is no `nil` exit to file.
        if let Some(found) = node.as_case_match_node() {
            for arm in found.conditions().iter().filter_map(|arm| arm.as_in_node()) {
                self.branch(arm.statements().as_ref(), depth);
            }
            if let Some(otherwise) = found.else_clause() {
                self.branch(otherwise.statements().as_ref(), depth);
            }
            return;
        }
        if let Some(found) = node.as_else_node() {
            self.branch(found.statements().as_ref(), depth);
            return;
        }
        // A `begin`/`rescue` as the last statement has the same shape as one on the `def` itself,
        // and is read the same way instead of pushed whole as an unreadable exit, at the same
        // depth: its `rescue` clauses are branches, and charge their own level.
        if let Some(found) = node.as_begin_node() {
            self.rescued(&found, depth);
            return;
        }
        // A tail-position `return` is **already** this method's exit: [`Visit::visit_return_node`]
        // pushes its value. Pushing the `return` node too would add an unreadable exit, and the
        // agreement rule would decline `def title; return "x"; end` while `def title; "x"; end`
        // answers `String`.
        if node.as_return_node().is_some() {
            return;
        }
        // `rescue … retry` runs the `begin` again, so the method leaves through that `begin`'s
        // other exits, never through the `retry`.
        if node.as_retry_node().is_some() {
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
        if let Some(found) = statements {
            self.last(found, depth);
        }
    }

    /// The last statement of a list, expanded at `depth`, with what every guard above it says
    /// about the block ([`Self::guard`]).
    fn last(&mut self, statements: &StatementsNode<'pr>, depth: usize) {
        let held = self.given.len();
        let mut body = statements.body().iter().peekable();
        while let Some(statement) = body.next() {
            if body.peek().is_none() {
                self.tail(statement, depth);
            } else if let Some(fact) = self.guard(&statement) {
                self.given.push(fact);
            }
        }
        self.given.truncate(held);
    }

    /// One branch's last statement, expanded in turn.
    ///
    /// **An unwritten or empty branch is the `nil` exit.** `unless` and `case` reach their missing
    /// `else` through here, and `if x then end` returns `nil` as `if x then nil end` does.
    fn branch(&mut self, statements: Option<&StatementsNode<'pr>>, depth: usize) {
        match statements {
            Some(found) => self.last(found, depth + 1),
            None => self.push(Exit::Nil),
        }
    }
}

/// `value`, as a [`Receiver::BlockGiven`] of the `def` at `at` named `method` where the path to it
/// says something about that `def`'s block.
fn guarded(given: Option<bool>, at: u32, method: &str, value: Receiver) -> Receiver {
    match given {
        Some(given) => Receiver::BlockGiven {
            at,
            method: method.to_owned(),
            given,
            value: Box::new(value),
        },
        None => value,
    }
}

/// What each side of a conditional on `predicate` says about the enclosing `def`'s block: where
/// the predicate holds, and where it fails. `Some(true)` is "a block was passed".
///
/// - **`block_given?`**, and a read of the `def`'s own `&block` (`parameter` says which reads are
///   that), since the parameter is `nil` exactly where no block was passed.
/// - **`!`, `not`, `&&`, `||` and parentheses** are Ruby's: `a && b` holds only where both do,
///   and fails where either may.
/// - **Anything else says nothing**, including a `block_given?` with a receiver, which is some
///   other object's method.
fn implied(
    predicate: &Node<'_>,
    parameter: &dyn Fn(&LocalVariableReadNode<'_>) -> bool,
) -> (Option<bool>, Option<bool>) {
    if let Some(call) = predicate.as_call_node() {
        let bare = call.arguments().is_none() && call.block().is_none();
        if bare && call.receiver().is_none() && call.name().as_slice() == b"block_given?" {
            return (Some(true), Some(false));
        }
        if bare && call.name().as_slice() == b"!" {
            return call.receiver().map_or((None, None), |operand| {
                let (holds, fails) = implied(&operand, parameter);
                (fails, holds)
            });
        }
        return (None, None);
    }
    if let Some(read) = predicate.as_local_variable_read_node() {
        return if parameter(&read) {
            (Some(true), Some(false))
        } else {
            (None, None)
        };
    }
    if let Some(both) = predicate.as_and_node() {
        let (left, _) = implied(&both.left(), parameter);
        let (right, _) = implied(&both.right(), parameter);
        return (left.or(right), None);
    }
    if let Some(either) = predicate.as_or_node() {
        let (_, left) = implied(&either.left(), parameter);
        let (_, right) = implied(&either.right(), parameter);
        return (None, left.or(right));
    }
    if let Some(inner) = predicate
        .as_parentheses_node()
        .and_then(|found| found.body())
        .and_then(|body| body.as_statements_node())
        .and_then(|body| {
            let mut statements = body.body().iter();
            let first = statements.next()?;
            statements.next().is_none().then_some(first)
        })
    {
        return implied(&inner, parameter);
    }
    (None, None)
}

/// What the rest of a statement list says, past a conditional whose sides say `holds` and `fails`:
/// the side that does not leave the method, where the other one always does.
fn beyond(
    taken: Option<StatementsNode<'_>>,
    otherwise: Option<StatementsNode<'_>>,
    holds: Option<bool>,
    fails: Option<bool>,
) -> Option<bool> {
    match (leaves(taken.as_ref()), leaves(otherwise.as_ref())) {
        (true, false) => fails,
        (false, true) => holds,
        _ => None,
    }
}

/// Whether a branch always leaves the method: its last statement is a `return`, or a `raise`.
fn leaves(statements: Option<&StatementsNode<'_>>) -> bool {
    statements
        .and_then(|found| found.body().iter().last())
        .is_some_and(|last| {
            last.as_return_node().is_some()
                || last.as_call_node().is_some_and(|call| is_raise(&call))
        })
}

/// A `def`'s `&block` parameter, where nothing in its body writes that name again.
fn own_block(node: &DefNode<'_>) -> Option<Vec<u8>> {
    let name = node.parameters()?.block()?.name()?.as_slice().to_vec();
    let mut writes = Rewrites {
        name: &name,
        found: false,
    };
    if let Some(body) = node.body() {
        writes.visit(&body);
    }
    (!writes.found).then_some(name)
}

/// Whether a local of one name is written anywhere in a body, at any depth.
struct Rewrites<'n> {
    name: &'n [u8],
    found: bool,
}

impl<'pr> Visit<'pr> for Rewrites<'_> {
    fn visit_local_variable_write_node(&mut self, node: &ruby_prism::LocalVariableWriteNode<'pr>) {
        self.found |= node.name().as_slice() == self.name;
        ruby_prism::visit_local_variable_write_node(self, node);
    }

    fn visit_local_variable_target_node(
        &mut self,
        node: &ruby_prism::LocalVariableTargetNode<'pr>,
    ) {
        self.found |= node.name().as_slice() == self.name;
    }

    fn visit_local_variable_operator_write_node(
        &mut self,
        node: &ruby_prism::LocalVariableOperatorWriteNode<'pr>,
    ) {
        self.found |= node.name().as_slice() == self.name;
        ruby_prism::visit_local_variable_operator_write_node(self, node);
    }

    fn visit_local_variable_or_write_node(
        &mut self,
        node: &ruby_prism::LocalVariableOrWriteNode<'pr>,
    ) {
        self.found |= node.name().as_slice() == self.name;
        ruby_prism::visit_local_variable_or_write_node(self, node);
    }

    fn visit_local_variable_and_write_node(
        &mut self,
        node: &ruby_prism::LocalVariableAndWriteNode<'pr>,
    ) {
        self.found |= node.name().as_slice() == self.name;
        ruby_prism::visit_local_variable_and_write_node(self, node);
    }
}

/// The `next`s and `break`s of one block's own body: the two ways out [`Exits`] does not see
///.
///
/// - **This block's own only; the fence keeps the walk linear.** A `next` or `break` in a nested
///   block, a lambda, a `def` or a loop (`while`, `until`, `for`) belongs to that construct. Not
///   descending also keeps every enclosing block from re-walking the same subtree: in a file of
///   `describe do … it do … end … end`, the whole file once per level.
/// - **The value is the argument, or `nil` for a bare one.** `next a, b` hands back an `Array`,
///   which no shape here says, so it makes the block unreadable, as does a `redo`.
#[derive(Default)]
struct Leaving<'pr> {
    nexts: Vec<Option<Node<'pr>>>,
    breaks: Vec<Option<Node<'pr>>>,
    unreadable: bool,
}

impl<'pr> Leaving<'pr> {
    fn value(&mut self, arguments: Option<ruby_prism::ArgumentsNode<'pr>>) -> Option<Node<'pr>> {
        let arguments = arguments?;
        let one = exactly_one(&arguments);
        if one.is_none() {
            self.unreadable = true;
        }
        one
    }
}

impl<'pr> Visit<'pr> for Leaving<'pr> {
    fn visit_next_node(&mut self, node: &ruby_prism::NextNode<'pr>) {
        let value = self.value(node.arguments());
        self.nexts.push(value);
    }

    fn visit_break_node(&mut self, node: &ruby_prism::BreakNode<'pr>) {
        let value = self.value(node.arguments());
        self.breaks.push(value);
    }

    fn visit_redo_node(&mut self, _: &ruby_prism::RedoNode<'pr>) {
        self.unreadable = true;
    }

    fn visit_block_node(&mut self, _: &BlockNode<'pr>) {}

    fn visit_lambda_node(&mut self, _: &ruby_prism::LambdaNode<'pr>) {}

    fn visit_def_node(&mut self, _: &DefNode<'pr>) {}

    fn visit_while_node(&mut self, _: &ruby_prism::WhileNode<'pr>) {}

    fn visit_until_node(&mut self, _: &ruby_prism::UntilNode<'pr>) {}

    fn visit_for_node(&mut self, _: &ruby_prism::ForNode<'pr>) {}
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
/// A cursor-less walk, like [`shapes`] and [`instance_variable`], and the one that reports rather
/// than resolves: `u32::MAX` again means the whole file is collected.
///
/// - **`within` is not an afterwards filter.** Classifying a shape walks a chain and every
///   assignment above it, and the caller asks about the range on screen. Applying the range here
///   stops classification from running at all; in the caller it would only discard answers.
///   Collecting candidates is one parse either way.
/// - **It tests overlap, not containment, and an empty range is legal.** A name half scrolled off
///   the top is still a binding the visible half wants labelled, and a caller asking about one
///   label passes both ends equal.
/// - **An unshapeable binding is dropped**, not reported as [`Receiver::Unknown`]: there is no
///   label to draw for it.
#[must_use]
pub fn bindings_in(source: &str, within: (u32, u32)) -> Vec<Bound> {
    let result = parse(source);
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
///   and reaches the parameter's own value through [`Variables`].
/// - **`u32::MAX` means settled text.** `typeDefinition` is not one of the three requests answered
///   between a keystroke and the index, so there is no half-typed call to blank.
///   [`instance_variable`] and [`returns_in`] do the same.
#[must_use]
pub fn type_of(text: &Parsed<'_>, offset: u32) -> Option<(u32, u32, Receiver)> {
    let (source, result) = (text.source(), text.result());
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

/// Which kind of name a cursor on a local is on ([`local_at`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Local {
    /// A name being bound: a local assigned, or a block's parameter.
    Binding,
    /// A read of a local, a block's parameter or a method's parameter.
    Read,
    /// A method's own parameter, in its `def`'s header.
    Parameter,
}

/// The local, block parameter or method parameter under the cursor, and the shape that types it:
/// for a card about the name itself. Calls, constants and instance variables are other cards'.
#[must_use]
pub fn local_at(text: &Parsed<'_>, offset: u32) -> Option<(u32, u32, Receiver, Local)> {
    let (source, result) = (text.source(), text.result());
    let mut finder = Finder::new(source, u32::MAX);
    finder.visit(&result.node());
    if let Some(bound) = finder
        .bindings((offset, offset))
        .into_iter()
        .find(|bound| bound.name.0 <= offset && offset <= bound.name.1)
    {
        return Some((bound.name.0, bound.name.1, bound.was, Local::Binding));
    }
    // A parameter in its `def`'s header: only those a slot can name ([`Def::parameters`]).
    let named = |at: u32| {
        let end = source.as_bytes()[at as usize..]
            .iter()
            .position(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_' || *byte >= 0x80))
            .map_or(source.len(), |length| at as usize + length);
        u32::try_from(end).unwrap_or(u32::MAX)
    };
    for def in finder.defs.iter().rev() {
        if let Some(parameter) = def
            .parameters
            .iter()
            .find(|parameter| parameter.at <= offset && offset <= named(parameter.at))
        {
            return Some((
                parameter.at,
                named(parameter.at),
                Finder::parameter_shape(def, parameter),
                Local::Parameter,
            ));
        }
    }
    let mut pointed = Pointed {
        offset,
        found: None,
    };
    pointed.visit(&result.node());
    let (start, end, node) = pointed.found?;
    if node.as_local_variable_read_node().is_none()
        && node.as_it_local_variable_read_node().is_none()
    {
        return None;
    }
    let receiver = finder.receiver_of(Some(&node), Budget::default());
    (!matches!(receiver, Receiver::Unknown)).then_some((start, end, receiver, Local::Read))
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

/// A call rubydex files away from its name, or not at all, with the cursor on that name.
///
/// The two calls rubydex misfiles, and only those: `locator` asks this where the graph
/// holds nothing at the cursor, and a call found anywhere else is rubydex's to place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Misplaced {
    /// An operator write through a call: `a.b ||= c` and `a.b &&= c` file the reference on
    /// the operator, and `a.b += c` (and the other ten operators) on the `.`, never on `b`.
    Parked {
        message: (u32, u32),
        /// Where rubydex filed it: the `||=`, or the `.` (`&.`, `::`) before `b`.
        operator: (u32, u32),
        /// What the call reads: `b`, never the `b=` it writes.
        name: String,
    },
    /// Any call inside a constant path's parent, `a.b::C` : rubydex files the constant and
    /// never visits the parent, so the call has no reference anywhere.
    Unrecorded { message: (u32, u32), name: String },
}

/// Which [`Misplaced`] shape the cursor is on the name of, if any.
#[must_use]
pub fn misplaced(text: &Parsed<'_>, offset: u32) -> Option<Misplaced> {
    let result = text.result();
    let mut walk = Misplacing {
        offset,
        unrecorded: false,
        found: None,
    };
    walk.visit(&result.node());
    walk.found
}

/// The walk [`misplaced`] runs. The innermost shape around the cursor stands, as in [`Pointed`].
struct Misplacing {
    offset: u32,
    /// Inside a constant path's parent, where rubydex visits nothing.
    unrecorded: bool,
    found: Option<Misplaced>,
}

impl Misplacing {
    /// Both ends included, for [`Pointed::holds`]' reason.
    fn holds(&self, location: &Location<'_>) -> bool {
        location.start_offset() as u32 <= self.offset && self.offset <= location.end_offset() as u32
    }

    fn parked(
        &mut self,
        message: Option<Location<'_>>,
        operator: Option<Location<'_>>,
        name: &[u8],
    ) {
        if let Some(message) = message.filter(|message| self.holds(message))
            && let Some(operator) = operator
        {
            self.found = Some(Misplaced::Parked {
                message: (message.start_offset() as u32, message.end_offset() as u32),
                operator: (operator.start_offset() as u32, operator.end_offset() as u32),
                name: String::from_utf8_lossy(name).into_owned(),
            });
        }
    }
}

impl<'pr> Visit<'pr> for Misplacing {
    fn visit_call_or_write_node(&mut self, node: &CallOrWriteNode<'pr>) {
        self.parked(
            node.message_loc(),
            Some(node.operator_loc()),
            node.read_name().as_slice(),
        );
        ruby_prism::visit_call_or_write_node(self, node);
    }

    fn visit_call_and_write_node(&mut self, node: &CallAndWriteNode<'pr>) {
        self.parked(
            node.message_loc(),
            Some(node.operator_loc()),
            node.read_name().as_slice(),
        );
        ruby_prism::visit_call_and_write_node(self, node);
    }

    fn visit_call_operator_write_node(&mut self, node: &CallOperatorWriteNode<'pr>) {
        self.parked(
            node.message_loc(),
            node.call_operator_loc(),
            node.read_name().as_slice(),
        );
        ruby_prism::visit_call_operator_write_node(self, node);
    }

    /// The parent only: the name after `::` is a constant, which rubydex does file.
    fn visit_constant_path_node(&mut self, node: &ConstantPathNode<'pr>) {
        if let Some(parent) = node.parent() {
            let outside = std::mem::replace(&mut self.unrecorded, true);
            self.visit(&parent);
            self.unrecorded = outside;
        }
    }

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        if self.unrecorded
            && let Some(message) = node.message_loc().filter(|message| self.holds(message))
        {
            self.found = Some(Misplaced::Unrecorded {
                message: (message.start_offset() as u32, message.end_offset() as u32),
                name: String::from_utf8_lossy(node.name().as_slice()).into_owned(),
            });
        }
        ruby_prism::visit_call_node(self, node);
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

/// One text and Prism's parse of it, made the first time a question needs the tree.
///
/// A request asks several questions of the buffer under the cursor: what shape the cursor is in
/// ([`at`]), what the call there returns ([`type_of`]), whether rubydex misfiled it
/// ([`misplaced`]), whether a symbol there names a method or a variable ([`named_symbol`],
/// [`macro_symbol`], [`macro_variable`], [`keyed_literal`]), and the scope walk's
/// ([`scopes::variable`]). Each is a walk of the same tree, so a request makes one of these and
/// hands it to each. Parsed per question, `hover` parsed its buffer up to six times, a third of its
/// time on a small app (2026-09-29). A caller with one question passes `&Parsed::new(source)`.
pub struct Parsed<'s> {
    source: &'s str,
    result: OnceCell<ParseResult<'s>>,
}

impl<'s> Parsed<'s> {
    /// `source`, not parsed yet.
    #[must_use]
    pub fn new(source: &'s str) -> Self {
        Self {
            source,
            result: OnceCell::new(),
        }
    }

    /// The text every offset asked about is measured in.
    #[must_use]
    pub fn source(&self) -> &'s str {
        self.source
    }

    /// Prism's parse of the text, done the first time it is asked for.
    pub(super) fn result(&self) -> &ParseResult<'s> {
        self.result.get_or_init(|| parse(self.source))
    }
}

/// Prism's parse of one text: every parse on the type side goes through here, so a test can count
/// them ([`parses_taken`]). A parse is the one cost a request should pay once per text.
pub(super) fn parse(source: &str) -> ParseResult<'_> {
    #[cfg(test)]
    PARSES.set(PARSES.get() + 1);
    ruby_prism::parse(source.as_bytes())
}

// How many texts were parsed through [`parse`] since a test last asked ([`parses_taken`]), on the
// requesting thread: `CLASSIFIED`'s reason for a counter, and for `thread_local`.
#[cfg(test)]
thread_local! {
    static PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many texts were parsed since the last call, and resets it.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn parses_taken() -> usize {
    PARSES.replace(0)
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
pub fn macro_symbol(text: &Parsed<'_>, offset: u32) -> Option<MacroSymbol> {
    let result = text.result();
    let mut finder = MacroSymbols {
        offset,
        body: false,
        variables: false,
        found: None,
    };
    finder.visit(&result.node());
    finder.found
}

/// A macro's `:@name` argument `offset` is on, if any: an instance variable's name handed to a
/// receiverless call straight in a class or module body.
///
/// `delegate :render, to: :@template` and Forwardable's `def_delegators :@items, :size` make
/// methods that read the variable on the objects the class makes. [`macro_symbol`]'s rule with two
/// differences: a keyword's value counts, since an instance variable's name configures nothing
/// else, and only a name spelled as one (`@x`, not `@@x`). The calls that read or write a
/// variable reflectively are left to what they reach: a receiverless `instance_variable_get` in a
/// class body reads the class object's.
#[must_use]
pub fn macro_variable(text: &Parsed<'_>, offset: u32) -> Option<MacroSymbol> {
    let result = text.result();
    let mut finder = MacroSymbols {
        offset,
        body: false,
        variables: true,
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
    /// [`macro_variable`]'s question rather than [`macro_symbol`]'s.
    variables: bool,
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
        if self.variables {
            return self.variable_at(node);
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

    /// [`macro_variable`]'s half of [`Self::argument_at`]: a positional argument or a keyword's
    /// value.
    fn variable_at(&self, node: &CallNode<'_>) -> Option<MacroSymbol> {
        let called = node.name();
        if REFLECTIVE_CALLS.contains(&called.as_slice()) {
            return None;
        }
        let symbols = node.arguments()?.arguments().iter().flat_map(|argument| {
            match argument.as_keyword_hash_node() {
                Some(hash) => hash
                    .elements()
                    .iter()
                    .filter_map(|element| element.as_assoc_node()?.value().as_symbol_node())
                    .collect(),
                None => argument.as_symbol_node().into_iter().collect::<Vec<_>>(),
            }
        });
        for symbol in symbols {
            let literal = symbol.location();
            if self.offset < literal.start_offset() as u32
                || self.offset > literal.end_offset() as u32
            {
                continue;
            }
            let name = String::from_utf8_lossy(symbol.unescaped()).into_owned();
            if !name.starts_with('@') || name.starts_with("@@") || name.len() == 1 {
                return None;
            }
            let value = symbol.value_loc()?;
            return Some(MacroSymbol {
                name,
                macro_name: String::from_utf8_lossy(called.as_slice()).into_owned(),
                start: value.start_offset() as u32,
                end: value.end_offset() as u32,
            });
        }
        None
    }
}

/// Ruby's calls that read or write an instance variable by the name their first argument gives:
/// what they reach decides whose variable it is, not the class body they are written in.
const REFLECTIVE_CALLS: [&[u8]; 4] = [
    b"instance_variable_get",
    b"instance_variable_set",
    b"instance_variable_defined?",
    b"remove_instance_variable",
];

/// A symbol literal written as the first argument of a call, and where that call's name is.
///
/// `widget.send(:shout)`, `method(:shout)`, `try(:title)`, `instance_method(:shout)`: a method's
/// name, handed to a member that looks it up. Which members do that is a fact about the member the
/// call reaches, so this learns no vocabulary, as [`macro_symbol`] learns none:
/// [`types`](super::types) decides from the declaration the call found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedSymbol {
    /// The name, colon excluded: `shout`.
    pub name: String,
    /// The span of the name, colon excluded: what the editor underlines.
    pub start: u32,
    pub end: u32,
    /// Where the call's own name starts (`send`), which is what the graph files the call under.
    pub message: u32,
}

/// The first-argument symbol `offset` is on, if any ([`NamedSymbol`]).
///
/// - **The first argument only**, and a symbol literal only: a name only running Ruby knows
///   (`send(name)`, `:"#{x}_count"`) has nothing to look up.
/// - **Anywhere**: in a `def`, on a receiver, in a block. What the symbol names is found on the
///   object the call is sent to, which the call itself is resolved to find.
#[must_use]
pub fn named_symbol(text: &Parsed<'_>, offset: u32) -> Option<NamedSymbol> {
    let result = text.result();
    let mut finder = NamedSymbols {
        offset,
        found: None,
    };
    finder.visit(&result.node());
    finder.found
}

/// The walk [`named_symbol`] runs.
struct NamedSymbols {
    offset: u32,
    found: Option<NamedSymbol>,
}

impl<'pr> Visit<'pr> for NamedSymbols {
    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        if self.found.is_none() {
            self.found = first_symbol_at(node, self.offset);
        }
        if self.found.is_none() {
            ruby_prism::visit_call_node(self, node);
        }
    }
}

/// `node`'s first argument, where it is a symbol literal covering `offset`: colon included in the
/// hit test, as [`MacroSymbols::argument_at`] counts it.
fn first_symbol_at(node: &CallNode<'_>, offset: u32) -> Option<NamedSymbol> {
    let symbol = node
        .arguments()?
        .arguments()
        .iter()
        .next()?
        .as_symbol_node()?;
    let literal = symbol.location();
    if offset < literal.start_offset() as u32 || offset > literal.end_offset() as u32 {
        return None;
    }
    let value = symbol.value_loc()?;
    Some(NamedSymbol {
        name: String::from_utf8_lossy(symbol.unescaped()).into_owned(),
        start: value.start_offset() as u32,
        end: value.end_offset() as u32,
        message: node.message_loc()?.start_offset() as u32,
    })
}

/// A literal key a call passes first ([`keyed_literal`]): the key, where its text is, where the
/// call's name is, and the keywords the call writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyedLiteral {
    /// A string's text or a symbol's name.
    pub key: String,
    /// The span of that text, quotes and colon excluded: what the editor underlines.
    pub start: u32,
    pub end: u32,
    /// Where the call's own name starts, which is what the graph files the call under.
    pub message: u32,
    /// Each keyword the call writes, and its value where that is a literal.
    pub keywords: Vec<(String, crate::knowledge::Written)>,
}

/// The first-argument string or symbol `offset` is on, if any ([`KeyedLiteral`]): `"users.title"`
/// in `t("users.title", count: 2)`. An interpolated string names what only running Ruby knows, and
/// is not one.
#[must_use]
pub fn keyed_literal(text: &Parsed<'_>, offset: u32) -> Option<KeyedLiteral> {
    struct Found {
        offset: u32,
        found: Option<KeyedLiteral>,
    }
    impl<'pr> Visit<'pr> for Found {
        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            if self.found.is_none() {
                self.found = keyed_at(node, self.offset);
            }
            if self.found.is_none() {
                ruby_prism::visit_call_node(self, node);
            }
        }
    }
    let result = text.result();
    let mut finder = Found {
        offset,
        found: None,
    };
    finder.visit(&result.node());
    finder.found
}

/// `node`'s first argument, where it is a string or symbol literal covering `offset`.
fn keyed_at(node: &CallNode<'_>, offset: u32) -> Option<KeyedLiteral> {
    let arguments = node.arguments()?;
    let first = arguments.arguments().iter().next()?;
    let literal = first.location();
    if offset < literal.start_offset() as u32 || offset > literal.end_offset() as u32 {
        return None;
    }
    let (key, content) = if let Some(string) = first.as_string_node() {
        (string.unescaped().to_vec(), string.content_loc())
    } else {
        let symbol = first.as_symbol_node()?;
        (symbol.unescaped().to_vec(), symbol.value_loc()?)
    };
    let keywords = arguments
        .arguments()
        .iter()
        .filter_map(|argument| argument.as_keyword_hash_node())
        .flat_map(|hash| hash.elements().iter().collect::<Vec<_>>())
        .filter_map(|element| {
            let pair = element.as_assoc_node()?;
            let name = pair.key().as_symbol_node()?;
            let value = pair.value();
            let written = if let Some(string) = value.as_string_node() {
                crate::knowledge::Written::Text(
                    String::from_utf8_lossy(string.unescaped()).into_owned(),
                )
            } else if let Some(symbol) = value.as_symbol_node() {
                crate::knowledge::Written::Symbol(
                    String::from_utf8_lossy(symbol.unescaped()).into_owned(),
                )
            } else {
                crate::knowledge::Written::Other
            };
            Some((
                String::from_utf8_lossy(name.unescaped()).into_owned(),
                written,
            ))
        })
        .collect();
    Some(KeyedLiteral {
        key: String::from_utf8(key).ok()?,
        start: content.start_offset() as u32,
        end: content.end_offset() as u32,
        message: node.message_loc()?.start_offset() as u32,
        keywords,
    })
}

/// Every symbol written as a call's first argument, with the call's name and the span of the
/// symbol's: where a text may hand a method's name to `send`, `try` and their kin, which no call
/// reference records (`references::find`). Which calls take a name is the caller's question.
#[must_use]
pub fn symbols_handed(source: &str) -> Vec<(String, String, u32, u32)> {
    let result = parse(source);
    let mut finder = HandedSymbols { found: Vec::new() };
    finder.visit(&result.node());
    finder.found
}

/// The walk [`symbols_handed`] runs.
struct HandedSymbols {
    found: Vec<(String, String, u32, u32)>,
}

impl<'pr> Visit<'pr> for HandedSymbols {
    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        if let Some(symbol) = node
            .arguments()
            .and_then(|arguments| arguments.arguments().iter().next())
            .and_then(|first| first.as_symbol_node())
            && let Some(value) = symbol.value_loc()
        {
            self.found.push((
                String::from_utf8_lossy(node.name().as_slice()).into_owned(),
                String::from_utf8_lossy(symbol.unescaped()).into_owned(),
                value.start_offset() as u32,
                value.end_offset() as u32,
            ));
        }
        ruby_prism::visit_call_node(self, node);
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
    instance_writes: Vec<VariableWrite<'pr>>,
    /// Every write to a local, of every kind this walk reads: `=`, a multiple assignment's
    /// targets, `+=` and the rest, `||=` and `&&=`. Keyed like [`Self::instance_writes`].
    ///
    /// **All of them, wherever the cursor is**, unlike [`Self::locals`]: this answers which writes
    /// *reach* a read, and a write below a read in a loop reaches it on the next turn.
    local_writes: Vec<VariableWrite<'pr>>,
    /// Every construct that may not run, or may run again: branches, loops and blocks.
    regions: Vec<Region>,
    /// Every check on a local, with where it holds.
    checks: Vec<Check>,
    /// What each parameter this walk can type binds, by the start of its name.
    declared: HashMap<u32, Declared>,
    /// The instance-variable writes a `def initialize` makes as statements of its own body, by the
    /// start of the name: the writes every instance has run before any other method can.
    initialized: std::collections::HashSet<u32>,
    /// Every receiverless `def initialize`, by where it starts.
    initializers: HashMap<u32, Initializer>,
    /// Every other receiverless `def`, by where it starts: the instance variables it writes as
    /// statements of its own body before any that may `return`.
    openings: HashMap<u32, Vec<String>>,
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
    defs: Vec<Def>,
    /// The half-typed operator the cursor is completing after, as a span to blank out.
    ///
    /// See [`Finder::without_the_half_typed_call`]. Recorded here because its call node is gone by
    /// the time the question is asked.
    repair: Option<(u32, u32)>,
}

/// One write to a variable, and how it gives the variable its value.
///
/// Collected from the whole file, not just before the cursor: an instance variable is assigned in
/// `initialize` and read in every other method, and a local's write below a read in a loop reaches
/// that read on the next turn.
struct VariableWrite<'pr> {
    /// The span of the name, which [`scopes`] keys the occurrence by.
    name: (u32, u32),
    /// Where the write takes effect: the end of the value. A read at or after this sees it, which
    /// is what keeps `x = x.foo` from answering its own read.
    at: u32,
    assigning: Assigning<'pr>,
}

/// How a write gives its variable a value.
enum Assigning<'pr> {
    /// `x = value`, or one target of `a, b = value`: the value itself, or its `index`th element.
    Value {
        value: Node<'pr>,
        index: Option<u32>,
    },
    /// `x += value` and its siblings: the operator method, called on the old value.
    Operator { value: Node<'pr>, method: String },
    /// `x ||= value` (`and` false) and `x &&= value` (`and` true).
    Shortcut { value: Node<'pr>, and: bool },
    /// `rescue A, B => x`: the exception caught, by where each class's constant ends
    /// ([`Receiver::Rescued`]). `None` where a class is not a constant (`rescue *ERRORS => x`),
    /// which no reading of the text can name.
    Rescued { classes: Option<Vec<u32>> },
    /// `a, *rest = value`'s `rest`: an `Array` whatever `value` is (`a, *b = 1` makes `b` `[]`).
    /// The value is kept only for where the write happens.
    Splatted { value: Node<'pr> },
}

/// One construct that may not run, or may run again.
///
/// - **`branch`**: a write inside may not have run by the time control leaves it: an `if`'s arm,
///   the right side of `&&`, a `rescue`, a block.
/// - **`repeat`**: control may come back to its start, so a write below a read inside reaches that
///   read: a loop, a block.
/// - **`closure`**: a block or lambda. Its body may run *later*, after writes below it.
///
/// Spans are Prism nodes' spans, so two regions nest or are apart; [`Regions`] relies on that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Region {
    span: (u32, u32),
    branch: bool,
    repeat: bool,
    closure: bool,
}

impl Region {
    fn holds(&self, at: u32) -> bool {
        self.span.0 <= at && at < self.span.1
    }
}

/// What a parameter binds before any write, for the ones this walk can type.
///
/// Every other binding (a pattern's capture, a destructured parameter) is a write [`Variables`]
/// cannot read, and refuses. `rescue => e` is a write of its own ([`Assigning::Rescued`]).
enum Declared {
    /// A `def`'s parameter, by the index of the `def` in [`Finder::defs`] and of the parameter in
    /// its list.
    Parameter { def: usize, index: usize },
    /// A block's positional parameter, by its index in [`Finder::yielded`].
    Yielded(usize),
    /// A block-local, `|x; y|`'s `y`: `nil` until written.
    Nil,
    /// A `*rest`, `**rest` or `&block` parameter of a `def`, a block or a lambda: a class Ruby
    /// fixes, whatever the caller passed ([`containers`]).
    Container(Receiver),
    /// A positional parameter of a proc or lambda literal ([`Receiver::ProcParameter`]).
    ProcParameter { at: u32, index: usize },
}

/// What each named `*rest`, `**rest` and `&block` parameter holds, keyed where its name starts, as
/// [`scopes`] keys the parameter.
///
/// - **Ruby's rule, not the caller's.** `*rest` is an `Array` and `**rest` a `Hash` whatever was
///   passed, even nothing. `&block` is a `Proc`, or `nil` where no block was given: Ruby converts
///   what `&` passes with `to_proc` and raises unless that is a `Proc`.
/// - **No element type.** A signature's `*String` would say more, and nothing reads it here; an
///   `Array` with nothing known inside is still the true class.
/// - **Anonymous ones are left out** (`def f(*)`, `**`, `&`, `...`): they bind no name to read.
fn containers(parameters: &ruby_prism::ParametersNode<'_>) -> Vec<(u32, Receiver)> {
    let mut out = Vec::new();
    if let Some(at) = parameters
        .rest()
        .and_then(|rest| rest.as_rest_parameter_node())
        .and_then(|rest| rest.name_loc())
    {
        out.push((at.start_offset() as u32, Receiver::literal("Array")));
    }
    if let Some(at) = parameters
        .keyword_rest()
        .and_then(|rest| rest.as_keyword_rest_parameter_node())
        .and_then(|rest| rest.name_loc())
    {
        out.push((at.start_offset() as u32, Receiver::literal("Hash")));
    }
    if let Some(at) = parameters.block().and_then(|block| block.name_loc()) {
        out.push((
            at.start_offset() as u32,
            Receiver::Either(vec![
                Receiver::literal("Proc"),
                Receiver::literal("NilClass"),
            ]),
        ));
    }
    out
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

/// The parameters a `def` binds, as the ones a position or keyword can name.
///
/// **What is left out is the care here.** `*rest`, `**rest` and `&block` have container types, not
/// what the caller wrote. A positional *after* a rest has no position countable from the left (as
/// [`Receiver::Destructured`] refuses a splat on an assignment's left). A destructured positional,
/// `def f((a, b))`, binds names this does not track, but still **holds its place**, so the index
/// counts past it.
fn bound_by(node: &DefNode<'_>) -> Vec<DefParameter> {
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
        let at = match (
            parameter.as_required_parameter_node(),
            parameter.as_optional_parameter_node(),
        ) {
            (Some(required), _) => required.location(),
            (_, Some(optional)) => optional.name_loc(),
            // `def f((a, b))`: a destructure, whose names are bound one level down.
            _ => continue,
        };
        out.push(DefParameter {
            at: at.start_offset() as u32,
            slot: ParameterSlot::Positional(index),
        });
    }
    // `posts()` are the positionals after a `*rest`, deliberately not walked.
    for parameter in written.keywords().iter() {
        let (name, at) = if let Some(required) = parameter.as_required_keyword_parameter_node() {
            (required.name(), required.name_loc())
        } else if let Some(optional) = parameter.as_optional_keyword_parameter_node() {
            (optional.name(), optional.name_loc())
        } else {
            continue;
        };
        out.push(DefParameter {
            at: at.start_offset() as u32,
            slot: ParameterSlot::Keyword(String::from_utf8_lossy(name.as_slice()).into_owned()),
        });
    }
    out
}

/// One `def`, as the three things a read inside it may ask about.
struct Def {
    span: (u32, u32),
    name: String,
    /// The parameters this `def` binds, in countable order.
    ///
    /// Only those that can have a slot: `*rest`, `**rest`, `&block` and every positional after a
    /// rest are left out, not numbered. See [`ParameterSlot`].
    parameters: Vec<DefParameter>,
    /// The name of its `&block` parameter, if it names one ([`Finder::yield_of`]).
    block: Option<String>,
}

/// One parameter of a `def`: where its name is, and where it sits.
struct DefParameter {
    /// Where the name starts, which is where [`scopes`] keys the parameter's occurrence.
    at: u32,
    slot: ParameterSlot,
}

/// One parameter of one block, and the call the block was written on.
struct BlockParameter<'pr> {
    name: (u32, u32),
    /// Which positional parameter it is: the index RBS lists a block's own parameters by.
    index: usize,
    /// The call, unclassified, for [`LocalWrite::value`]'s reason: classifying it is
    /// `receiver_of`'s job, which cannot run during the collecting walk.
    call: Node<'pr>,
    /// Whether Ruby unpacks one `Array` handed to this block: more than one positional parameter,
    /// or one beside a `*rest` or a trailing comma (`|a,|`). A lone `*rest` does not.
    spreads: bool,
    /// An optional parameter's default value, unclassified.
    default: Option<Node<'pr>>,
}

/// One `x = <something>`, kept as a span, not a name, so matching allocates nothing.
struct LocalWrite<'pr> {
    name: (u32, u32),
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
            // Innermost wins here too, so a call with nothing to repair clears an outer one's.
            self.repair = self
                .took_a_later_token(operator.end_offset() as u32, &message)
                .then_some((operator.start_offset() as u32, operator.end_offset() as u32));
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
            && proc_literal(&node.as_node()).is_some()
        {
            // `lambda { |x| }` and `proc { |x| }` are literals: their parameters are what a call of
            // them passes, not what `Kernel#lambda` yields.
            self.declare_containers(&parameters);
            self.declare_proc_parameters(node.location().start_offset() as u32, &parameters);
        } else if let Some(block) = node.block().and_then(|block| block.as_block_node())
            && let Some(parameters) = block
                .parameters()
                .and_then(|it| it.as_block_parameters_node())
                .and_then(|it| it.parameters())
        {
            let positionals = parameters.requireds().iter().count()
                + parameters.optionals().iter().count()
                + parameters.posts().iter().count();
            let spreads = positionals > 1 || (positionals == 1 && parameters.rest().is_some());
            for (index, parameter) in parameters.requireds().iter().enumerate() {
                let Some(required) = parameter.as_required_parameter_node() else {
                    continue;
                };
                let name = required.location();
                self.yielded.push(BlockParameter {
                    name: (name.start_offset() as u32, name.end_offset() as u32),
                    index,
                    call: node.as_node(),
                    spreads,
                    default: None,
                });
                self.declared.insert(
                    name.start_offset() as u32,
                    Declared::Yielded(self.yielded.len() - 1),
                );
            }
            // Optional ones count on from the required ones, where no required one follows them:
            // Ruby fills required parameters first, so a trailing one moves every position.
            let leading = parameters.requireds().iter().count();
            if parameters.posts().iter().count() == 0 {
                for (offset, parameter) in parameters.optionals().iter().enumerate() {
                    let Some(optional) = parameter.as_optional_parameter_node() else {
                        continue;
                    };
                    let name = optional.name_loc();
                    self.yielded.push(BlockParameter {
                        name: (name.start_offset() as u32, name.end_offset() as u32),
                        index: leading + offset,
                        call: node.as_node(),
                        spreads,
                        default: Some(optional.value()),
                    });
                    self.declared.insert(
                        name.start_offset() as u32,
                        Declared::Yielded(self.yielded.len() - 1),
                    );
                }
            }
            self.declare_containers(&parameters);
        }

        // `a&.b(x = 1)` skips its arguments and block where `a` is `nil`.
        if node.is_safe_navigation()
            && let Some(message) = node.message_loc()
        {
            self.branch(Some((
                message.start_offset() as u32,
                node.location().end_offset() as u32,
            )));
        }

        ruby_prism::visit_call_node(self, node);
    }

    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        // An index, not a search (see [`Finder::defs`]). Every other walk in this impl looks for
        // one node; this one builds a table, so nothing stops it.
        let parameters = bound_by(node);
        for (index, parameter) in parameters.iter().enumerate() {
            self.declared.insert(
                parameter.at,
                Declared::Parameter {
                    def: self.defs.len(),
                    index,
                },
            );
        }
        if let Some(written) = node.parameters() {
            self.declare_containers(&written);
        }
        if node.receiver().is_none() {
            let initialize = node.name().as_slice() == b"initialize";
            self.note_initialized(
                node.location().start_offset() as u32,
                node.body(),
                initialize,
            );
        }
        self.defs.push(Def {
            span: (
                node.location().start_offset() as u32,
                node.location().end_offset() as u32,
            ),
            name: String::from_utf8_lossy(node.name().as_slice()).into_owned(),
            parameters,
            block: node
                .parameters()
                .and_then(|written| written.block())
                .and_then(|block| block.name())
                .map(|name| String::from_utf8_lossy(name.as_slice()).into_owned()),
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
        let written = self.note_write(
            &node.name_loc(),
            Assigning::Value {
                value: node.value(),
                index: None,
            },
        );
        if written.at <= self.offset {
            self.locals.push(LocalWrite {
                name: written.name,
                value: node.value(),
                index: None,
            });
        }
        self.local_writes.push(written);
        ruby_prism::visit_local_variable_write_node(self, node);
    }

    fn visit_local_variable_operator_write_node(
        &mut self,
        node: &ruby_prism::LocalVariableOperatorWriteNode<'pr>,
    ) {
        let written = self.note_write(
            &node.name_loc(),
            Assigning::Operator {
                value: node.value(),
                method: String::from_utf8_lossy(node.binary_operator().as_slice()).into_owned(),
            },
        );
        self.local_writes.push(written);
        ruby_prism::visit_local_variable_operator_write_node(self, node);
    }

    fn visit_local_variable_or_write_node(
        &mut self,
        node: &ruby_prism::LocalVariableOrWriteNode<'pr>,
    ) {
        self.branch(Some(span_of(&node.value())));
        let written = self.note_write(
            &node.name_loc(),
            Assigning::Shortcut {
                value: node.value(),
                and: false,
            },
        );
        self.local_writes.push(written);
        ruby_prism::visit_local_variable_or_write_node(self, node);
    }

    fn visit_local_variable_and_write_node(
        &mut self,
        node: &ruby_prism::LocalVariableAndWriteNode<'pr>,
    ) {
        self.branch(Some(span_of(&node.value())));
        let written = self.note_write(
            &node.name_loc(),
            Assigning::Shortcut {
                value: node.value(),
                and: true,
            },
        );
        self.local_writes.push(written);
        ruby_prism::visit_local_variable_and_write_node(self, node);
    }

    /// `a, b = value`: every target countable from the left.
    ///
    /// **Refused entirely when a `*rest` is written.** In `a, *b, c = value` only `a` is fixed at
    /// 0; `c`'s position depends on the value's length. Keeping the names before the rest would be
    /// correct but not worth it: splats in destructures are rare, and half an answer reads like a
    /// whole one.
    fn visit_multi_write_node(&mut self, node: &MultiWriteNode<'pr>) {
        // The `*rest` target itself is an `Array`, whatever the value is: the one name a splat
        // leaves countable.
        if let Some(target) = node
            .rest()
            .and_then(|rest| rest.as_splat_node())
            .and_then(|splat| splat.expression())
        {
            let splatted = || Assigning::Splatted {
                value: node.value(),
            };
            if let Some(local) = target.as_local_variable_target_node() {
                let written = self.note_write(&local.location(), splatted());
                self.local_writes.push(written);
            } else if let Some(instance) = target.as_instance_variable_target_node() {
                let written = self.note_write(&instance.location(), splatted());
                self.instance_writes.push(written);
            }
        }
        let at = node.value().location().end_offset() as u32;
        if node.rest().is_none() {
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
            let element = |index: usize| {
                written
                    .then(|| {
                        node.value()
                            .as_array_node()
                            .and_then(|array| array.elements().iter().nth(index))
                    })
                    .flatten()
            };
            for (index, target) in node.lefts().iter().enumerate() {
                let Ok(counted) = u32::try_from(index) else {
                    continue;
                };
                let assigning = || Assigning::Value {
                    index: element(index).is_none().then_some(counted),
                    value: element(index).unwrap_or_else(|| node.value()),
                };
                // An instance variable destructured the same way, which `@a, @b = pair` is.
                if let Some(target) = target.as_instance_variable_target_node() {
                    let name = span_of(&target.as_node());
                    self.instance_writes.push(VariableWrite {
                        name,
                        at,
                        assigning: assigning(),
                    });
                    continue;
                }
                let Some(target) = target.as_local_variable_target_node() else {
                    continue;
                };
                let name = span_of(&target.as_node());
                self.local_writes.push(VariableWrite {
                    name,
                    at,
                    assigning: assigning(),
                });
                if at <= self.offset {
                    self.locals.push(LocalWrite {
                        name,
                        index: element(index).is_none().then_some(counted),
                        value: element(index).unwrap_or_else(|| node.value()),
                    });
                }
            }
        }
        ruby_prism::visit_multi_write_node(self, node);
    }

    // `rescue A, B => e` binds `e` to the exception caught ([`Receiver::Rescued`]), for a local or
    // an instance variable alike.
    fn visit_rescue_node(&mut self, node: &ruby_prism::RescueNode<'pr>) {
        if let Some(reference) = node.reference() {
            let mut classes = Some(Vec::new());
            for exception in node.exceptions().iter() {
                match classes.as_mut() {
                    Some(held) if is_constant(&exception) => {
                        held.push(exception.location().end_offset() as u32);
                    }
                    _ => classes = None,
                }
            }
            if let Some(target) = reference.as_local_variable_target_node() {
                let written = self.note_write(&target.location(), Assigning::Rescued { classes });
                self.local_writes.push(written);
            } else if let Some(target) = reference.as_instance_variable_target_node() {
                let written = self.note_write(&target.location(), Assigning::Rescued { classes });
                self.instance_writes.push(written);
            }
        }
        ruby_prism::visit_rescue_node(self, node);
    }

    fn visit_instance_variable_write_node(&mut self, node: &InstanceVariableWriteNode<'pr>) {
        let written = self.note_write(
            &node.name_loc(),
            Assigning::Value {
                value: node.value(),
                index: None,
            },
        );
        self.instance_writes.push(written);
        ruby_prism::visit_instance_variable_write_node(self, node);
    }

    // `@cache ||= build` gives the variable `build` where it was falsy and leaves it alone
    // otherwise, so as a write it adds `build`: the old value is some other write's, or `nil`.
    fn visit_instance_variable_or_write_node(&mut self, node: &InstanceVariableOrWriteNode<'pr>) {
        self.branch(Some(span_of(&node.value())));
        let written = self.note_write(
            &node.name_loc(),
            Assigning::Shortcut {
                value: node.value(),
                and: false,
            },
        );
        self.instance_writes.push(written);
        ruby_prism::visit_instance_variable_or_write_node(self, node);
    }

    fn visit_instance_variable_and_write_node(&mut self, node: &InstanceVariableAndWriteNode<'pr>) {
        self.branch(Some(span_of(&node.value())));
        let written = self.note_write(
            &node.name_loc(),
            Assigning::Shortcut {
                value: node.value(),
                and: true,
            },
        );
        self.instance_writes.push(written);
        ruby_prism::visit_instance_variable_and_write_node(self, node);
    }

    // `@n += 1` is a write too: the old value's `+`, which is a question about every write,
    // including this one.
    fn visit_instance_variable_operator_write_node(
        &mut self,
        node: &ruby_prism::InstanceVariableOperatorWriteNode<'pr>,
    ) {
        let written = self.note_write(
            &node.name_loc(),
            Assigning::Operator {
                value: node.value(),
                method: String::from_utf8_lossy(node.binary_operator().as_slice()).into_owned(),
            },
        );
        self.instance_writes.push(written);
        ruby_prism::visit_instance_variable_operator_write_node(self, node);
    }

    // Parameters that bind before any write, and the constructs that decide what can reach what.

    fn visit_block_local_variable_node(&mut self, node: &ruby_prism::BlockLocalVariableNode<'pr>) {
        self.declared
            .insert(node.location().start_offset() as u32, Declared::Nil);
    }

    fn visit_if_node(&mut self, node: &ruby_prism::IfNode<'pr>) {
        let taken = node.statements().map(|found| span_of(&found.as_node()));
        let otherwise = node.subsequent().map(|found| span_of(&found));
        self.branch(taken);
        self.branch(otherwise);
        let construct = span_of(&node.as_node());
        self.check(taken, construct, &node.predicate(), true);
        self.check(otherwise, construct, &node.predicate(), false);
        ruby_prism::visit_if_node(self, node);
    }

    fn visit_unless_node(&mut self, node: &ruby_prism::UnlessNode<'pr>) {
        let taken = node.statements().map(|found| span_of(&found.as_node()));
        let otherwise = node.else_clause().map(|found| span_of(&found.as_node()));
        self.branch(taken);
        self.branch(otherwise);
        let construct = span_of(&node.as_node());
        self.check(taken, construct, &node.predicate(), false);
        self.check(otherwise, construct, &node.predicate(), true);
        ruby_prism::visit_unless_node(self, node);
    }

    // Every `when` and `in` is a branch, condition included: after the first, a condition may not
    // be reached.
    fn visit_case_node(&mut self, node: &ruby_prism::CaseNode<'pr>) {
        for condition in node.conditions().iter() {
            self.branch(Some(span_of(&condition)));
        }
        self.branch(node.else_clause().map(|found| span_of(&found.as_node())));
        self.check_case(node);
        ruby_prism::visit_case_node(self, node);
    }

    // A guard says something about every statement after it in its list.
    fn visit_statements_node(&mut self, node: &StatementsNode<'pr>) {
        let end = node.location().end_offset() as u32;
        for statement in node.body().iter() {
            let after = statement.location().end_offset() as u32;
            if after < end
                && let Some((predicate, facts)) = guard_facts(&statement)
            {
                self.checks.push(Check {
                    holds: (after, end),
                    construct: span_of(&statement),
                    predicate,
                    facts,
                });
            }
        }
        ruby_prism::visit_statements_node(self, node);
    }

    fn visit_case_match_node(&mut self, node: &ruby_prism::CaseMatchNode<'pr>) {
        for condition in node.conditions().iter() {
            self.branch(Some(span_of(&condition)));
        }
        self.branch(node.else_clause().map(|found| span_of(&found.as_node())));
        ruby_prism::visit_case_match_node(self, node);
    }

    // A loop repeats its condition and its body, and its body may not run at all, except in
    // `begin ... end while`, which runs it once first.
    fn visit_while_node(&mut self, node: &ruby_prism::WhileNode<'pr>) {
        self.repeat(span_of(&node.as_node()));
        if !node.is_begin_modifier() {
            self.branch(node.statements().map(|found| span_of(&found.as_node())));
        }
        ruby_prism::visit_while_node(self, node);
    }

    fn visit_until_node(&mut self, node: &ruby_prism::UntilNode<'pr>) {
        self.repeat(span_of(&node.as_node()));
        if !node.is_begin_modifier() {
            self.branch(node.statements().map(|found| span_of(&found.as_node())));
        }
        ruby_prism::visit_until_node(self, node);
    }

    // `for x in list` writes `x` and runs its body once per element, possibly none.
    fn visit_for_node(&mut self, node: &ruby_prism::ForNode<'pr>) {
        let span = (
            node.index().location().start_offset() as u32,
            node.location().end_offset() as u32,
        );
        self.branch(Some(span));
        self.repeat(span);
        ruby_prism::visit_for_node(self, node);
    }

    fn visit_and_node(&mut self, node: &ruby_prism::AndNode<'pr>) {
        self.branch(Some(span_of(&node.right())));
        let construct = span_of(&node.as_node());
        self.check(Some(span_of(&node.right())), construct, &node.left(), true);
        ruby_prism::visit_and_node(self, node);
    }

    fn visit_or_node(&mut self, node: &ruby_prism::OrNode<'pr>) {
        self.branch(Some(span_of(&node.right())));
        let construct = span_of(&node.as_node());
        self.check(Some(span_of(&node.right())), construct, &node.left(), false);
        ruby_prism::visit_or_node(self, node);
    }

    // `begin`'s statements may stop at any one of them, so a read after the `rescue` cannot count
    // on any of their writes; each `rescue` and the `else` run only sometimes. An `ensure` always
    // runs. A `retry` makes the whole construct a loop.
    fn visit_begin_node(&mut self, node: &ruby_prism::BeginNode<'pr>) {
        if let Some(rescued) = node.rescue_clause() {
            self.branch(node.statements().map(|found| span_of(&found.as_node())));
            let mut retries = Retries(false);
            retries.visit_rescue_node(&rescued);
            let mut clause = Some(rescued);
            while let Some(found) = clause {
                self.branch(Some(span_of(&found.as_node())));
                clause = found.subsequent();
            }
            self.branch(node.else_clause().map(|found| span_of(&found.as_node())));
            if retries.0 {
                self.repeat(span_of(&node.as_node()));
            }
        }
        ruby_prism::visit_begin_node(self, node);
    }

    fn visit_rescue_modifier_node(&mut self, node: &ruby_prism::RescueModifierNode<'pr>) {
        self.branch(Some(span_of(&node.expression())));
        self.branch(Some(span_of(&node.rescue_expression())));
        ruby_prism::visit_rescue_modifier_node(self, node);
    }

    fn visit_block_node(&mut self, node: &BlockNode<'pr>) {
        self.closure(span_of(&node.as_node()));
        ruby_prism::visit_block_node(self, node);
    }

    fn visit_lambda_node(&mut self, node: &LambdaNode<'pr>) {
        self.closure(span_of(&node.as_node()));
        if let Some(parameters) = node
            .parameters()
            .and_then(|it| it.as_block_parameters_node())
            .and_then(|it| it.parameters())
        {
            self.declare_containers(&parameters);
            self.declare_proc_parameters(node.location().start_offset() as u32, &parameters);
        }
        ruby_prism::visit_lambda_node(self, node);
    }

    // `defined?(x = 1)` never runs what it asks about.
    fn visit_defined_node(&mut self, node: &ruby_prism::DefinedNode<'pr>) {
        self.branch(Some(span_of(&node.value())));
        ruby_prism::visit_defined_node(self, node);
    }

    // The value of every other `||=` and `&&=` runs only sometimes.

    fn visit_class_variable_or_write_node(
        &mut self,
        node: &ruby_prism::ClassVariableOrWriteNode<'pr>,
    ) {
        self.branch(Some(span_of(&node.value())));
        ruby_prism::visit_class_variable_or_write_node(self, node);
    }

    fn visit_class_variable_and_write_node(
        &mut self,
        node: &ruby_prism::ClassVariableAndWriteNode<'pr>,
    ) {
        self.branch(Some(span_of(&node.value())));
        ruby_prism::visit_class_variable_and_write_node(self, node);
    }

    fn visit_global_variable_or_write_node(
        &mut self,
        node: &ruby_prism::GlobalVariableOrWriteNode<'pr>,
    ) {
        self.branch(Some(span_of(&node.value())));
        ruby_prism::visit_global_variable_or_write_node(self, node);
    }

    fn visit_global_variable_and_write_node(
        &mut self,
        node: &ruby_prism::GlobalVariableAndWriteNode<'pr>,
    ) {
        self.branch(Some(span_of(&node.value())));
        ruby_prism::visit_global_variable_and_write_node(self, node);
    }

    fn visit_constant_and_write_node(&mut self, node: &ruby_prism::ConstantAndWriteNode<'pr>) {
        self.branch(Some(span_of(&node.value())));
        ruby_prism::visit_constant_and_write_node(self, node);
    }

    fn visit_constant_path_and_write_node(
        &mut self,
        node: &ruby_prism::ConstantPathAndWriteNode<'pr>,
    ) {
        self.branch(Some(span_of(&node.value())));
        ruby_prism::visit_constant_path_and_write_node(self, node);
    }

    fn visit_call_or_write_node(&mut self, node: &ruby_prism::CallOrWriteNode<'pr>) {
        self.operator_written(
            node.call_operator_loc(),
            node.message_loc(),
            node.receiver(),
        );
        self.branch(Some(span_of(&node.value())));
        ruby_prism::visit_call_or_write_node(self, node);
    }

    fn visit_call_and_write_node(&mut self, node: &ruby_prism::CallAndWriteNode<'pr>) {
        self.operator_written(
            node.call_operator_loc(),
            node.message_loc(),
            node.receiver(),
        );
        self.branch(Some(span_of(&node.value())));
        ruby_prism::visit_call_and_write_node(self, node);
    }

    fn visit_call_operator_write_node(&mut self, node: &CallOperatorWriteNode<'pr>) {
        self.operator_written(
            node.call_operator_loc(),
            node.message_loc(),
            node.receiver(),
        );
        ruby_prism::visit_call_operator_write_node(self, node);
    }

    fn visit_index_or_write_node(&mut self, node: &ruby_prism::IndexOrWriteNode<'pr>) {
        self.branch(Some(span_of(&node.value())));
        ruby_prism::visit_index_or_write_node(self, node);
    }

    fn visit_index_and_write_node(&mut self, node: &ruby_prism::IndexAndWriteNode<'pr>) {
        self.branch(Some(span_of(&node.value())));
        ruby_prism::visit_index_and_write_node(self, node);
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
        self.branch(Some(span_of(&node.value())));
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
        self.branch(Some(span_of(&node.value())));
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
    /// `a.b ||= c`, `a.b &&= c` and `a.b += c` with the cursor on `b`: a call of `b` on `a`, as
    /// [`Visit::visit_call_node`] reads `a.b`. The same span test, and nothing to repair: an
    /// operator write is never half-typed at its message.
    fn operator_written(
        &mut self,
        operator: Option<Location<'pr>>,
        message: Option<Location<'pr>>,
        receiver: Option<Node<'pr>>,
    ) {
        if let (Some(operator), Some(message)) = (operator, message)
            && operator.end_offset() as u32 <= self.offset
            && self.offset <= message.end_offset() as u32
        {
            self.operator = Some(Pending::MethodCall(receiver));
        }
    }

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
            local_writes: Vec::new(),
            regions: Vec::new(),
            checks: Vec::new(),
            declared: HashMap::new(),
            initialized: std::collections::HashSet::new(),
            initializers: HashMap::new(),
            openings: HashMap::new(),
            constant_writes: Vec::new(),
            defs: Vec::new(),
            repair: None,
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
    fn enclosing_def(&self, at: u32) -> Option<&Def> {
        self.defs
            .iter()
            .rev()
            .find(|found| found.span.0 <= at && at < found.span.1)
    }

    /// What a `def`'s parameter holds before anything writes it, as a shape: what the `def`
    /// declares for it ([`Receiver::Parameter`]), which [`types`](super::types) reads.
    fn parameter_shape(def: &Def, parameter: &DefParameter) -> Receiver {
        Receiver::Parameter {
            at: def.span.0,
            method: def.name.clone(),
            slot: parameter.slot.clone(),
        }
    }

    /// Whether Prism took the message of the `.` being typed from a token written after it, the
    /// one shape [`Finder::without_the_half_typed_call`] repairs.
    ///
    /// - **A message written against the `.`** (`name.|size`, `name.si|ze`, `name.center|(1)`, or
    ///   none at all before a `)`) is the user's own. The line parses as written, and blanking
    ///   would leave `name size` or `name (1)`: a call to a method `name`, and no variable to read.
    /// - **A message past a line break** was taken from the line below (`name.|` above `end`,
    ///   `else` or the next statement). With the `.` blanked, the receiver ends its own line.
    /// - **On the cursor's own line, only a word that closes or continues the construct around
    ///   it** (`i.| end` in a one-line block). Any other word, with the `.` blanked, would become
    ///   the receiver's argument.
    fn took_a_later_token(&self, operator_end: u32, message: &ruby_prism::Location<'_>) -> bool {
        let start = message.start_offset() as u32;
        if start <= self.offset {
            return false;
        }
        let gap = self
            .source
            .as_bytes()
            .get(operator_end as usize..start as usize)
            .unwrap_or_default();
        gap.contains(&b'\n')
            || matches!(
                message.as_slice(),
                b"end" | b"else" | b"elsif" | b"when" | b"in" | b"rescue" | b"ensure" | b"then"
            )
    }

    /// The file with the half-typed call blanked out, byte for byte, where Prism read it wrong.
    ///
    /// Completion fires on invalid Ruby, and Prism recovers from a dangling `.` by reading whatever
    /// follows as the method name. For `@name.` above an `end`, that consumes the `end` and
    /// **reparents everything below it**: an `@name = "ada"` in an `initialize` written below lands
    /// inside the method being typed, and reads as a different variable.
    ///
    /// So the scope question is asked about the file without that operator. Only the operator is
    /// blanked, and only where [`Finder::took_a_later_token`] says so: a line that already
    /// writes its member parses as written. Its bytes become spaces, as
    /// [`signatures`](super::signatures) does, so every offset and line in the answer is the
    /// caller's.
    fn without_the_half_typed_call(&self) -> Cow<'s, str> {
        let Some((start, end)) = self.repair else {
            return Cow::Borrowed(self.source);
        };
        let mut bytes = self.source.as_bytes().to_vec();
        let Some(slice) = bytes.get_mut(start as usize..end as usize) else {
            return Cow::Borrowed(self.source);
        };
        // The operator alone (`.`, `&.`), which holds no newline to keep.
        slice.fill(b' ');
        // Whole characters were replaced by ASCII, so this holds. If not, the result is a scope
        // question about mangled text, never a panic.
        String::from_utf8(bytes).map_or(Cow::Borrowed(self.source), Cow::Owned)
    }

    /// One write, placed: its name, and where it takes effect.
    /// Files each leading required and optional positional of a proc literal's parameters as a
    /// [`Declared::ProcParameter`], counted as [`ProcParameters`] counts them.
    fn declare_proc_parameters(&mut self, at: u32, parameters: &ruby_prism::ParametersNode<'_>) {
        for (index, parameter) in parameters
            .requireds()
            .iter()
            .chain(parameters.optionals().iter())
            .enumerate()
        {
            let name = match (
                parameter.as_required_parameter_node(),
                parameter.as_optional_parameter_node(),
            ) {
                (Some(required), _) => required.location(),
                (_, Some(optional)) => optional.name_loc(),
                _ => continue,
            };
            self.declared.insert(
                name.start_offset() as u32,
                Declared::ProcParameter { at, index },
            );
        }
    }

    /// Files each named `*rest`, `**rest` and `&block` of one parameter list ([`containers`]).
    fn declare_containers(&mut self, parameters: &ruby_prism::ParametersNode<'_>) {
        for (at, shape) in containers(parameters) {
            self.declared.insert(at, Declared::Container(shape));
        }
    }

    fn note_write(&self, name: &Location<'_>, assigning: Assigning<'pr>) -> VariableWrite<'pr> {
        let value = match &assigning {
            Assigning::Value { value, .. }
            | Assigning::Operator { value, .. }
            | Assigning::Shortcut { value, .. }
            | Assigning::Splatted { value } => value,
            // No value node: the exception is bound as the `rescue` clause starts, where the name
            // ends.
            Assigning::Rescued { .. } => {
                return VariableWrite {
                    name: (name.start_offset() as u32, name.end_offset() as u32),
                    at: name.end_offset() as u32,
                    assigning,
                };
            }
        };
        VariableWrite {
            name: (name.start_offset() as u32, name.end_offset() as u32),
            at: value.location().end_offset() as u32,
            assigning,
        }
    }

    /// The instance variables a receiverless `def` writes as statements of its own body, before any
    /// statement that may `return` (`initialize`'s, and a controller callback's).
    ///
    /// Only the body's own statements: one in a branch or a block may not run. A `rescue` clause
    /// still counts its `begin`'s statements, because an exception out of the method stops what
    /// would read the variable. **A `return` stops the list**: `return if x` above `@y = 1` leaves
    /// an object without `@y` (an `initialize`'s), or runs the action with none (a callback's).
    ///
    /// Also whether one of those statements is `super`, which runs the `initialize` above. The
    /// write offsets count as initialized only for `initialize`.
    fn note_initialized(&mut self, def: u32, body: Option<Node<'pr>>, initialize: bool) {
        let statements = body.and_then(|body| {
            body.as_statements_node()
                .or_else(|| body.as_begin_node()?.statements())
        });
        let mut initializer = Initializer::default();
        for statement in statements.iter().flat_map(|found| found.body().iter()) {
            if may_return(&statement) {
                break;
            }
            initializer.supers |= matches!(
                statement,
                Node::SuperNode { .. } | Node::ForwardingSuperNode { .. }
            );
            let name = statement
                .as_instance_variable_write_node()
                .map(|write| write.name_loc())
                .or_else(|| {
                    statement
                        .as_instance_variable_or_write_node()
                        .map(|write| write.name_loc())
                });
            if let Some(name) = name {
                if initialize {
                    self.initialized.insert(name.start_offset() as u32);
                }
                initializer
                    .writes
                    .push(String::from_utf8_lossy(name.as_slice()).into_owned());
            }
        }
        if initialize {
            self.initializers.insert(def, initializer);
        } else {
            self.openings.insert(def, initializer.writes);
        }
    }

    /// A construct that may not run, or may stop part way. An empty one holds nothing.
    fn branch(&mut self, span: Option<(u32, u32)>) {
        if let Some(span) = span.filter(|span| span.0 < span.1) {
            self.regions.push(Region {
                span,
                branch: true,
                repeat: false,
                closure: false,
            });
        }
    }

    /// A construct control can come back to the start of.
    fn repeat(&mut self, span: (u32, u32)) {
        self.regions.push(Region {
            span,
            branch: false,
            repeat: true,
            closure: false,
        });
    }

    /// A block or a lambda: it may not run, may run again, and may run later.
    fn closure(&mut self, span: (u32, u32)) {
        self.regions.push(Region {
            span,
            branch: true,
            repeat: true,
            closure: true,
        });
    }

    /// The facts `predicate` makes about the locals it reads where it `holds`, over `span`
    ///.
    ///
    /// `construct` is the conditional the predicate belongs to.
    fn check(
        &mut self,
        span: Option<(u32, u32)>,
        construct: (u32, u32),
        predicate: &Node<'_>,
        holds: bool,
    ) {
        let Some(span) = span.filter(|span| span.0 < span.1) else {
            return;
        };
        let mut found = Vec::new();
        facts_of(predicate, holds, &mut found);
        if !found.is_empty() {
            self.checks.push(Check {
                holds: span,
                construct,
                predicate: span_of(predicate),
                facts: found.into_iter().map(|(checked, _)| checked).collect(),
            });
        }
    }

    /// `case x`: in a `when` whose every condition is a constant or `nil`, `x` is one of them; in
    /// the `else`, none of the constants and `nil` any `when` names. `K === x` is
    /// `x.is_a?(K)` for a class. A `when` with any other condition says nothing of its own.
    fn check_case(&mut self, node: &ruby_prism::CaseNode<'_>) {
        let Some(asked) = node.predicate() else {
            return;
        };
        let Some((local, at)) = cased(&asked) else {
            return;
        };
        let (construct, predicate) = (span_of(&node.as_node()), span_of(&asked));
        let (mut none_of, mut not_nil) = (Vec::new(), false);
        for condition in node.conditions().iter() {
            let Some(when) = condition.as_when_node() else {
                continue;
            };
            let (mut classes, mut nil, mut plain) = (Vec::new(), false, true);
            for tested in when.conditions().iter() {
                if is_constant(&tested) {
                    classes.push(tested.location().end_offset() as u32);
                } else if tested.as_nil_node().is_some() {
                    nil = true;
                } else {
                    plain = false;
                }
            }
            none_of.extend(classes.iter().copied());
            not_nil |= nil;
            if plain && let Some(statements) = when.statements() {
                self.checks.push(Check {
                    holds: span_of(&statements.as_node()),
                    construct,
                    predicate,
                    facts: vec![Checked {
                        local,
                        at,
                        fact: Fact::Is {
                            classes: classes.into(),
                            nil,
                        },
                    }],
                });
            }
        }
        if let Some(otherwise) = node.else_clause()
            && (!none_of.is_empty() || not_nil)
        {
            self.checks.push(Check {
                holds: span_of(&otherwise.as_node()),
                construct,
                predicate,
                facts: vec![Checked {
                    local,
                    at,
                    fact: Fact::IsNot {
                        classes: none_of.into(),
                        nil: not_nil,
                    },
                }],
            });
        }
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
        // `@x ||= begin … end`: a `begin` that rescues nothing is its last statement, as parentheses
        // are, and an empty one is `nil`. One that rescues is a conditional ([`Self::either`]).
        if let Some(last) = unrescued(node) {
            return match last {
                Some(last) => self.receiver_of(Some(&last), budget.linked()),
                None => Receiver::literal("NilClass"),
            };
        }
        // `x ||= v`, `x &&= v` and `x += v` combine the old value with the new one, so they are
        // asked before the rule below, which would answer the new one alone. What they hand back is
        // also held by `x` ([`Receiver::Stored`]).
        if let Some(rewritten) = self.rewritten(node, budget) {
            return Receiver::Stored(Box::new(rewritten));
        }
        // An assignment **is** its value, by Ruby's rule (see [`assigned_value`]), now also held by
        // what it was written to ([`Receiver::Stored`]). A link, not a spread: one question, asked
        // once.
        if let Some(value) = assigned_value(node) {
            return Receiver::Stored(Box::new(self.receiver_of(Some(&value), budget.linked())));
        }
        // `obj&.x = v` is the argument where the receiver is not `nil`, and `nil` where it is: a
        // union, which [`attribute_written`] leaves to this arm. Answering it here also
        // keeps the arms below from reading the message as a call to the **getter**.
        if is_safe_attribute_write(node) {
            let value = node
                .as_call_node()
                .and_then(|call| call.arguments())
                .and_then(|written| written.arguments().iter().last());
            return Receiver::Stored(Box::new(Receiver::Either(vec![
                self.receiver_of(value.as_ref(), budget.spread()),
                Receiver::literal("NilClass"),
            ])));
        }
        // `defined?(x)` names what `x` is, or is `nil`: a keyword, so no method can answer
        // otherwise. `$1` and `$&` are a part of the last match, or `nil` where there is none:
        // read off `$~`, which Ruby lets hold only a `MatchData` or `nil`.
        if node.as_defined_node().is_some()
            || node.as_numbered_reference_read_node().is_some()
            || node.as_back_reference_read_node().is_some()
        {
            return Receiver::Either(vec![
                Receiver::literal("String"),
                Receiver::literal("NilClass"),
            ]);
        }
        if node.as_self_node().is_some() {
            return Receiver::SelfObject(node.location().start_offset() as u32);
        }
        if is_constant(node) {
            // The end of the path, inside its last segment: `HR::Person` resolves as a whole, and
            // its graph reference ends here too.
            return Receiver::Constant(node.location().end_offset() as u32);
        }
        if let Some(at) = proc_literal(node) {
            let call = node
                .as_call_node()
                .map(|_| Box::new(self.after_literals(node, budget)));
            return Receiver::Proc { at, call };
        }
        self.after_literals(node, budget)
    }

    /// [`Self::receiver_of`] from a literal down: a `lambda { }` read as the call it also is
    /// (the `call` of [`Receiver::Proc`]) starts here.
    fn after_literals(&self, node: &Node<'_>, budget: Budget) -> Receiver {
        if let Some(class) = literal_class(node) {
            return Receiver::Literal {
                class,
                arguments: held_by(node, class),
                symbol: node
                    .as_symbol_node()
                    .map(|symbol| String::from_utf8_lossy(symbol.unescaped()).into()),
                text: Text(
                    node.as_string_node()
                        .map(|string| String::from_utf8_lossy(string.unescaped()).into()),
                ),
                names: Literals(
                    matches!(class, "Array" | "Hash")
                        .then(|| Names::of(node, 0))
                        .flatten()
                        .map(Rc::new),
                ),
            };
        }
        if let Some((at, call)) = instantiated(node) {
            return Receiver::Instance {
                at,
                arity: arity_of(&call),
                arguments: self.written_arguments(&call, budget),
                keywords: self.written_keywords(&call, budget),
            };
        }
        if let Some(receiver) = self.super_in(node) {
            return receiver;
        }
        // A read of a variable is every write that reaches it, which the table built from this
        // same text answers (see [`Receiver::Variable`]). The spelling rides along for the rung
        // below, so a variable nothing types still ends where a bare name ends.
        if let Some(span) = local_span(node).or_else(|| instance_span(node)) {
            return Receiver::Spelled {
                was: Box::new(Receiver::Variable(span.0)),
                name: self.source[span.0 as usize..span.1 as usize].to_owned(),
            };
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
        // A conditional read as a value is one of its branches, whichever ran: Ruby's rule, as for
        // `&&`. See [`Receiver::Either`].
        if let Some(receiver) = self.either(node, budget) {
            return receiver;
        }
        // What the method's own block hands back: the call's business, not this text's. See
        // [`Receiver::Yield`].
        if let Some(receiver) = self.yield_of(node, budget) {
            return receiver;
        }
        self.returned_by(node, budget)
    }

    /// `yield …`, or a call of the enclosing `def`'s own `&block`, as [`Receiver::Yield`]; `None`
    /// for any other node, and outside a `def`.
    ///
    /// A block parameter written to anywhere in the `def` is not the block any more, so its calls
    /// are ordinary ones.
    fn yield_of(&self, node: &Node<'_>, budget: Budget) -> Option<Receiver> {
        let at = node.location().start_offset() as u32;
        let def = self
            .defs
            .iter()
            .filter(|def| def.span.0 <= at && at < def.span.1)
            .max_by_key(|def| def.span.0)?;
        // `block&.call` is skipped only where no block was passed, and a call that passed none
        // answers nothing here anyway, so it reads like `block.call`.
        let handed = if let Some(found) = node.as_yield_node() {
            found.arguments()
        } else {
            let call = node.as_call_node()?;
            if !matches!(call.name().as_slice(), b"call" | b"yield" | b"[]") {
                return None;
            }
            let read = call.receiver()?.as_local_variable_read_node()?;
            let block = def.block.as_deref()?;
            if read.name().as_slice() != block.as_bytes() || self.written_in(def, block) {
                return None;
            }
            call.arguments()
        };
        let arguments = handed
            .map(|written| {
                written
                    .arguments()
                    .iter()
                    .map(|argument| self.receiver_of(Some(&argument), budget.spread()))
                    .collect()
            })
            .unwrap_or_default();
        Some(Receiver::Yield {
            at: def.span.0,
            method: def.name.clone(),
            arguments,
        })
    }

    /// Whether the `def` spanning `span` writes to its own `&block` parameter.
    fn block_rewritten(&self, span: (u32, u32)) -> bool {
        self.defs
            .iter()
            .find(|def| def.span == span)
            .and_then(|def| {
                def.block
                    .as_deref()
                    .map(|block| self.written_in(def, block))
            })
            .unwrap_or(false)
    }

    /// Whether a local named `name` is written anywhere in `def`.
    fn written_in(&self, def: &Def, name: &str) -> bool {
        self.local_writes.iter().any(|write| {
            def.span.0 <= write.name.0
                && write.name.1 <= def.span.1
                && self
                    .source
                    .get(write.name.0 as usize..write.name.1 as usize)
                    == Some(name)
        })
    }

    /// A conditional, as the values its branches hand back ([`Receiver::Either`]); `None` for any
    /// other node.
    ///
    /// Each branch spends one step of fan-out, as a shortcut's operands do. A branch whose value
    /// cannot be read stays in as [`Receiver::Unknown`], which refuses the whole value on the type
    /// side: dropping it would rest the answer on the branches that happened to be readable.
    fn either(&self, node: &Node<'_>, budget: Budget) -> Option<Receiver> {
        let mut arms = Vec::new();
        if !self.branches(node, budget, &mut arms) {
            return None;
        }
        Some(match arms.len() {
            // Every branch raises or jumps away: the conditional hands nothing back.
            0 => Receiver::Unknown,
            _ => Receiver::Either(arms),
        })
    }

    /// Push every value `node`'s branches hand back onto `arms`; `false` if `node` is not a
    /// conditional.
    fn branches(&self, node: &Node<'_>, budget: Budget, arms: &mut Vec<Receiver>) -> bool {
        let budget = budget.spread();
        // A side of a conditional on the block is one side of `block_given?`
        // ([`Receiver::BlockGiven`]).
        let sided =
            |fact: Option<bool>, def: Option<&Def>, side: Vec<Receiver>, arms: &mut Vec<_>| {
                arms.extend(side.into_iter().map(|value| match def {
                    Some(def) => guarded(fact, def.span.0, &def.name, value),
                    None => value,
                }));
            };
        if let Some(found) = node.as_if_node() {
            let (def, (holds, fails)) = self.asked(&found.predicate());
            let mut side = Vec::new();
            self.branch_value(found.statements(), budget, &mut side);
            sided(holds, def, side, arms);
            let mut side = Vec::new();
            match found.subsequent() {
                // An `elsif` is a branch of the same conditional.
                Some(otherwise) => match otherwise.as_else_node() {
                    Some(otherwise) => self.branch_value(otherwise.statements(), budget, &mut side),
                    None => {
                        self.branches(&otherwise, budget, &mut side);
                    }
                },
                None => side.push(Receiver::literal("NilClass")),
            }
            sided(fails, def, side, arms);
            return true;
        }
        if let Some(found) = node.as_unless_node() {
            let (def, (holds, fails)) = self.asked(&found.predicate());
            let mut side = Vec::new();
            self.branch_value(found.statements(), budget, &mut side);
            sided(fails, def, side, arms);
            let mut side = Vec::new();
            match found.else_clause() {
                Some(otherwise) => self.branch_value(otherwise.statements(), budget, &mut side),
                None => side.push(Receiver::literal("NilClass")),
            }
            sided(holds, def, side, arms);
            return true;
        }
        if let Some(found) = node.as_case_node() {
            for when in found.conditions().iter() {
                match when.as_when_node() {
                    Some(when) => self.branch_value(when.statements(), budget, arms),
                    None => arms.push(Receiver::Unknown),
                }
            }
            match found.else_clause() {
                Some(otherwise) => self.branch_value(otherwise.statements(), budget, arms),
                None => arms.push(Receiver::literal("NilClass")),
            }
            return true;
        }
        // No `else` is no `nil`: a value no pattern matches raises `NoMatchingPatternError`.
        if let Some(found) = node.as_case_match_node() {
            for arm in found.conditions().iter() {
                match arm.as_in_node() {
                    Some(arm) => self.branch_value(arm.statements(), budget, arms),
                    None => arms.push(Receiver::Unknown),
                }
            }
            if let Some(otherwise) = found.else_clause() {
                self.branch_value(otherwise.statements(), budget, arms);
            }
            return true;
        }
        // `begin … rescue … end`: the statements' value, or the `else`'s where one is written (it
        // replaces the statements' value when nothing was raised), or a `rescue`'s. An `ensure` runs
        // too, but its value is thrown away.
        if let Some(found) = node.as_begin_node() {
            let Some(rescued) = found.rescue_clause() else {
                return false;
            };
            match found.else_clause() {
                Some(otherwise) => self.branch_value(otherwise.statements(), budget, arms),
                None => self.branch_value(found.statements(), budget, arms),
            }
            let mut clause = Some(rescued);
            while let Some(found) = clause {
                self.branch_value(found.statements(), budget, arms);
                clause = found.subsequent();
            }
            return true;
        }
        if let Some(found) = node.as_rescue_modifier_node() {
            self.value_of(&found.expression(), budget, arms);
            self.value_of(&found.rescue_expression(), budget, arms);
            return true;
        }
        false
    }

    /// The `def` around a conditional on `predicate`, and what each side of it says about that
    /// `def`'s block ([`implied`]).
    ///
    /// **A read of `&block` counts only in the `def`'s own scope**, outside every block and
    /// lambda in it, where the name cannot be some block's own parameter.
    fn asked(&self, predicate: &Node<'_>) -> (Option<&Def>, (Option<bool>, Option<bool>)) {
        let at = predicate.location().start_offset() as u32;
        let Some(def) = self.enclosing_def(at) else {
            return (None, (None, None));
        };
        let block = def
            .block
            .as_deref()
            .filter(|block| !self.written_in(def, block));
        let sides = implied(predicate, &|read: &LocalVariableReadNode<'_>| {
            block.is_some_and(|block| read.name().as_slice() == block.as_bytes())
                && read.depth() == 0
                && !self
                    .regions
                    .iter()
                    .any(|region| region.closure && def.span.0 < region.span.0 && region.holds(at))
        });
        (Some(def), sides)
    }

    /// One branch's value: its last statement, or `nil` where it has none.
    fn branch_value(
        &self,
        statements: Option<ruby_prism::StatementsNode<'_>>,
        budget: Budget,
        arms: &mut Vec<Receiver>,
    ) {
        match statements.and_then(|found| found.body().iter().last()) {
            Some(last) => self.value_of(&last, budget, arms),
            None => arms.push(Receiver::literal("NilClass")),
        }
    }

    /// One branch's last statement as a value, or nothing where control never comes back with one.
    fn value_of(&self, last: &Node<'_>, budget: Budget, arms: &mut Vec<Receiver>) {
        if matches!(
            last,
            Node::ReturnNode { .. }
                | Node::NextNode { .. }
                | Node::BreakNode { .. }
                | Node::RedoNode { .. }
                | Node::RetryNode { .. }
        ) {
            return;
        }
        // A nested conditional is more branches of this one, not one opaque value.
        let mut nested = Vec::new();
        if self.branches(last, budget, &mut nested) {
            arms.extend(nested);
            return;
        }
        let value = self.receiver_of(Some(last), budget);
        if !never_returns(&value) {
            arms.push(value);
        }
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

    /// What a block's parameter is handed: the call the block was written on, and which parameter.
    ///
    /// Asked for a binding ([`bindings_in`]) and for a read of the parameter (through
    /// [`Variables`]). Both need the call classified, which cannot happen during the walk.
    fn yielded_shape(&self, parameter: &BlockParameter<'pr>, budget: Budget) -> Option<Receiver> {
        // One block, one call: the innermost parameter is picked before this runs, so this is a
        // link, not a visit to every candidate.
        let Receiver::Returned {
            on,
            method,
            safe,
            arity,
            arguments,
            keywords,
            ..
        } = self.receiver_of(Some(&parameter.call), budget.linked())
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
            safe,
            arity,
            arguments,
            keywords,
            spreads: parameter.spreads,
            default: parameter
                .default
                .as_ref()
                .map(|value| Box::new(self.receiver_of(Some(value), budget.spread()))),
        })
    }

    /// `x ||= v`, `x &&= v` and `x += v` as values, for a local or an instance variable.
    ///
    /// **The old value and the new one**, never the new one alone: `x ||= v` is `x` wherever `x`
    /// was truthy, and `x += 1` is `x`'s own `+`. The old value is the variable read where its
    /// name is written, which [`Variables`] answers like any read.
    fn rewritten(&self, node: &Node<'_>, budget: Budget) -> Option<Receiver> {
        enum How {
            Operator(String),
            Shortcut(bool),
        }
        let spelled = |id: ruby_prism::ConstantId<'_>| {
            How::Operator(String::from_utf8_lossy(id.as_slice()).into_owned())
        };
        let (name, value, how) = if let Some(found) = node.as_local_variable_operator_write_node() {
            (
                found.name_loc(),
                found.value(),
                spelled(found.binary_operator()),
            )
        } else if let Some(found) = node.as_instance_variable_operator_write_node() {
            (
                found.name_loc(),
                found.value(),
                spelled(found.binary_operator()),
            )
        } else if let Some(found) = node.as_local_variable_or_write_node() {
            (found.name_loc(), found.value(), How::Shortcut(false))
        } else if let Some(found) = node.as_instance_variable_or_write_node() {
            (found.name_loc(), found.value(), How::Shortcut(false))
        } else if let Some(found) = node.as_local_variable_and_write_node() {
            (found.name_loc(), found.value(), How::Shortcut(true))
        } else {
            let found = node.as_instance_variable_and_write_node()?;
            (found.name_loc(), found.value(), How::Shortcut(true))
        };
        let old = Box::new(Receiver::Variable(name.start_offset() as u32));
        let new = self.receiver_of(Some(&value), budget.spread());
        Some(match how {
            How::Operator(method) => Receiver::Returned {
                on: old,
                method,
                block: Block::None,
                arity: Arity::Exactly(1),
                arguments: vec![new],
                keywords: Some(Vec::new()),
                safe: false,
            },
            How::Shortcut(and) => Receiver::Shortcut {
                left: old,
                right: Box::new(new),
                and,
            },
        })
    }

    /// One write's value, as the shape the variable holds after it.
    ///
    /// - **A local's `x ||= v` is the old value or `v`**, like its expression ([`Self::rewritten`]):
    ///   the write kills every write above it, so the old value must come through its own read.
    /// - **An instance variable's is `v` alone.** Every write in the class reaches every read, so
    ///   the old value is already some other write's, or `nil`.
    fn write_shape(&self, write: &VariableWrite<'pr>, instance: bool) -> Receiver {
        let budget = Budget::default();
        let old = || Box::new(Receiver::Variable(write.name.0));
        match &write.assigning {
            Assigning::Value { value, index } => {
                match (index, self.receiver_of(Some(value), budget)) {
                    // A multiple-assignment target holds the value's `index`th element. An
                    // unanswerable value stays unanswerable rather than becoming an index into
                    // nothing.
                    (Some(index), receiver) if !matches!(receiver, Receiver::Unknown) => {
                        Receiver::Destructured {
                            of: Box::new(receiver),
                            index: *index,
                        }
                    }
                    (_, receiver) => receiver,
                }
            }
            Assigning::Operator { value, method } => Receiver::Returned {
                on: old(),
                method: method.clone(),
                block: Block::None,
                arity: Arity::Exactly(1),
                arguments: vec![self.receiver_of(Some(value), budget.spread())],
                keywords: Some(Vec::new()),
                safe: false,
            },
            Assigning::Shortcut { value, .. } if instance => self.receiver_of(Some(value), budget),
            Assigning::Shortcut { value, and } => Receiver::Shortcut {
                left: old(),
                right: Box::new(self.receiver_of(Some(value), budget.spread())),
                and: *and,
            },
            Assigning::Rescued { classes } => {
                classes.clone().map_or(Receiver::Unknown, Receiver::Rescued)
            }
            Assigning::Splatted { .. } => Receiver::literal("Array"),
        }
    }

    /// What a parameter binds before any write, as a shape; [`Receiver::Unknown`] where nothing
    /// says.
    fn declared_shape(&self, declared: &Declared) -> Receiver {
        match declared {
            Declared::Parameter { def, index } => {
                let def = &self.defs[*def];
                Self::parameter_shape(def, &def.parameters[*index])
            }
            Declared::Yielded(index) => self
                .yielded_shape(&self.yielded[*index], Budget::default())
                .unwrap_or(Receiver::Unknown),
            Declared::Nil => Receiver::literal("NilClass"),
            Declared::Container(shape) => shape.clone(),
            Declared::ProcParameter { at, index } => Receiver::ProcParameter {
                at: *at,
                index: *index,
            },
        }
    }

    /// Every read in the text and what can reach it: the table [`Receiver::Variable`] points into.
    /// `calls` and `uses` are the same text's [`CallEnds`] and [`Uses`], from [`Gathered`].
    fn variables(
        &self,
        calls: &CallEnds,
        uses: &HashMap<u32, Use<'pr>>,
        dropped: &HashSet<u32>,
    ) -> Variables {
        let regions = Regions::new(self.regions.clone());
        let locals: HashMap<u32, &VariableWrite<'pr>> = self
            .local_writes
            .iter()
            .map(|write| (write.name.0, write))
            .collect();
        let instances: HashMap<u32, &VariableWrite<'pr>> = self
            .instance_writes
            .iter()
            .map(|write| (write.name.0, write))
            .collect();
        let mut checked: HashMap<u32, Vec<Held<'_>>> = HashMap::new();
        for check in &self.checks {
            for fact in &check.facts {
                checked.entry(fact.local).or_default().push((check, fact));
            }
        }
        let mut table = Variables::default();
        for group in scopes::every_variable(self.source) {
            match group.scope {
                Some(scope) => {
                    let relevant: Vec<Held<'_>> = group
                        .occurrences
                        .iter()
                        .filter_map(|occurrence| checked.get(&occurrence.start))
                        .flatten()
                        .copied()
                        .collect();
                    self.reach_local(
                        &mut table,
                        &regions,
                        &group,
                        scope,
                        &locals,
                        &relevant,
                        (uses, dropped),
                    );
                }
                None => self.reach_instance(&mut table, &regions, &group, &instances, calls),
            }
        }
        table.readers = scopes::readers(self.source)
            .into_iter()
            .map(|reader| (reader.at, reader))
            .collect();
        // A setter writes whatever its callers pass: nothing in this text types it, and the type
        // side reads the calls.
        for setter in scopes::setters(self.source) {
            let write = table.write(setter.at, Receiver::Unknown);
            table
                .instances
                .entry(setter.name)
                .or_default()
                .push(InstanceWrite {
                    write,
                    level: Some(setter.level),
                    setter: true,
                });
        }
        // A reflective write on `self` is a write nothing can type (a removal is `nil`); one on
        // another object reaches a variable no class here can be said to own.
        for reflection in scopes::reflections(self.source) {
            let level = match reflection.on {
                scopes::Reflected::Own(level) => level,
                scopes::Reflected::Main => {
                    table.main.extend(reflection.names);
                    continue;
                }
                scopes::Reflected::Other => {
                    table.foreign.extend(reflection.names);
                    continue;
                }
            };
            let shape = if reflection.removes {
                Receiver::literal("NilClass")
            } else {
                Receiver::Unknown
            };
            let write = InstanceWrite {
                write: table.write(reflection.at, shape),
                level,
                setter: false,
            };
            for name in reflection.names {
                match name {
                    scopes::Spelled::Exactly(name) => {
                        table.instances.entry(name).or_default().push(write.clone());
                    }
                    // A pattern, or a parameter's argument: on `self`, whatever the callers pass
                    // is this object's, so any name it could be counts.
                    pattern => table.patterns.push((pattern, write.clone())),
                }
            }
        }
        table.initializers = self.initializers.clone();
        table.openings = self.openings.clone();
        table
    }

    /// One local's reads, each with the writes that can reach it.
    #[allow(clippy::too_many_arguments)]
    fn reach_local(
        &self,
        table: &mut Variables,
        regions: &Regions,
        group: &scopes::Group,
        scope: (u32, u32),
        writes: &HashMap<u32, &VariableWrite<'pr>>,
        relevant: &[Held<'_>],
        (uses, dropped): (&HashMap<u32, Use<'pr>>, &HashSet<u32>),
    ) {
        let mut written: Vec<Placed> = Vec::new();
        let mut bound: Option<Rc<Receiver>> = None;
        let mut reads: Vec<u32> = Vec::new();
        for occurrence in &group.occurrences {
            if !occurrence.write {
                reads.push(occurrence.start);
                continue;
            }
            if let Some(write) = writes.get(&occurrence.start) {
                // `x += 1` and `x ||= v` read `x` where its name is written, before they write.
                if !matches!(write.assigning, Assigning::Value { .. }) {
                    reads.push(occurrence.start);
                }
                written.push(Placed {
                    id: table.write(occurrence.start, self.write_shape(write, false)),
                    name: occurrence.start,
                    at: write.at,
                    closures: closures_around(regions, scope, occurrence.start),
                });
            } else if let Some(declared) = self.declared.get(&occurrence.start) {
                bound = Some(Rc::new(self.declared_shape(declared)));
            } else {
                // A binding this walk cannot read (`rescue => e`, `for x in`, a pattern's capture,
                // `*rest`), kept as the write it is, so a read it reaches refuses.
                written.push(Placed {
                    id: table.write(occurrence.start, Receiver::Unknown),
                    name: occurrence.start,
                    at: occurrence.end,
                    closures: closures_around(regions, scope, occurrence.start),
                });
            }
        }
        if let ([only], Some(fill)) = (written.as_slice(), self.fill(group, writes, uses)) {
            table.fills.insert(only.id, fill);
        }
        // An operator write's read (`x ||= v`) has no use noted, and counts as one that escapes. A
        // write whose value something else takes (`x = y = v`, a list's last statement) gives the
        // object another name.
        let kept = reads
            .iter()
            .all(|read| matches!(uses.get(read), Some(Use::Reads)))
            && written.iter().all(|write| dropped.contains(&write.name));
        for read in reads {
            let mut reaching = reaching_local(regions, scope, &written, bound.as_ref(), read);
            reaching.kept = kept;
            if !relevant.is_empty() {
                reaching.narrowed = narrowings(relevant, &written, &reaching, read);
            }
            table.reads.insert(read, reaching);
        }
    }

    /// What its own method puts into a local whose one write is an empty `[]`, `{}`, `Array.new` or
    /// `Hash.new`: every value an `Array`'s `<<`, `push`, `append`, `unshift`,
    /// `prepend`, `insert` or `[]=`, or a `Hash`'s `[]=` or `store`, adds, by position.
    ///
    /// `None` where a read of it escapes ([`Use::Escapes`]: an argument, another variable, a
    /// member that may hand it or change it some other way), where an added value reads the local
    /// itself, and where nothing is added.
    fn fill(
        &self,
        group: &scopes::Group,
        writes: &HashMap<u32, &VariableWrite<'pr>>,
        uses: &HashMap<u32, Use<'pr>>,
    ) -> Option<Fill> {
        let mut occurrences = group.occurrences.iter();
        let write = writes.get(&occurrences.find(|occurrence| occurrence.write)?.start)?;
        let Assigning::Value { value, index: None } = &write.assigning else {
            return None;
        };
        let class = empty_container(value)?;
        let mut held: Vec<Vec<(u32, Receiver)>> =
            vec![Vec::new(); if class == "Hash" { 2 } else { 1 }];
        let reads: Vec<u32> = group
            .occurrences
            .iter()
            .filter(|occurrence| !occurrence.write)
            .map(|occurrence| occurrence.start)
            .collect();
        for read in &reads {
            match uses.get(read)? {
                Use::Escapes => return None,
                Use::Reads => {}
                Use::Adds(values, keyed) => {
                    // An `Array`'s `[]=` adds its value (the index says where); a `Hash` takes a
                    // key and a value, and has no `<<`.
                    let added: Vec<(usize, &Node<'pr>)> = match (class, keyed) {
                        ("Hash", false) => return None,
                        ("Hash", true) => values.iter().enumerate().collect(),
                        (_, true) => values.iter().skip(1).map(|value| (0, value)).collect(),
                        (_, false) => values.iter().map(|value| (0, value)).collect(),
                    };
                    for (position, node) in added {
                        let span = span_of(node);
                        if reads.iter().any(|read| span.0 <= *read && *read < span.1) {
                            return None;
                        }
                        held.get_mut(position)?
                            .push((span.0, self.receiver_of(Some(node), Budget::default())));
                    }
                }
            }
        }
        (!held[0].is_empty()).then_some(Fill { class, held })
    }

    /// One instance variable's reads, each reached by every write to it in the file, and its
    /// writes, indexed for a read in another text.
    ///
    /// - **Methods run in any order**, so every write reaches every read, and `nil` does too unless
    ///   the object cannot exist without the variable set (`initialize` writes it as a statement of
    ///   its own body) or the reading method has already set it.
    /// - **That is this text's answer, and only the top level's.** Every method of every class the
    ///   object can be an instance of may write the variable too, so where a namespace names the
    ///   object the type side folds all of those instead ([`InstanceRead`]).
    /// - **An occurrence whose `self` is not known refuses here** ([`scopes::Group::loose`]): its
    ///   row holds a write nothing types, and says it is loose, for the type side to ask the
    ///   block's call ([`InstanceRead::loose`]). Its writes are indexed at every level.
    fn reach_instance(
        &self,
        table: &mut Variables,
        regions: &Regions,
        group: &scopes::Group,
        writes: &HashMap<u32, &VariableWrite<'pr>>,
        calls: &CallEnds,
    ) {
        let loose = |at: u32| group.loose.contains(&at);
        // `@x = value`: a write whose value is the variable's, all of it.
        let plain = |name: u32| {
            writes.get(&name).is_some_and(|write| {
                matches!(write.assigning, Assigning::Value { index: None, .. })
            })
        };
        let written: Vec<Placed> = group
            .occurrences
            .iter()
            .filter(|occurrence| occurrence.write)
            .map(|occurrence| {
                let found = writes.get(&occurrence.start);
                // Each write names its line, for the card ([`Receiver::Assigned`]).
                let shape = Receiver::Assigned {
                    at: occurrence.start,
                    was: Box::new(
                        found.map_or(Receiver::Unknown, |write| self.write_shape(write, true)),
                    ),
                };
                Placed {
                    id: table.write(occurrence.start, shape),
                    name: occurrence.start,
                    at: found.map_or(occurrence.end, |write| write.at),
                    closures: Vec::new(),
                }
            })
            .collect();
        if group.level.is_some() {
            let indexed = table.instances.entry(group.name.clone()).or_default();
            for write in &written {
                indexed.push(InstanceWrite {
                    write: write.id,
                    level: if loose(write.name) { None } else { group.level },
                    setter: false,
                });
            }
        }
        let initialized = written
            .iter()
            .any(|write| self.initialized.contains(&write.name));
        let ids: Rc<[usize]> = written.iter().map(|write| write.id).collect();
        let within = |at: u32| self.enclosing_def(at).map(|def| def.span);
        for occurrence in &group.occurrences {
            let read = occurrence.start;
            if loose(read) {
                let unknown = table.write(read, Receiver::Unknown);
                table.reads.insert(
                    read,
                    Reaching {
                        writes: Rc::new([unknown]),
                        nil: true,
                        bound: None,
                        instance: Some(Box::new(InstanceRead {
                            name: group.name.clone(),
                            level: None,
                            set_here: false,
                            loose: true,
                            method: None,
                            decided: None,
                        })),
                        narrowed: Box::default(),
                        kept: false,
                    },
                );
                continue;
            }
            let set_here = within(read).is_some_and(|def| {
                written.iter().any(|write| {
                    write.at <= read
                        && within(write.name) == Some(def)
                        && dominates(regions, write.name, read)
                })
            });
            // The last write in the reading `def` that runs on every path, where
            // nothing between it and the read can run code (no call ends there), no other write
            // of the variable starts there, and no loop or block around the read leaves it out.
            let decided = within(read).and_then(|def| {
                let last = written
                    .iter()
                    .filter(|write| {
                        write.at <= read
                            && within(write.name) == Some(def)
                            && dominates(regions, write.name, read)
                    })
                    .max_by_key(|write| write.at)?;
                let quiet = plain(last.name)
                    && !written
                        .iter()
                        .any(|write| write.name >= last.at && write.name < read)
                    && !calls.between(last.name, last.at, read)
                    && regions
                        .around(read)
                        .filter(|region| region.repeat || region.closure)
                        .all(|region| region.holds(last.name));
                quiet.then_some(last.id)
            });
            table.reads.insert(
                read,
                Reaching {
                    writes: Rc::clone(&ids),
                    nil: !initialized && !set_here,
                    bound: None,
                    instance: Some(Box::new(InstanceRead {
                        name: group.name.clone(),
                        level: group.level,
                        set_here,
                        loose: false,
                        method: self.enclosing_def(read).map(|def| def.name.clone()),
                        decided,
                    })),
                    narrowed: Box::default(),
                    kept: false,
                },
            );
        }
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
        // A call Prism recovered with no message span (a mid-edit `foo.`) has no name *here*.
        // `foo.()` has none either, but it is `foo.call()` written short, and its parenthesis says
        // so: a recovered call has none (for a proc called that way).
        let nameless = call
            .message_loc()
            .is_none_or(|message| message.start_offset() == message.end_offset());
        if nameless && !(name.as_slice() == b"call" && call.opening_loc().is_some()) {
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
                keywords: self.written_keywords(&call, budget),
                safe: false,
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
        // The linear arm, and the only one the `links` budget ([`MAX_WIDTH`]) is spent on: one call
        // per `.` written, with no candidate list below.
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
            keywords: self.written_keywords(&call, budget),
            safe: call.is_safe_navigation(),
        }
    }

    /// The shape of every positional argument a call wrote, or nothing at all.
    ///
    /// - **[`arity_of`]'s twin**, walking the same list by the same rules (a keyword hash is
    ///   skipped; a splat or `...` gives up), so the two cannot disagree about what an argument is.
    /// - **A fan-out step**: the walk visits several expressions instead of following one.
    /// - **Past [`MAX_ARGUMENTS`] arguments, or with the budget spent, the list is emptied, not
    ///   shortened.** A partial list is worse than none: every position after a dropped one would
    ///   be miscounted, and the consumer needs one shape per counted argument. See
    ///   [`Receiver::Returned`]'s `arguments`.
    fn written_arguments(&self, call: &CallNode<'_>, budget: Budget) -> Vec<Receiver> {
        self.listed_arguments(call.arguments().as_ref(), budget)
    }

    /// [`Self::written_arguments`] of an argument list, which `super(a, b)` writes too.
    fn listed_arguments(
        &self,
        written: Option<&ruby_prism::ArgumentsNode<'_>>,
        budget: Budget,
    ) -> Vec<Receiver> {
        let Some(written) = written else {
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
            if shapes.len() >= MAX_ARGUMENTS {
                return Vec::new();
            }
            shapes.push(self.receiver_of(Some(&argument), budget.spread()));
        }
        shapes
    }

    /// The keywords a call wrote without braces, by name, or `None` where that cannot be read.
    ///
    /// - **[`written_arguments`]'s other half**: it skips the keyword hash, and this reads only
    ///   that, under the same width budget.
    /// - **`None`, not a shorter list, for anything but `name: value`**: a `**` splat can pass any
    ///   keyword, and a string or computed key is not a parameter's name.
    fn written_keywords(
        &self,
        call: &CallNode<'_>,
        budget: Budget,
    ) -> Option<Vec<(String, Receiver)>> {
        self.listed_keywords(call.arguments().as_ref(), budget)
    }

    /// [`Self::written_keywords`] of an argument list, which `super(a, b)` writes too.
    fn listed_keywords(
        &self,
        written: Option<&ruby_prism::ArgumentsNode<'_>>,
        budget: Budget,
    ) -> Option<Vec<(String, Receiver)>> {
        let Some(written) = written else {
            return Some(Vec::new());
        };
        if budget.spent() {
            return None;
        }
        let mut keywords = Vec::new();
        for argument in written.arguments().iter() {
            let Some(hash) = argument.as_keyword_hash_node() else {
                continue;
            };
            for element in hash.elements().iter() {
                let pair = element.as_assoc_node()?;
                let key = pair.key().as_symbol_node()?;
                let name = String::from_utf8(key.unescaped().to_vec()).ok()?;
                if keywords.len() >= MAX_ARGUMENTS {
                    return None;
                }
                keywords.push((name, self.receiver_of(Some(&pair.value()), budget.spread())));
            }
        }
        Some(keywords)
    }

    /// What a call's block slot holds: whether a block was written, and what it returns.
    fn block_written(&self, call: &CallNode<'_>, budget: Budget) -> Block {
        let Some(written) = call.block() else {
            return Block::None;
        };
        // `&:upcase` is the method on what the block is handed. A forwarded `&blk` passes a block
        // too, so the signature's block arm applies, but there is no body here to read a value from.
        let Some(block) = written.as_block_node() else {
            let expression = written
                .as_block_argument_node()
                .and_then(|argument| argument.expression());
            if let Some(symbol) = expression.as_ref().and_then(|found| found.as_symbol_node()) {
                return Block::Symbol(String::from_utf8_lossy(symbol.unescaped()).into_owned());
            }
            if let Some(value) = expression {
                return Block::Passed(Box::new(self.receiver_of(Some(&value), budget.spread())));
            }
            return Block::Forwarded;
        };
        let (exits, breaks) = self.handed_back(&block, budget.spread());
        // A literal's `break` leaves the lambda, or raises once `proc` has returned: never a value
        // of the call that made it.
        if breaks.is_empty() || proc_literal(&call.as_node()).is_some() {
            Block::Written(exits)
        } else {
            Block::Breaking(Box::new(Breaks { exits, breaks }))
        }
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
    /// - **A `next` is one more value of the block, and a `break` a value of the call**,
    ///   wherever they are written in this block's own body ([`Leaving`]). In tail position they,
    ///   like a `return`, hand the block nothing themselves: a `return` leaves the enclosing method,
    ///   so that path gives the call no value at all.
    /// - **A spread, not a link**: the block is a second expression hanging off the call, not
    ///   another `.` in the chain, and a block whose tail is a local sends the walk to every write
    ///   of that name.
    fn handed_back(
        &self,
        block: &BlockNode<'_>,
        budget: Budget,
    ) -> (Box<[Receiver]>, Box<[Receiver]>) {
        let mut leaving = Leaving::default();
        if let Some(body) = block.body() {
            leaving.visit(&body);
        }
        if leaving.unreadable {
            return (Box::new([Receiver::Unknown]), Box::default());
        }
        let shape = |value: &Option<Node<'_>>| match value {
            Some(node) => self.receiver_of(Some(node), budget),
            None => Receiver::literal("NilClass"),
        };
        let here = span_of(&block.as_node());
        let handed = self
            .tails_of(block.body(), here, budget)
            .into_iter()
            .chain(leaving.nexts.iter().map(shape))
            .collect();
        (handed, leaving.breaks.iter().map(shape).collect())
    }

    /// One proc literal's [`ProcShape`].
    fn proc_shape(&self, literal: &Node<'_>) -> ProcShape {
        let (lambda, parameters, body) = if let Some(found) = literal.as_lambda_node() {
            (true, found.parameters(), found.body())
        } else {
            let call = literal
                .as_call_node()
                .expect("a proc literal is a lambda or a call");
            let block = call
                .block()
                .and_then(|block| block.as_block_node())
                .expect("a proc literal call writes a block");
            (
                call.name().as_slice() == b"lambda",
                block.parameters(),
                block.body(),
            )
        };
        let budget = Budget::default();
        let parameters = match parameters
            .and_then(|written| written.as_block_parameters_node())
            .and_then(|written| written.parameters())
        {
            None => Some(ProcParameters {
                required: 0,
                defaults: Vec::new(),
                rest: false,
                spreads: false,
            }),
            Some(written) => {
                let plain = written
                    .requireds()
                    .iter()
                    .all(|parameter| parameter.as_required_parameter_node().is_some())
                    && written.posts().iter().count() == 0
                    && written.keywords().iter().count() == 0
                    && written.keyword_rest().is_none();
                plain.then(|| {
                    let required = written.requireds().iter().count();
                    let defaults: Vec<Receiver> = written
                        .optionals()
                        .iter()
                        .filter_map(|parameter| {
                            let optional = parameter.as_optional_parameter_node()?;
                            Some(self.receiver_of(Some(&optional.value()), budget))
                        })
                        .collect();
                    let rest = written.rest().is_some();
                    let positionals = required + defaults.len();
                    ProcParameters {
                        required,
                        defaults,
                        rest,
                        spreads: positionals > 1 || (positionals == 1 && rest),
                    }
                })
            }
        };
        let here = span_of(literal);
        let mut leaving = Leaving::default();
        let mut returning = Returning::default();
        if let Some(found) = &body {
            leaving.visit(found);
            returning.visit(found);
        }
        // A proc's `return` and `break` leave the method it was written in, not the proc.
        let escapes = !lambda && (!leaving.breaks.is_empty() || !returning.found.is_empty());
        if leaving.unreadable || returning.unreadable || escapes {
            return ProcShape {
                lambda,
                parameters,
                exits: vec![Receiver::Unknown],
            };
        }
        let shape = |value: &Option<Node<'_>>| match value {
            Some(node) => self.receiver_of(Some(node), budget),
            None => Receiver::literal("NilClass"),
        };
        let mut exits = self.tails_of(body, here, budget);
        exits.extend(leaving.nexts.iter().map(shape));
        exits.extend(leaving.breaks.iter().map(shape));
        exits.extend(returning.found.iter().map(shape));
        ProcShape {
            lambda,
            parameters,
            exits,
        }
    }

    /// A block's or lambda's tail values ([`Exits::tail`]), without the `next`, `break` and
    /// `return` written there: each hands back its own value, which the caller reads apart.
    fn tails_of(&self, body: Option<Node<'_>>, here: (u32, u32), budget: Budget) -> Vec<Receiver> {
        // **`block_given?` in a block asks about the `def` around it**, whose frame the block
        // runs in. Its `&block` is not followed in here: a read of it is one scope further out.
        let mut exits = Exits::new(vec![here], None);
        match body {
            Some(body) => match body.as_begin_node() {
                Some(found) => exits.rescued(&found, 0),
                None => exits.statements(body.as_statements_node().as_ref(), 0),
            },
            None => exits.push(Exit::Nil),
        }
        let def = self.enclosing_def(here.0);
        exits
            .found
            .remove(&here)
            .unwrap_or_default()
            .iter()
            .filter_map(|(exit, given)| {
                let value = match exit {
                    Exit::Written(node)
                        if node.as_next_node().is_some()
                            || node.as_break_node().is_some()
                            || node.as_return_node().is_some() =>
                    {
                        return None;
                    }
                    Exit::Written(node) => self.receiver_of(Some(node), budget),
                    Exit::Nil => Receiver::literal("NilClass"),
                    Exit::Unknown => Receiver::Unknown,
                };
                Some(match def {
                    Some(def) => guarded(*given, def.span.0, &def.name, value),
                    None => value,
                })
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

/// The last statement of a `begin … end` that rescues nothing, `Some(None)` where it has none, or
/// `None` for any other node. An `ensure` runs too, but its value is thrown away.
///
/// An `else` with no `rescue` is a syntax error Prism recovers from, so it is not read.
fn unrescued<'pr>(node: &Node<'pr>) -> Option<Option<Node<'pr>>> {
    let found = node.as_begin_node()?;
    if found.rescue_clause().is_some() || found.else_clause().is_some() {
        return None;
    }
    Some(
        found
            .statements()
            .and_then(|statements| statements.body().iter().last()),
    )
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
    let mut keyed = false;
    let mut spread = true;
    for argument in arguments.arguments().iter() {
        if argument.as_splat_node().is_some() || argument.as_forwarding_arguments_node().is_some() {
            return Arity::Unknown;
        }
        if let Some(hash) = argument.as_keyword_hash_node() {
            keyed = true;
            spread &= hash
                .elements()
                .iter()
                .all(|element| element.as_assoc_splat_node().is_some());
        } else {
            written += 1;
        }
    }
    match (keyed, spread) {
        (true, true) => Arity::Spread(written),
        (true, false) => Arity::Keyed(written),
        (false, _) => Arity::Exactly(written),
    }
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
/// **`||=`, `&&=` and `+=` never reach this.** [`Finder::rewritten`] runs first and answers each
/// as the old value and the new one together (a `Shortcut`, or the operator's call). What is left
/// out here:
///
/// - **`obj&.x = v`.** It returns `nil` when the receiver is `nil`, so it is `v | nil`: a union,
///   answered where it is detected ([`is_safe_attribute_write`]).
fn assigned_value<'pr>(node: &Node<'pr>) -> Option<Node<'pr>> {
    let value = if let Some(found) = node.as_local_variable_write_node() {
        found.value()
    } else if let Some(found) = node.as_instance_variable_write_node() {
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
/// - **Safe navigation is left out**: `obj&.x = v` is `nil` where the receiver is, a union
///   [`Finder::receiver_of`] builds itself.
/// - **No fallback to the signature.** `Hash#[]=` is `(K, V) -> V`, but the argument written on the
///   line is the value itself, better than the bound declared on it.
fn attribute_written<'pr>(node: &ruby_prism::CallNode<'pr>) -> Option<Node<'pr>> {
    if !node.is_attribute_write() || node.is_safe_navigation() {
        return None;
    }
    node.arguments()?.arguments().iter().last()
}

/// Where a proc or lambda literal starts, or `None` for any other node ([`Receiver::Proc`]).
///
/// `->(x) { }`, and a block written on a receiverless `lambda` or `proc`, or on `Proc.new`, with
/// no other argument.
fn proc_literal(node: &Node<'_>) -> Option<u32> {
    if node.as_lambda_node().is_some() {
        return Some(node.location().start_offset() as u32);
    }
    let call = node.as_call_node()?;
    call.block()?.as_block_node()?;
    if call.arguments().is_some() {
        return None;
    }
    let literal = match call.receiver() {
        None => matches!(call.name().as_slice(), b"lambda" | b"proc"),
        Some(receiver) => {
            call.name().as_slice() == b"new"
                && receiver
                    .as_constant_read_node()
                    .is_some_and(|constant| constant.name().as_slice() == b"Proc")
        }
    };
    literal.then(|| node.location().start_offset() as u32)
}

/// Whether this is the one write [`attribute_written`] leaves out: `obj&.x = v`.
///
/// Answered in [`Finder::receiver_of`] as the argument or `nil`, not here, because merely returning
/// `None` would not do it: the arms below would go on to answer the node as an ordinary call,
/// whose message is the **getter**'s name.
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

/// `Foo.new` and `Foo::Bar.new`, as an offset into the constant, and the call for its arguments.
///
/// Only the literal message `new`. Overriding `new` to return something else is rare. A factory
/// method with another name needs its return type, which the signature rung handles, not this
/// syntax check.
fn instantiated<'pr>(node: &Node<'pr>) -> Option<(u32, CallNode<'pr>)> {
    let call = node.as_call_node()?;
    if call.name().as_slice() != b"new" {
        return None;
    }
    let receiver = call.receiver()?;
    is_constant(&receiver).then(|| (receiver.location().end_offset() as u32, call))
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

    #[test]
    fn a_call_rubydex_files_away_from_its_name_is_found_on_that_name() {
        let at = |source: &str, needle: &str| {
            misplaced(&Parsed::new(source), source.find(needle).unwrap() as u32)
        };

        let source = "a.b ||= 1\na.c &&= 2\na.d += 3\n";
        assert_eq!(
            at(source, "b ||="),
            Some(Misplaced::Parked {
                message: (2, 3),
                operator: (4, 7),
                name: "b".to_owned(),
            })
        );
        assert!(matches!(at(source, "c &&="), Some(Misplaced::Parked { name, .. }) if name == "c"));
        // `+=` is filed on the `.` before the name.
        assert_eq!(
            at(source, "d +="),
            Some(Misplaced::Parked {
                message: (22, 23),
                operator: (21, 22),
                name: "d".to_owned(),
            })
        );
        // The receiver and the operator are rubydex's to place.
        assert_eq!(at(source, "a.b"), None);
        assert_eq!(at(source, "||="), None);

        let source = "x.a.b::C\nY::Z\nx.c::D::E\n";
        assert_eq!(
            at(source, "b::C"),
            Some(Misplaced::Unrecorded {
                message: (4, 5),
                name: "b".to_owned(),
            })
        );
        // Anywhere inside the parent: rubydex visits none of it.
        assert!(
            matches!(at(source, "a.b"), Some(Misplaced::Unrecorded { name, .. }) if name == "a")
        );
        assert!(
            matches!(at(source, "c::D"), Some(Misplaced::Unrecorded { name, .. }) if name == "c")
        );
        // The constants are filed, and a constant parent holds no call.
        assert_eq!(at(source, "C\n"), None);
        assert_eq!(at(source, "Y::Z"), None);
        // An ordinary call is recorded, so it is not asked about, and a path from the top has no
        // parent to hold one.
        assert_eq!(at("x.a.b\n", "b\n"), None);
        assert_eq!(at("::Top\n", "Top"), None);
    }

    #[test]
    fn the_message_of_an_operator_write_is_a_call_on_its_receiver() {
        let context = |source: &str, needle: &str| {
            super::at(&Parsed::new(source), source.find(needle).unwrap() as u32)
                .map(|cursor| cursor.context)
        };
        for source in ["box.size ||= 1\n", "box.size &&= 1\n", "box.size += 1\n"] {
            assert!(
                matches!(context(source, "size"), Some(Context::MethodCall { .. })),
                "{source}: {:?}",
                context(source, "size")
            );
            // The value written is not the member: nothing there is a call on `box`.
            assert!(
                !matches!(context(source, "1"), Some(Context::MethodCall { .. })),
                "{source}"
            );
        }
    }

    /// `Foo.new` with nothing passed: an instance, with the constant's name ending at `at`.
    fn built(at: u32) -> Receiver {
        Receiver::Instance {
            at,
            arity: Arity::Exactly(0),
            arguments: Vec::new(),
            keywords: Some(Vec::new()),
        }
    }

    /// Classify the cursor written as `~` in the fixture; the `~` is removed before parsing.
    fn at_marker(marked: &str) -> Option<(Cursor, String)> {
        let offset = marked.find('~').expect("a ~ marking the cursor") as u32;
        let source = marked.replace('~', "");
        at(&Parsed::new(&source), offset).map(|cursor| {
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
            keywords: Some(Vec::new()),
            safe: false,
        }
    }

    /// An integer literal's shape, the argument most tests here use.
    fn integer() -> Receiver {
        Receiver::literal("Integer")
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
                keywords: Some(Vec::new()),
                safe: false,
            }),
            name: name.to_owned(),
        }
    }

    /// The row of the table for the variable read the `~` completes after, if the text writes or
    /// binds that variable at all.
    ///
    /// Read from the table built over the *repaired* text, as completion reads it (see
    /// [`Cursor::repaired`]).
    fn row(marked: &str) -> Option<Reaching> {
        let (cursor, _) = at_marker(marked).expect("a cursor");
        let Context::MethodCall {
            receiver: Receiver::Spelled { was, .. },
        } = cursor.context
        else {
            panic!(
                "expected a method call on a variable, got {:?}",
                cursor.context
            );
        };
        let Receiver::Variable(read) = *was else {
            panic!("expected a variable read, got {was:?}");
        };
        let text = cursor.repaired.unwrap_or_else(|| marked.replace('~', ""));
        shapes(&text).variables.reaching(read).cloned()
    }

    /// What can reach the variable read the `~` completes after: each reaching write's shape,
    /// whether `nil` can, and the parameter's own value where one can.
    fn reached(marked: &str) -> (Vec<Receiver>, bool, Option<Receiver>) {
        let (cursor, _) = at_marker(marked).expect("a cursor");
        let text = cursor.repaired.unwrap_or_else(|| marked.replace('~', ""));
        let table = shapes(&text).variables;
        let reaching = row(marked).expect("a row for the read");
        (
            reaching
                .writes
                .iter()
                .map(|write| table.assignment(*write).unwrap().shape.clone())
                .collect(),
            reaching.nil,
            reaching.bound.as_deref().cloned(),
        )
    }

    /// The one write that reaches the read, where exactly one does and neither `nil` nor a
    /// parameter can. Nearly every test here is about that shape alone;
    /// `a_typed_variable_still_carries_the_name_it_is_written_as` pins the wrapper once.
    fn typed(marked: &str) -> Receiver {
        match reached(marked) {
            (mut writes, false, None) if writes.len() == 1 => writes.pop().unwrap(),
            // No write, only what a parameter binds.
            (writes, false, Some(bound)) if writes.is_empty() => bound,
            other => panic!("expected exactly one value to reach, got {other:?}"),
        }
    }

    /// A read of the variable spelled `name`, whose row starts at `at`.
    fn read_of(at: u32, name: &str) -> Receiver {
        Receiver::Spelled {
            was: Box::new(Receiver::Variable(at)),
            name: name.to_owned(),
        }
    }

    /// An instance variable's write as the table holds it: the value, and where its name starts.
    fn written(at: u32, was: Receiver) -> Receiver {
        Receiver::Assigned {
            at,
            was: Box::new(was),
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
    fn a_bang_with_an_argument_or_a_block_is_a_method_not_the_operator() {
        // `x.!(y)` is someone's own two-argument `!`, and a `!` with a block is not `!x` either:
        // both are ordinary calls, answered by whatever `!` the receiver declares.
        for source in ["x = 1\ny = x.!(2)\ny.~\n", "x = 1\ny = x.! { 2 }\ny.~\n"] {
            let shape = typed(source);
            assert!(
                !matches!(shape, Receiver::Negated(_)),
                "{source}: {shape:?}"
            );
        }
    }

    #[test]
    fn a_method_ending_in_begin_and_rescue_returns_either() {
        // The last statement is a `begin` with a `rescue`: the `begin`'s value when nothing is
        // raised, each `rescue`'s otherwise. Read as one unreadable node, it would decline.
        let source = "def f\n  begin\n    1\n  rescue ArgumentError\n    \"x\"\n  rescue\n    nil\n  end\nend\n";
        let exits = returns_of(source, (0, source.len() as u32 - 1));
        assert_eq!(
            exits,
            vec![
                Receiver::literal("Integer"),
                Receiver::literal("String"),
                Receiver::literal("NilClass"),
            ]
        );
    }

    #[test]
    fn every_construct_that_may_not_run_or_may_run_again_is_read_as_such() {
        let nil = || Receiver::literal("NilClass");
        let string = || Receiver::literal("String");
        // A loop brings back the write below the read, and its body may not run.
        assert_eq!(
            reached("x = nil\nwhile c\n  x.~\n  x = 1\nend\n"),
            (vec![nil(), integer()], false, None)
        );
        assert_eq!(
            reached("until c\n  x = 1\nend\nx.~\n"),
            (vec![integer()], true, None)
        );
        // `begin ... end until` runs its body once first, so its write is on every path.
        assert_eq!(
            reached("x = 1\nbegin\n  x = \"s\"\nend until c\nx.~\n"),
            (vec![string()], false, None)
        );
        // So does `begin ... end while`.
        assert_eq!(
            reached("x = 1\nbegin\n  x = \"s\"\nend while c\nx.~\n"),
            (vec![string()], false, None)
        );
        // `begin` may stop at any statement, and a `rescue` runs only sometimes.
        assert_eq!(
            reached("begin\n  x = 1\nrescue\n  x = \"s\"\nend\nx.~\n"),
            (vec![integer(), string()], true, None)
        );
        // A `retry` runs the `begin` again, which is a loop.
        assert_eq!(
            reached("x = nil\nbegin\n  x.~\n  x = 1\nrescue\n  retry\nend\n"),
            (vec![nil(), integer()], false, None)
        );
        assert_eq!(
            reached("x = nil\nbegin\n  x.~\n  x = 1\nrescue\n  nil\nend\n"),
            (vec![nil()], false, None)
        );
        // Two branches one after the other each add.
        assert_eq!(
            reached("if a\n  x = 1\nend\nif b\n  x = \"s\"\nend\nx.~\n"),
            (vec![integer(), string()], true, None)
        );
        // A local's `||=` is its old value or the new one, read where its name is written, and it
        // runs on every path.
        let source = "x = nil\nx ||= \"s\"\nx.~\n";
        let at = source.find("x ||=").unwrap() as u32;
        assert_eq!(
            reached(source),
            (
                vec![Receiver::Shortcut {
                    left: Box::new(Receiver::Variable(at)),
                    right: Box::new(string()),
                    and: false,
                }],
                false,
                None
            )
        );
        // `&&=` as a value: the old value where it was falsy, the new one otherwise.
        assert_eq!(
            receiver("(x &&= \"s\").~"),
            stored(Receiver::Shortcut {
                left: Box::new(Receiver::Variable(1)),
                right: Box::new(string()),
                and: true,
            })
        );
        // A block-local starts fresh on every call: the block around it is its scope, not a loop
        // over it.
        assert_eq!(
            reached("[1].each do\n  y = 1\n  y.~\n  y = \"s\"\nend\n"),
            (vec![integer()], false, None)
        );
        // A write inside a lambda may run whenever the lambda is called, so a later write above
        // the read does not kill it; but a lambda made after the read cannot have run before it.
        assert_eq!(
            reached("x = 1\nf = -> { x = \"s\" }\nx = 2\nx.~\n"),
            (vec![string(), integer()], false, None)
        );
        assert_eq!(
            reached("x = 1\nx.~\nf = -> { x = \"s\" }\n"),
            (vec![integer()], false, None)
        );
        // A read inside a lambda may run after every write below it in the variable's scope.
        assert_eq!(
            reached("x = 1\nf = -> { x.~ }\nx = \"s\"\n"),
            (vec![integer(), string()], false, None)
        );
        // A destructure of something no shape describes stays unanswerable.
        assert_eq!(typed("a, b = (1; 2)\na.~\n"), Receiver::Unknown);
    }

    #[test]
    fn a_local_is_every_write_that_can_reach_it() {
        assert_eq!(typed("x = 1\nx.~"), Receiver::literal("Integer"));
        assert_eq!(
            typed("x = 1\nother = \"s\"\nx.~"),
            Receiver::literal("Integer"),
            "a later write to a different name is not this one's"
        );
        // A write that runs on every path kills every write above it, typed or not: it is the one
        // that ran (#11).
        assert_eq!(
            typed("x = whatever\nx = \"s\"\nx.~"),
            Receiver::literal("String")
        );
        assert_eq!(
            typed("x = \"s\"\nx = whatever\nx.~"),
            self_call(12, "whatever"),
            "a later write that types nothing is still the write that ran"
        );
        // A write in a branch adds to the one above it and kills nothing (#10).
        assert_eq!(
            reached("x = \"s\"\nx = 1 if c\nx.~"),
            (
                vec![Receiver::literal("String"), Receiver::literal("Integer")],
                false,
                None
            )
        );
        // With no write on every path, `nil` reaches: Ruby made the local when it parsed the
        // write.
        assert_eq!(
            reached("x = 1 if c\nx.~"),
            (vec![Receiver::literal("Integer")], true, None)
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
                    // The one literal that also carries its own name.
                    "Symbol" if source == ":name.~" => Receiver::Literal {
                        class,
                        arguments: Vec::new(),
                        symbol: Some("name".into()),
                        text: Text::default(),
                        names: Literals::default(),
                    },
                    _ => Receiver::literal(class),
                },
                "classifying {source:?}"
            );
        }
    }

    /// An assignment's value read as a value ([`Receiver::Stored`]).
    fn stored(value: Receiver) -> Receiver {
        Receiver::Stored(Box::new(value))
    }

    /// A literal carrying what it holds, for tests about something else.
    fn holding(class: &'static str, arguments: &[Option<&'static str>]) -> Receiver {
        Receiver::Literal {
            class,
            arguments: arguments.to_vec(),
            symbol: None,
            text: Text::default(),
            names: Literals::default(),
        }
    }

    /// An array or a hash literal of names, at every level, is read as the names it spells; one
    /// element of anything else, a key that is no name, or a literal nested past the guard spells
    /// nothing.
    #[test]
    fn a_literal_of_names_carries_what_it_spells() {
        let spelled = |source: &str| match receiver(source) {
            Receiver::Literal {
                names: Literals(names),
                ..
            } => names.map(|names| (*names).clone()),
            other => panic!("not a literal: {other:?}"),
        };
        let symbol = |name: &str| Names::Symbol(name.into());
        assert_eq!(
            spelled("[:title, \"body\", { tags: [] }, address: [:street]].~"),
            Some(Names::List(
                vec![
                    symbol("title"),
                    Names::Text("body".into()),
                    Names::Pairs(vec![("tags".into(), Names::List(Box::default()))].into()),
                    Names::Pairs(
                        vec![("address".into(), Names::List(vec![symbol("street")].into()))].into()
                    ),
                ]
                .into()
            ))
        );
        assert_eq!(
            spelled("{ \"meta\" => {} }.~"),
            Some(Names::Pairs(
                vec![("meta".into(), Names::Pairs(Box::default()))].into()
            ))
        );
        assert_eq!(spelled("[:a, b].~"), None);
        assert_eq!(spelled("{ key => 1 }.~"), None);
        assert_eq!(spelled("{ [:a] => 1 }.~"), None);
        assert_eq!(spelled("{ **rest }.~"), None);
        let deep = format!(
            "{}:a{}.~",
            "[".repeat(NAMES_DEEP + 1),
            "]".repeat(NAMES_DEEP + 1)
        );
        assert_eq!(spelled(&deep), None);
        let shallow = format!("{}:a{}.~", "[".repeat(NAMES_DEEP), "]".repeat(NAMES_DEEP));
        assert!(spelled(&shallow).is_some());
        assert_eq!(spelled(":a.~"), None);
    }

    /// A constant assigned an array or a hash literal of names frozen as written holds those
    /// names, by its name's last segment, whichever of the four forms rubydex files a definition
    /// for; one left unfrozen, frozen with anything, or holding anything but names holds
    /// nothing. An operator write is a constant written again, by the same span.
    #[test]
    fn a_constant_frozen_as_written_holds_its_names_and_an_operator_writes_it_again() {
        let source = "\
A = %i[a].freeze
Outer::B = [:b, { c: [] }].freeze
D ||= { d: {} }.freeze
Outer::E ||= %i[e].freeze
F = %i[f]
G = [:g, h].freeze
H = %i[h].freeze(true)
I = %i[i].freeze { }
J = \"j\".freeze
K = %i[k].dup
A += [:x]
Outer::B &&= nil
D -= []
Outer::E |= []
";
        let span = |name: &str| {
            let at = source.find(&format!("{name} ")).unwrap() as u32;
            (at, at + name.len() as u32)
        };
        let segment = |path: &str, name: &str| {
            let at = (source.find(path).unwrap() + path.len() - name.len()) as u32;
            (at, at + name.len() as u32)
        };
        let last = |path: &str, name: &str| {
            let at = (source.rfind(path).unwrap() + path.len() - name.len()) as u32;
            (at, at + name.len() as u32)
        };
        let symbol = |name: &str| Names::Symbol(name.into());
        let list = |names: Vec<Names>| Names::List(names.into());
        let found = frozen_constants(source);
        let frozen: HashMap<(u32, u32), Names> = found
            .frozen
            .iter()
            .map(|(span, names)| (*span, (**names).clone()))
            .collect();
        assert_eq!(
            frozen,
            HashMap::from([
                (span("A"), list(vec![symbol("a")])),
                (
                    segment("Outer::B", "B"),
                    list(vec![
                        symbol("b"),
                        Names::Pairs(vec![("c".into(), list(Vec::new()))].into()),
                    ]),
                ),
                (
                    span("D"),
                    Names::Pairs(vec![("d".into(), Names::Pairs(Box::default()))].into()),
                ),
                (segment("Outer::E", "E"), list(vec![symbol("e")])),
            ])
        );
        let rewritten = |name: &str| {
            let at = source.rfind(&format!("{name} ")).unwrap() as u32;
            (at, at + name.len() as u32)
        };
        assert_eq!(
            found.rewritten,
            HashSet::from([
                rewritten("A"),
                last("Outer::B", "B"),
                rewritten("D"),
                last("Outer::E", "E"),
            ])
        );
    }

    /// A symbol literal's own name is read, and an interpolated one has none.
    #[test]
    fn a_symbol_literal_carries_its_name() {
        let named = |source| match receiver(source) {
            Receiver::Literal { symbol, .. } => symbol,
            other => panic!("not a literal: {other:?}"),
        };
        assert_eq!(named(":name.~").as_deref(), Some("name"));
        assert_eq!(named(r#":"quoted".~"#).as_deref(), Some("quoted"));
        assert_eq!(named(r#":"a#{b}".~"#), None);
        assert_eq!(named(r#""name".~"#), None);
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
        assert_eq!(receiver("Person.new.~"), built(6));
        assert_eq!(receiver("HR::Person.new.~"), built(10));
        // What `new` was passed rides along, for the `initialize` the object runs.
        assert_eq!(
            receiver("Person.new(1, 2, id: \"x\").~"),
            Receiver::Instance {
                at: 6,
                arity: Arity::Keyed(2),
                arguments: vec![Receiver::literal("Integer"), Receiver::literal("Integer")],
                keywords: Some(vec![("id".to_owned(), Receiver::literal("String"))]),
            }
        );
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
                keywords: Some(Vec::new()),
                safe: false,
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
                keywords: Some(Vec::new()),
                safe: false,
            }
        );
    }

    #[test]
    fn a_multiple_assignment_gives_each_target_the_position_it_was_written_at() {
        // One value spread across several names. The shape must carry *which* name, because the
        // type is a position in the value, not the value.
        assert_eq!(
            typed("read_io, write_io = IO.pipe\nwrite_io.~"),
            Receiver::Destructured {
                of: Box::new(Receiver::Returned {
                    on: Box::new(Receiver::Constant(22)),
                    method: "pipe".to_owned(),
                    block: Block::None,
                    arity: Arity::Exactly(0),
                    arguments: Vec::new(),
                    keywords: Some(Vec::new()),
                    safe: false,
                }),
                index: 1,
            }
        );

        // **A written list is not a destructure.** `a, b = foo, bar` gives each name its own
        // element, exactly, so no index travels and the shape is what `b = bar` would give.
        assert_eq!(typed("a, b = Person.new, Widget.new\nb.~"), built(25));

        // A `*rest` fixes no position after it, so the target is a write nothing can read, and
        // the read refuses rather than guessing a position.
        assert_eq!(typed("head, *rest = IO.pipe\nhead.~"), Receiver::Unknown);

        // **A non-local target is not this local's write; its neighbours are.** `@held` is an
        // instance variable, a write of its own.
        assert_eq!(
            typed("kept, @held = IO.pipe\nkept.~"),
            Receiver::Destructured {
                of: Box::new(Receiver::Returned {
                    on: Box::new(Receiver::Constant(16)),
                    method: "pipe".to_owned(),
                    block: Block::None,
                    arity: Arity::Exactly(0),
                    arguments: Vec::new(),
                    keywords: Some(Vec::new()),
                    safe: false,
                }),
                index: 0,
            }
        );
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
        // Keywords are not positional, and RBS counts them apart too: `3.7.round(half: :up)`
        // reaches the zero-argument arm that takes keywords. That a hash was written is kept: an
        // arm taking no keywords gets it as one more positional.
        assert_eq!(arity("3.7.round(half: :up).~"), Arity::Keyed(0));
        assert_eq!(arity("3.7.round(1, half: :up).~"), Arity::Keyed(1));
        // `**opts` is the same fact. Prism reads a bare `k => v` tail as keywords whatever the keys
        // (Ruby 3's rule). Braces make it a positional `Hash`, which counts.
        assert_eq!(arity("f.g(**opts).~"), Arity::Spread(0));
        assert_eq!(arity("f.g(1, **a, **b).~"), Arity::Spread(1));
        assert_eq!(arity("f.g(**opts, a: 1).~"), Arity::Keyed(0));
        assert_eq!(arity("f.g(\"a\" => 1).~"), Arity::Keyed(0));
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
                keywords: Some(Vec::new()),
                safe: false,
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
                    keywords: Some(Vec::new()),
                    safe: false,
                }),
                method: "strip".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
                keywords: Some(Vec::new()),
                safe: false,
            }
        );
    }

    #[test]
    fn an_argument_list_past_the_bound_is_no_claim_rather_than_a_short_one() {
        /// The arity, positionals and keywords a call wrote.
        type Written = (Arity, Vec<Receiver>, Option<Vec<(String, Receiver)>>);
        /// What `f.g(list)` wrote.
        fn written(list: &str) -> Written {
            let Receiver::Returned {
                arity,
                arguments,
                keywords,
                ..
            } = receiver(&format!("f.g({list}).~"))
            else {
                panic!("a chain");
            };
            (arity, arguments, keywords)
        }
        let listed = |count: usize, spell: fn(usize) -> String| {
            (1..=count).map(spell).collect::<Vec<_>>().join(", ")
        };
        // Real code writes long lists: 89 positionals and 132 keywords in one call. Both are read
        // whole, well past the walk's own width.
        let (arity, arguments, _) = written(&listed(MAX_WIDTH + 1, |n| n.to_string()));
        assert_eq!(arity, Arity::Exactly(MAX_WIDTH as u32 + 1));
        assert_eq!(arguments.len(), MAX_WIDTH + 1);
        let (_, _, keywords) = written(&listed(132, |n| format!("k{n}: {n}")));
        assert_eq!(keywords.map(|found| found.len()), Some(132));
        // One shape per counted argument or nothing, never a prefix: positions after a dropped
        // argument would be miscounted, and the consumer needs the whole list to pick an arm. So
        // past the bound the count stays exact and the shapes go.
        let (arity, arguments, _) = written(&listed(MAX_ARGUMENTS + 1, |n| n.to_string()));
        assert_eq!(arity, Arity::Exactly(MAX_ARGUMENTS as u32 + 1));
        assert!(arguments.is_empty(), "{arguments:?}");
        let (_, _, keywords) = written(&listed(MAX_ARGUMENTS + 1, |n| format!("k{n}: {n}")));
        assert_eq!(keywords, None);
    }

    #[test]
    fn one_unknown_ends_the_chain_rather_than_being_carried_up_it() {
        // A half-typed `x.` has no name, so there is nothing to look up, and a `Returned` around
        // an `Unknown` would only make the graph side rediscover that. (`x.()` is `x.call`.)
        assert_eq!(receiver("x.\n.foo.~"), Receiver::Unknown);
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
                    keywords: Some(Vec::new()),
                    safe: false,
                }),
                method: "foo".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
                keywords: Some(Vec::new()),
                safe: false,
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
                keywords: Some(Vec::new()),
                safe: false,
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
                    keywords: Some(Vec::new()),
                    safe: false,
                }),
                method: "bar".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
                keywords: Some(Vec::new()),
                safe: false,
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
        // **A `next` is one more value of the block, and a `break` a value of the call**.
        // A bare one is `nil`; one in a nested block or a loop is that construct's.
        assert_eq!(
            block("[1].map { |n| next 1 if n\n \"x\" }.~"),
            Block::Written(Box::new([
                Receiver::literal("String"),
                Receiver::literal("Integer")
            ]))
        );
        assert_eq!(
            block("[1].each { |n| break if n\n \"x\" }.~"),
            Block::Breaking(Box::new(Breaks {
                exits: Box::new([Receiver::literal("String")]),
                breaks: Box::new([Receiver::literal("NilClass")]),
            }))
        );
        assert_eq!(
            block("[1].map { |n| [2].each { next 1 }\n while n do break end\n \"x\" }.~"),
            Block::Written(Box::new([Receiver::literal("String")]))
        );
        // In tail position a `next`, a `break` and a `return` hand back nothing themselves: the
        // first two are counted above, and a `return` leaves the enclosing method.
        assert_eq!(
            block("[1].map { |n| n ? next(1) : return }.~"),
            Block::Written(Box::new([Receiver::literal("Integer")]))
        );
        // `next a, b` hands back an `Array` no shape here says, and `redo` loops: unreadable.
        assert_eq!(
            block("[1].map { |n| next 1, 2 }.~"),
            Block::Written(Box::new([Receiver::Unknown]))
        );
        assert_eq!(
            block("[1].map { |n| redo }.~"),
            Block::Written(Box::new([Receiver::Unknown]))
        );
        // `&:upcase` is the method on what the block is handed, and `&value` a proc passed as the
        // block, kept as its shape: the arm applies either way.
        assert_eq!(block("[1].map(&:to_s).~"), Block::Symbol("to_s".to_owned()));
        assert!(matches!(block("[1].map(&blk).~"), Block::Passed(_)));
        // No block at all.
        assert_eq!(block("[1].first.~"), Block::None);
    }

    #[test]
    fn a_local_assigned_a_call_carries_the_call() {
        // A write's shape goes beyond literals and `.new`: it is whatever shape the assigned
        // expression has.
        assert_eq!(
            typed("shouted = \"hi\".upcase\nshouted.~\n"),
            Receiver::Returned {
                on: Box::new(Receiver::literal("String")),
                method: "upcase".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
                keywords: Some(Vec::new()),
                safe: false,
            }
        );
    }

    #[test]
    fn a_local_assigned_from_itself_terminates() {
        // `x = x.foo` reads a local inside its own write, which cannot answer that read: a write
        // takes effect where its value ends. The inner read is a row of its own, so the shape is
        // one reference deep, not a copy of everything above it.
        let call = |at: u32, method: &str| Receiver::Returned {
            on: Box::new(read_of(at, "x")),
            method: method.to_owned(),
            block: Block::None,
            arity: Arity::Exactly(0),
            arguments: Vec::new(),
            keywords: Some(Vec::new()),
            safe: false,
        };
        assert_eq!(typed("x = x.foo\nx.~"), call(4, "foo"));
        assert_eq!(typed("x = \"hi\"\nx = x.upcase\nx.~"), call(13, "upcase"));
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
        // (a half-typed `foo.`) is named nothing, and stops here. `handler.()` has no message span
        // either, but it is `handler.call()` written short, and its parenthesis says so.
        assert_eq!(named("handler.().~\n"), "call/Exactly(0)");
        assert_eq!(receiver("(handler.\n).~"), Receiver::Unknown);
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
        // A guard, not a limit real code meets: the longest chain the reference corpora write is
        // 24 links, and it is followed to its root.
        let written = format!("\"hi\"{}.~", ".upcase".repeat(24));
        let mut at = &receiver(&written);
        while let Receiver::Returned { on, .. } = at {
            at = on;
        }
        assert_eq!(*at, Receiver::literal("String"));
    }

    #[test]
    fn a_local_takes_the_type_of_what_was_assigned_to_it() {
        assert_eq!(
            typed("name = \"ada\"\nname.~\n"),
            Receiver::literal("String")
        );
        assert_eq!(typed("person = Person.new\nperson.~\n"), built(15));
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
        // The write's value is a lookup (`self.compute`), and the read keeps the local's own
        // spelling, tried last, so a workspace with no `compute` gives the name-rung answer.
        assert_eq!(receiver("x = compute\nx.~\n"), read_of(12, "x"));
        assert_eq!(typed("x = compute\nx.~\n"), self_call(4, "compute"));
        // Never assigned: a bare word, which Prism reads as a receiverless call, so it has the
        // *call's* shape with the same spelling underneath.
        assert_eq!(receiver("x.~\n"), self_call(0, "x"));
        // `x = x.` reads `x` before its own write has run. Ruby holds `nil` there: the local
        // exists from the moment Prism parses the write, and nothing wrote it yet.
        assert_eq!(reached("x = x.~\n"), (Vec::new(), true, None));
    }

    #[test]
    fn a_typed_variable_still_carries_the_name_it_is_written_as() {
        // The wrapper every other test looks past, pinned once. Both halves must be present: the
        // read, so a typing chain is answered from the code; and the spelling, so a failing chain
        // reaches the same rung a bare name does.
        let source = "story = Story.where(x).first\nstory.~\n";
        assert_eq!(receiver(source), read_of(29, "story"));
        assert_eq!(
            typed(source),
            Receiver::Returned {
                on: Box::new(Receiver::Returned {
                    on: Box::new(Receiver::Constant(13)),
                    method: "where".to_owned(),
                    block: Block::None,
                    arity: Arity::Exactly(1),
                    // The argument is a shape like any other: a bare `x` is a receiverless call
                    // here as everywhere.
                    arguments: vec![self_call(20, "x")],
                    keywords: Some(Vec::new()),
                    safe: false,
                }),
                method: "first".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
                keywords: Some(Vec::new()),
                safe: false,
            }
        );
        // An instance variable is read the same way, and each of its writes names its line.
        let source = "class C\n  def a\n    @story = fetch.first\n  end\n  def b\n    @story.~\n  end\nend\n";
        let read = source.find("@story.~").unwrap() as u32;
        assert_eq!(receiver(source), read_of(read, "@story"));
        assert_eq!(
            reached(source),
            (
                vec![written(
                    20,
                    Receiver::Returned {
                        on: Box::new(self_call(29, "fetch")),
                        method: "first".to_owned(),
                        block: Block::None,
                        arity: Arity::Exactly(0),
                        arguments: Vec::new(),
                        keywords: Some(Vec::new()),
                        safe: false,
                    }
                )],
                true,
                None
            )
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
        // The other half-typed shape: a word already begun. Nothing is blanked: the word is the
        // message, so the `end` below still closes the method.
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
        let found = row(
            "class Person\n  def self.build\n    @seed = \"x\"\n  end\n\n  def shout\n    @seed.~\n  end\nend\n",
        )
        .expect("a row for the read");
        assert!(
            found.writes.is_empty(),
            "the singleton's assignment is not this variable's: {found:?}"
        );
        assert_eq!(
            found.instance.map(|read| read.level),
            Some(Some(0)),
            "an instance's, for the type side to find in the object's other classes"
        );
    }

    #[test]
    fn two_assignments_of_different_classes_both_reach() {
        // Methods run in any order, so both writes reach `c`'s read, and `nil` does too: nothing
        // sets `@v` before `c` can run. The old rule answered with the last one (#12).
        assert_eq!(
            reached(
                "class Person\n  def a\n    @v = \"s\"\n  end\n  def b\n    @v = 1\n  end\n  def c\n    @v.~\n  end\nend\n",
            ),
            (
                vec![
                    written(25, Receiver::literal("String")),
                    written(52, integer())
                ],
                true,
                None
            )
        );
    }

    #[test]
    fn a_memoised_instance_variable_is_an_assignment() {
        // `@cache ||= …` is Ruby's memoisation and as much an assignment as `=`. `nil` reaches too:
        // nothing sets `@cache` before `use` can run.
        let source = "class C\n  def cache\n    @cache ||= \"x\"\n  end\n  def use\n    @cache.~\n  end\nend\n";
        let at = source.find("@cache").unwrap() as u32;
        assert_eq!(
            reached(source),
            (vec![written(at, Receiver::literal("String"))], true, None)
        );
        // `@n += 1` is a write too: the old value's `+`, read where the name is written, like any
        // read.
        let source = "class C\n  def bump\n    @n += 1\n  end\n  def use\n    @n.~\n  end\nend\n";
        let at = source.find("@n").unwrap() as u32;
        assert_eq!(
            reached(source),
            (
                vec![written(
                    at,
                    Receiver::Returned {
                        on: Box::new(Receiver::Variable(at)),
                        method: "+".to_owned(),
                        block: Block::None,
                        arity: Arity::Exactly(1),
                        arguments: vec![integer()],
                        keywords: Some(Vec::new()),
                        safe: false,
                    }
                )],
                true,
                None
            )
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
            Some(built((start + found.len()) as u32))
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
        // `@v = @v.foo` reads the variable inside its own write. The write reaches that read like
        // every write of `@v` reaches every read, and the table stops at the reference: settling
        // the cycle is `types`' rounds, not a copy per turn.
        let source = "class C\n  def a\n    @v = @v.~\n  end\nend\n";
        let read = source.rfind("@v").unwrap() as u32;
        let write = source.find("@v").unwrap() as u32;
        assert_eq!(
            reached(source),
            (vec![written(write, read_of(read, "@v"))], true, None)
        );
    }

    #[test]
    fn an_instance_variable_assigned_from_itself_many_times_is_one_entry_per_write() {
        // The shape of a real filter class, which assigns `@scope` sixty times,
        // mostly from itself. Every write reaches every read, and each self-referencing write is a
        // reference to a read, not a copy of the writes above it. `initialize` writes the first as
        // a statement of its own body, so `nil` does not reach.
        let mut source = String::from("class Person\n  def initialize\n    @name = \"ada\"\n");
        for _ in 0..12 {
            source.push_str("    @name = @name.strip\n");
        }
        source.push_str("  end\n\n  def shout\n    @name.~\n  end\nend\n");
        let (writes, nil, bound) = reached(&source);
        assert_eq!(writes.len(), 13);
        assert!(
            matches!(&writes[0], Receiver::Assigned { was, .. } if **was == Receiver::literal("String"))
        );
        assert!(!nil && bound.is_none());
    }

    #[test]
    fn a_constant_handed_on_is_one_written_as_a_value() {
        let source = "\
class Uploader < Base
  include Helpers
  mount_uploader :cover, CoverUploader
  X = Admin::Thing
  Other.new(1)
  y.is_a?(Kind)
  begin
  rescue Oops => e
  end
  case z
  when Shape then 1
  end
end
render(obj, serializer: Ns::AccountSerializer)
(a ? Picked : (b || Fallback)).new(1)

def build
  klass = Built
  klass.new(1)
  klass.name
  passed = Passed
  passed.new(1)
  register(passed)
  kept = Kept
end

def other
  klass.call
  register(klass)
end
";
        let mut handed: Vec<&str> = constants_handed_on(source)
            .into_iter()
            .map(|(start, end)| &source[start as usize..end as usize])
            .collect();
        handed.sort_unstable();
        assert_eq!(
            handed,
            ["AccountSerializer", "CoverUploader", "Passed", "Thing"]
        );
    }

    #[test]
    fn a_super_passes_what_it_writes_or_the_def_s_own_parameters() {
        let source = "\
class A
  def initialize(object, opts = {}, key: 1, need:)
    super
  end

  def b(x)
    super(x.to_s, y: 2)
  end

  def c(x)
    x = 1
    super
  end

  def d(*rest)
    super
  end

  def k(**opts)
    super
  end

  def e
    [1].each { super }
    def inner(z) = super
  end
end
define_method(:f) { super }
define_method(:g) { super(1) }
";
        let at = |needle: &str| source.find(needle).expect(needle) as u32;
        let parameter = |def: &str, slot: ParameterSlot| Receiver::Parameter {
            at: at(&format!("def {def}")),
            method: def.to_owned(),
            slot,
        };
        type Passed = (u32, Arity, Vec<Receiver>, Option<Vec<(String, Receiver)>>);
        let found: Vec<Passed> = supers(source)
            .into_iter()
            .map(|site| {
                assert!(site.def.0 < site.at && site.at < site.def.1, "{site:?}");
                (site.at, site.arity, site.arguments, site.keywords)
            })
            .collect();
        let on_x = |shape: &Receiver| {
            matches!(shape, Receiver::Returned { method, on, .. }
                if method == "to_s"
                    && matches!(&**on, Receiver::Spelled { was, .. }
                        if matches!(**was, Receiver::Variable(_))))
        };
        assert_eq!(found.len(), 7, "{found:#?}");
        // A bare `super` passes each parameter, positionals and keywords alike.
        assert_eq!(
            found[0],
            (
                at("super\n  end"),
                Arity::Keyed(2),
                vec![
                    parameter("initialize", ParameterSlot::Positional(0)),
                    parameter("initialize", ParameterSlot::Positional(1)),
                ],
                Some(vec![
                    (
                        "key".to_owned(),
                        parameter("initialize", ParameterSlot::Keyword("key".to_owned()))
                    ),
                    (
                        "need".to_owned(),
                        parameter("initialize", ParameterSlot::Keyword("need".to_owned()))
                    ),
                ]),
            )
        );
        // `super(…)` passes what it writes.
        assert_eq!(found[1].1, Arity::Keyed(1));
        assert!(on_x(&found[1].2[0]), "{:?}", found[1].2);
        assert!(matches!(&found[1].3.as_deref(), Some([(key, _)]) if key == "y"));
        // A parameter written before may hold anything; a rest passes a count nothing knows.
        assert_eq!(found[2].1, Arity::Exactly(1));
        assert_eq!(found[2].2, vec![Receiver::Unknown]);
        assert_eq!((found[3].1, found[3].2.len()), (Arity::Unknown, 0));
        assert_eq!((found[4].1, found[4].3.clone()), (Arity::Unknown, None));
        // A block's `super` is the `def`'s around it; a nested `def`'s is its own; one outside
        // every `def` is no site.
        assert_eq!((found[5].1, found[5].2.len()), (Arity::Exactly(0), 0));
        assert_eq!(
            found[6].2,
            vec![parameter("inner", ParameterSlot::Positional(0))]
        );
    }

    #[test]
    fn a_literal_spelling_a_name_is_read_where_it_is_written() {
        // An argument names its call, the keyword it sits under and, as the first positional, where
        // the call's name starts; an array's words and a hash's values are its arguments too. A
        // `when`, a hash's key, an `alias` and an `undef` compare or name; a block pass is its own;
        // anything else is held. A part of an interpolation, a sentence and a constant path are no
        // names.
        let source = "\
send(:a, 'b')
before_action :c, only: %i[d], if: { e: :f }
items.map(&:g)
case x
when :h then 1
end
{ :i => :j }
alias k l
undef m
HANDLERS = [:n, \"two words\", \"o#{p}\", :\"q?\"]
call(**rest, r: :s, &)
[:_t, \"9u\"]
";
        let uses = spelled_uses(source);
        let how = |name: &str| -> Vec<Spelling> {
            uses.get(name)
                .map(|found| found.iter().map(|one| one.how.clone()).collect())
                .unwrap_or_default()
        };
        let argument = |call: &str, key: Option<&str>, first: Option<u32>| Spelling::Argument {
            call: call.to_owned(),
            key: key.map(str::to_owned),
            first,
        };
        assert_eq!(how("a"), [argument("send", None, Some(0))]);
        assert_eq!(how("b"), [argument("send", None, None)]);
        let before = source.find("before_action").unwrap() as u32;
        assert_eq!(how("c"), [argument("before_action", None, Some(before))]);
        assert_eq!(how("d"), [argument("before_action", Some("only"), None)]);
        assert_eq!(how("e"), [Spelling::Compared]);
        assert_eq!(how("f"), [argument("before_action", Some("e"), None)]);
        assert_eq!(how("g"), [Spelling::BlockPass]);
        assert_eq!(how("h"), [Spelling::Compared]);
        assert_eq!(how("i"), [Spelling::Compared]);
        assert_eq!(how("j"), [Spelling::Held]);
        assert_eq!(how("k"), [Spelling::Compared]);
        assert_eq!(how("l"), [Spelling::Compared]);
        assert_eq!(how("m"), [Spelling::Compared]);
        assert_eq!(how("n"), [Spelling::Held]);
        assert_eq!(how("q?"), [Spelling::Held]);
        assert!(how("o").is_empty() && how("p").is_empty());
        assert_eq!(how("s"), [argument("call", Some("r"), None)]);
        assert_eq!(how("_t"), [Spelling::Held]);
        assert!(!uses.contains_key("9u"));
        assert!(!uses.contains_key("two words"));
        assert_eq!(uses.get("a").unwrap()[0].at, 5);
        assert!(uses.get("b").unwrap()[0].text && !uses.get("a").unwrap()[0].text);
    }

    #[test]
    fn a_hook_mixes_in_what_it_hands_its_one_parameter() {
        // Only `self.included`, `self.extended` and `self.prepended` with one plain parameter, and
        // only a mixin called on that parameter, directly or through a sender, outside a nested
        // `def`. Each constant and `self` it is handed is one row; anything else is not read.
        let source = "\
module Hooked
  def self.included(base)
    base.extend(ClassMethods, self)
    base.send(:include, Tools::Kit)
    base.public_send(:define_method, :x) { }
    base.include(mixin)
    other.include(Elsewhere)
    def helper(base) = base.include(Nested)
  end

  def self.extended(object) = object.prepend(Front)
  def self.prepended(base, extra) = base.extend(Two)
  def self.included(*bases) = bases.first.extend(Splat)
  def self.extended(base = nil) = base.extend(Optional)
  def self.included(base, *rest) = base.extend(Rest)
  def self.included(base, after) = base.extend(Posts)
  def self.included(base, key:) = base.extend(Keyword)
  def self.included(base, **options) = base.extend(Options)
  def self.included = extend(Bare)
  def included(base) = base.extend(Instance)
  def self.inherited(base) = base.extend(Other)
end
";
        let found = hook_mixins(source);
        let at = |needle: &str| source.find(needle).unwrap() as u32;
        let end = |needle: &str| (source.find(needle).unwrap() + needle.len()) as u32;
        let row = |hook: &str, mixer: &str, mixed: Option<u32>, def: &str| HookMixin {
            hook_at: at(def),
            hook: hook.to_owned(),
            mixer: mixer.to_owned(),
            mixed,
        };
        assert_eq!(
            found,
            [
                row(
                    "included",
                    "extend",
                    Some(end("ClassMethods")),
                    "def self.included(base)"
                ),
                row("included", "extend", None, "def self.included(base)"),
                row(
                    "included",
                    "include",
                    Some(end("Tools::Kit")),
                    "def self.included(base)"
                ),
                row(
                    "extended",
                    "prepend",
                    Some(end("Front")),
                    "def self.extended(object)"
                ),
            ]
        );
    }

    #[test]
    fn a_text_s_calls_and_walk_from_one_parse_are_what_each_reads_alone() {
        let source = "class Shelf\n  def stack(books, by: :title)\n    sorted = books.sort_by { |b| b.public_send(by) }\n    @top = sorted.first&.title\n    yield sorted if block_given?\n    sorted\n  end\nend\nShelf.new.stack([], by: :year) { |s| s.size }\n";
        let (calls, shapes) = every_call_and_shapes(source);
        assert!(!calls.is_empty());
        assert_eq!(calls, every_call(source));
        assert_eq!(shapes, super::shapes(source));
    }

    #[test]
    fn every_call_that_sends_a_method_by_name_is_recorded() {
        // `send` and its kin, with the names each can send and how many values
        // follow. A local's name is what every write of it in its `def` spells; a `def` inside
        // keeps its own. Only a name that can end in `=` with one value may call a writer.
        let source = "\
class Form
  def assign(key, value, *rest)
    public_send(:\"#{key}=\", value)
    send(:name=, value)
    __send__(\"title=\", value)
    try(:name, value)
    send(:name=, value, value)
    send(:name=, *rest)
    public_send(*rest)
    record.public_send(key, value)
    self.try!(:x=, value)
    setter = :\"#{key}=\"
    send(setter, value)
    other = \"a=\"
    other = \"b=\"
    send(other, value)
    mixed = \"a=\"
    mixed = key
    send(mixed, value)
    held ||= \"x=\"
    send(held, value)
    anded = \"a=\"
    anded &&= \"b=\"
    send(anded, value)
    summed = \"a\"
    summed += \"=\"
    send(summed, value)
    targeted, _ = rest
    send(targeted, value)
    same = \"c=\"
    same = \"c=\"
    send(same, value)
    plain = :d=
    send(plain, value)
    built = \"#{key}=\"
    send(built, value, *rest)
    send(unwritten, value)
    try!(:\"#{key}_id\", value)
    send
  end

  def forward(...)
    send(...)
  end

  def outer
    name = \"a=\"
    def inner
      name = \"b\"
    end
    send(name, 1)
  end
end
send(:top=, 1)
";
        let shown = |name: &scopes::Spelled| match name {
            scopes::Spelled::Exactly(exactly) => exactly.clone(),
            scopes::Spelled::Like { head, tail } => format!("{head}*{tail}"),
            scopes::Spelled::Argument { .. } => "argument".to_owned(),
        };
        let sends: Vec<(String, Option<usize>, bool, bool)> = shapes(source)
            .sends
            .iter()
            .map(|sent| {
                (
                    shown(&sent.name),
                    sent.values,
                    matches!(sent.on, Receiver::SelfObject(_)),
                    sent.may_write(),
                )
            })
            .collect();
        let expected: Vec<(String, Option<usize>, bool, bool)> = [
            ("*=", Some(1), true, true),
            ("name=", Some(1), true, true),
            ("title=", Some(1), true, true),
            ("name", Some(1), true, false),
            ("name=", Some(2), true, false),
            ("name=", None, true, true),
            ("*", None, true, true),
            ("*", Some(1), false, true),
            ("x=", Some(1), true, true),
            ("*=", Some(1), true, true),
            ("*=", Some(1), true, true),
            ("*", Some(1), true, true),
            ("*", Some(1), true, true),
            ("*", Some(1), true, true),
            ("*", Some(1), true, true),
            ("*", Some(1), true, true),
            ("c=", Some(1), true, true),
            ("d=", Some(1), true, true),
            ("*=", None, true, true),
            ("*", Some(1), true, true),
            ("*_id", Some(1), true, false),
            ("*", None, true, true),
            ("a=", Some(1), true, true),
            ("top=", Some(1), true, true),
        ]
        .into_iter()
        .map(|(name, values, own, writes)| (name.to_owned(), values, own, writes))
        .collect();
        assert_eq!(sends, expected);
    }

    #[test]
    fn a_set_call_writes_each_of_its_keywords() {
        // `set(name: value)` on a receiver writes `name`; a double splat, a hash of
        // strings or anything else is any name, `**`. A receiverless `set` is no one's.
        let source = "\
Current.set(account: 1, user: 2) { }
Current.set({ lane: 3 })
Current.set(**options)
Current.set(\"x\" => 4)
Current.set(5)
set(skipped: 6)
Current.account = 7
";
        let setters: Vec<(String, bool)> = shapes(source)
            .setters
            .iter()
            .map(|setter| (setter.name.clone(), setter.set))
            .collect();
        let expected: Vec<(String, bool)> = [
            ("account", true),
            ("user", true),
            ("lane", true),
            ("**", true),
            ("**", true),
            ("account", false),
        ]
        .into_iter()
        .map(|(name, set)| (name.to_owned(), set))
        .collect();
        assert_eq!(setters, expected);
    }

    #[test]
    fn every_construct_that_may_run_a_method_ends_where_it_runs() {
        // Each construct below runs a method, recorded where it (or its splat) ends; a
        // literal, a lambda and a variable write run none.
        let constructs = [
            "a.b",
            "c",
            "d.e += 1",
            "d.e &&= 1",
            "d.e ||= 1",
            "f[1] += 1",
            "f[1] &&= 1",
            "f[1] ||= 1",
            "@g += 1",
            "h += 1",
            "@@i += 1",
            "$j += 1",
            "K += 1",
            "L::M += 1",
            "super",
            "super(1)",
            "yield",
            "\"#{1}\"",
            ":\"#{1}\"",
            "/#{1}/",
            "`#{1}`",
            "`ls`",
            "/(?<n>.)/ =~ s",
            "1 in Integer",
            "1 => Integer",
            "case 1\nwhen 2 then 3\nend",
            "case 1\nin 2 then 3\nend",
            "for q in [] do end",
            "r, s = 1, 2",
            "[*t]",
            "{ **u }",
            "w(&x)",
            "(1..2)",
            "def y; end",
            "alias z y",
            "undef z",
            "class Kls; end",
            "module Mod; end",
            "class << self; end",
            "N = 1",
            "O::P = 1",
        ];
        for construct in constructs {
            let source = format!("{construct}\n");
            let parsed = parse(&source);
            let ends = call_ends(&parsed.node()).ends;
            assert!(
                !ends.is_empty() && ends.iter().all(|end| *end <= construct.len() as u32),
                "{construct}: {ends:?}"
            );
        }
        for quiet in ["1", "@a = 1", "b = [1, 2]", "c = { d: 1 }", "-> {}", "@e"] {
            let source = format!("{quiet}\n");
            assert!(call_ends(&parse(&source).node()).ends.is_empty(), "{quiet}");
        }
        // Every modifier's body and condition; a written-out `if` is in order already.
        let source = "a.b if c?\nd if @e\nif f?\n  g\nend\nh while i?\n";
        let modifiers = call_ends(&parse(source).node()).modifiers;
        let span = |text: &str| {
            let start = source.find(text).expect("in the fixture") as u32;
            (start, start + text.len() as u32)
        };
        assert_eq!(
            modifiers,
            [
                (span("a.b"), span("c?")),
                (span("d"), span("@e")),
                (span("h"), span("i?"))
            ]
        );
    }

    #[test]
    fn a_def_s_opening_writes_stop_at_a_statement_that_may_return() {
        // What a receiverless `def` surely writes before anything can leave it. A
        // nested `def`'s `return` is its own; a block's leaves the method.
        let source = "\
class K
  def initialize(skip)
    @a = 1
    def inner
      return 2
    end
    @b = 2
    [1].each { |x| return x }
    @c = 3
  end

  def set
    @d = 1
    @e ||= 2
    return if @d
    @f = 3
  end

  def self.other
    @g = 1
  end
end
";
        let table = shapes(source).variables;
        let at = |text: &str| source.find(text).expect("in the fixture") as u32;
        assert_eq!(
            table
                .initializer(at("def initialize"))
                .map(|found| found.writes.clone()),
            Some(vec!["@a".to_owned(), "@b".to_owned()])
        );
        assert_eq!(
            table.opening(at("def set")),
            Some(&["@d".to_owned(), "@e".to_owned()][..])
        );
        assert_eq!(table.opening(at("def inner")), Some(&[][..]));
        assert_eq!(table.opening(at("def self.other")), None);
        assert_eq!(table.opening(at("def initialize")), None);
    }

    #[test]
    fn every_instance_write_is_indexed_for_a_read_in_another_text() {
        // What a read in another file of the object's classes asks: every write to one name, and
        // at which level. A setter is a write nothing types; a block straight in the class body
        // may run at either level; `main`'s variables belong to no class.
        let source = "\
class K
  attr_writer :name
  before_action { @story = 1 }

  def initialize
    super
    @seed = 1
  end

  def a
    @seed = 2
  end
end
@top = 1
";
        let at = |text: &str| source.find(text).expect("in the fixture") as u32;
        let table = shapes(source).variables;
        let indexed = |name: &str| -> Vec<(u32, Option<i32>)> {
            table
                .instance_writes(name)
                .iter()
                .map(|write| (table.assignment(write.write).unwrap().at, write.level))
                .collect()
        };
        assert_eq!(
            indexed("@seed"),
            vec![(at("@seed = 1"), Some(0)), (at("@seed = 2"), Some(0))]
        );
        assert_eq!(indexed("@story"), vec![(at("@story"), None)]);
        // A setter's write sits at its name, past the colon, where rubydex files the method.
        assert_eq!(indexed("@name"), vec![(at(":name") + 1, Some(0))]);
        assert_eq!(
            table
                .assignment(table.instance_writes("@name")[0].write)
                .unwrap()
                .shape,
            Receiver::Unknown
        );
        assert!(indexed("@top").is_empty());

        // `initialize` writes `@seed` as a statement of its own body, after `super`.
        assert_eq!(
            table.initializer(at("def initialize")),
            Some(&Initializer {
                writes: vec!["@seed".to_owned()],
                supers: true,
            })
        );
        assert_eq!(table.initializer(at("def a")), None);
    }

    #[test]
    fn a_read_whose_self_is_not_known_is_refused() {
        // `after_action { }` may run on an instance or on the class, so no write can be said to
        // reach its `@story`.
        assert_eq!(
            reached(
                "class K\n  after_action { @story.~ }\n\n  def a\n    @story = 1\n  end\nend\n"
            ),
            (vec![Receiver::Unknown], true, None)
        );
    }

    #[test]
    fn an_instance_variable_nothing_assigned_keeps_only_its_name() {
        // Nothing in this text writes it: an empty row, which says whose variable it is so the type
        // side can ask the object's other classes.
        assert_eq!(
            row("class C\n  def a\n    @v.~\n  end\nend\n"),
            Some(Reaching {
                writes: Rc::from([]),
                nil: true,
                bound: None,
                instance: Some(Box::new(InstanceRead {
                    name: "@v".to_owned(),
                    level: Some(0),
                    set_here: false,
                    loose: false,
                    method: Some("a".to_owned()),
                    decided: None,
                })),
                narrowed: Box::default(),
                kept: false,
            })
        );
        // Assigned a receiverless call, which is `self.compute`, and every older rung is still
        // underneath in order: the call's own name, then (when `method_receiver` returns nothing
        // for the read) the variable's, which the guess uses.
        assert_eq!(
            reached("class C\n  def a\n    @v = compute\n  end\n  def b\n    @v.~\n  end\nend\n"),
            (vec![written(20, self_call(25, "compute"))], true, None)
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
        // This module still guesses nothing: the read's spelling is a spelling, not a type.
        // Whether a `p` can be a `P` is for the graph, a rung down, and a setting can turn it off.
        // The `self` rung adds a *lookup* under the read, not a guess.
        assert_eq!(
            context("p = build_person\np.~\n"),
            Context::MethodCall {
                receiver: read_of(17, "p")
            }
        );
        assert_eq!(
            typed("p = build_person\np.~\n"),
            self_call(4, "build_person")
        );
        // Written nowhere in this text, and `main`'s: an empty row with no namespace, which the type
        // side refuses, so the rungs below have only the name.
        assert_eq!(
            context("@person.sh~\n"),
            Context::MethodCall {
                receiver: read_of(0, "@person")
            }
        );
        assert!(
            row("@person.sh~\n").is_some_and(|found| found.writes.is_empty()
                && found.instance.is_some_and(|read| read.level.is_none()))
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
            typed("Story.where(id: 1).each do |story|\n  story.~\nend\n"),
            Receiver::Yielded {
                on: Box::new(Receiver::Returned {
                    on: Box::new(Receiver::Constant(5)),
                    method: "where".to_owned(),
                    block: Block::None,
                    arity: Arity::Keyed(0),
                    arguments: Vec::new(),
                    keywords: Some(vec![("id".to_owned(), Receiver::literal("Integer"))]),
                    safe: false,
                }),
                method: "each".to_owned(),
                index: 0,
                safe: false,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
                keywords: Some(Vec::new()),
                spreads: false,
                default: None,
            }
        );
    }

    #[test]
    fn the_innermost_block_a_name_is_a_parameter_of_wins() {
        // Shadowing a block parameter is legal, and a read in the inner block means the inner one:
        // Prism's `depth` says which, through `scopes`.
        let Receiver::Yielded { on, method, .. } =
            typed("a.each do |x|\n  b.map do |x|\n    x.~\n  end\nend\n")
        else {
            panic!("a yielded receiver");
        };
        assert_eq!(method, "map");
        assert_eq!(*on, self_call(16, "b"));
    }

    #[test]
    fn a_block_on_a_call_with_no_receiver_is_asked_of_self() {
        // `each { |x| }` with no receiver is an implicit `self`, which is a question, not a dead
        // end: the parameter is whatever `self.each` says it yields. Where nothing declares it (a
        // `yield` in the method's own body is not a declaration), the read's spelling is the name
        // rung.
        assert_eq!(
            typed("each do |story|\n  story.~\nend\n"),
            Receiver::Yielded {
                on: Box::new(Receiver::SelfObject(0)),
                method: "each".to_owned(),
                index: 0,
                safe: false,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
                keywords: Some(Vec::new()),
                spreads: false,
                default: None,
            }
        );
    }

    #[test]
    fn a_write_in_a_block_adds_to_the_write_above_the_block() {
        // A real `color_extractor.rb`: `max_distance_color = nil` above the
        // loop, `max_distance_color = color` inside it. The block may not run, or run many times,
        // so both reach: `nil`, or whatever `palette.each` hands its block.
        let source = "best = nil\npalette.each do |color|\n  best = color\nend\nbest.~\n";
        let color = source.find("color\nend").unwrap() as u32;
        assert_eq!(
            reached(source),
            (
                vec![Receiver::literal("NilClass"), read_of(color, "color")],
                false,
                None
            )
        );
    }

    #[test]
    fn a_write_rooted_in_a_call_on_self_is_the_write_that_ran() {
        // A receiverless call may resolve to nothing (`self` in a spec, a rake task or a top-level
        // script is `Object`), and when it does, the read has no answer: the old slots answered
        // with the write above it instead, which had not run (#11).
        assert_eq!(
            typed("x = \"s\"\nx = whatever\nx.~"),
            self_call(12, "whatever")
        );
        assert_eq!(
            typed("x = \"s\"\nx = whatever(1)\nx.~"),
            self_call_with(12, vec![integer()], "whatever")
        );
        // A chain through another variable carries that variable's read, not its writes.
        let source = "l = \"s\"\nc = make(1)\nl = c.rows.last\nl.~";
        let c = source.find("c.rows").unwrap() as u32;
        assert_eq!(
            typed(source),
            Receiver::Returned {
                on: Box::new(Receiver::Returned {
                    on: Box::new(read_of(c, "c")),
                    method: "rows".to_owned(),
                    block: Block::None,
                    arity: Arity::Exactly(0),
                    arguments: Vec::new(),
                    keywords: Some(Vec::new()),
                    safe: false,
                }),
                method: "last".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
                keywords: Some(Vec::new()),
                safe: false,
            }
        );
        // A block's parameter is a variable of the block's own, whatever is spelled the same
        // outside it.
        assert!(matches!(
            typed("u = make(1)\nItem.new.tap do |u|\n  u.~\nend\n"),
            Receiver::Yielded { .. }
        ));
    }

    #[test]
    fn a_name_read_outside_the_block_it_is_a_parameter_of_is_not_one() {
        // A block's parameter is the block's own variable. Outside the block the name is not a
        // local at all, so Prism reads it as a call on `self`.
        assert_eq!(
            context("a.each do |story|\n  story\nend\nstory.~\n"),
            Context::MethodCall {
                receiver: self_call(30, "story")
            }
        );
    }

    #[test]
    fn a_write_in_a_block_kills_the_block_s_parameter() {
        // `story = Person.new` inside the block runs on every path to the read below it, so the
        // parameter's own value no longer reaches.
        assert!(matches!(
            typed("a.each do |story|\n  story = Person.new\n  story.~\nend\n"),
            Receiver::Instance { .. }
        ));
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
    fn a_call_written_with_safe_navigation_says_so_and_the_next_link_does_not() {
        // `a&.m` is `M?` only because of the operator, so the shape must keep it: `types` cannot
        // read it back from anything else. Ruby skips **one** call on `nil`, so the link after it
        // is an ordinary call, and an implicit `self` has no operator at all.
        assert_eq!(
            receiver("\"hi\"&.upcase.strip.~"),
            Receiver::Returned {
                on: Box::new(Receiver::Returned {
                    on: Box::new(Receiver::literal("String")),
                    method: "upcase".to_owned(),
                    block: Block::None,
                    arity: Arity::Exactly(0),
                    arguments: Vec::new(),
                    keywords: Some(Vec::new()),
                    safe: true,
                }),
                method: "strip".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
                keywords: Some(Vec::new()),
                safe: false,
            }
        );
        assert_eq!(
            receiver("upcase&.strip.~"),
            Receiver::Returned {
                on: Box::new(self_call(0, "upcase")),
                method: "strip".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
                keywords: Some(Vec::new()),
                safe: true,
            }
        );
    }

    #[test]
    fn a_trailing_dot_above_an_end_is_still_a_method_call() {
        // Ruby continues an expression across a trailing `.`, so Prism reads the `end` below as the
        // method name and the cursor lands *before* the message.
        assert_eq!(
            context("items.each do |i|\n  i.~\nend\n"),
            Context::MethodCall {
                receiver: read_of(20, "i")
            }
        );
        // `i` is read in the repaired text, where it is still the block's parameter: the shape
        // asking what `items.each` hands its block.
        assert_eq!(
            typed("items.each do |i|\n  i.~\nend\n"),
            Receiver::Yielded {
                on: Box::new(self_call(0, "items")),
                method: "each".to_owned(),
                index: 0,
                safe: false,
                arity: Arity::Exactly(0),
                arguments: Vec::new(),
                keywords: Some(Vec::new()),
                spreads: false,
                default: None,
            }
        );
        // The same repair in a one-line body: the `end` on the cursor's line is taken as the
        // message too, and only a closing word is.
        assert_eq!(
            typed("items.each do |i| i.~ end\n"),
            typed("items.each do |i|\n  i.~\nend\n"),
        );
    }

    #[test]
    fn a_member_written_after_the_dot_leaves_the_receiver_a_variable() {
        // The cursor sits before, inside or after a member the line already writes. Prism reads
        // the line as written, so nothing is repaired: blanking the `.` (or the `.` and the word)
        // would leave `name size`, `name (1)` or `name do … end`, a call to a method `name`, and
        // the local would be gone.
        for marked in [
            "name = \"ada\"\nname.~size\n",
            "name = \"ada\"\nname.si~ze\n",
            "name = \"ada\"\nname.size~\n",
            "name = \"ada\"\nname.center~(1)\n",
            "name = \"ada\"\nname.each_char~ do |c|\n  c\nend\n",
            "name = \"ada\"\nname.each_char~ { |c| c }\n",
            "name = \"ada\"\nfoo(name.~size)\n",
            "name = \"ada\"\nname.~ size\n",
            "def go\n  name = \"ada\"\n  name.~size\nend\n",
        ] {
            assert_eq!(typed(marked), Receiver::literal("String"), "at {marked:?}");
            assert_eq!(
                at_marker(marked).expect("a cursor").0.repaired,
                None,
                "nothing to repair at {marked:?}"
            );
        }
        // A trailing-dot chain: the message is on the next line, so the `.` is blanked, and the
        // receiver still ends its own line.
        assert_eq!(
            typed("name = \"ada\"\nname.~\n  size\n"),
            Receiver::literal("String")
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
        macro_symbol(&Parsed::new(&source), offset).map_or_else(
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
        let result = parse(&source);
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
            at(&Parsed::new(&marked.replace('~', "")), offset).expect("a cursor")
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
        assert!(at(&Parsed::new("class Broken\n  def foo\n"), 5).is_some());
        assert!(at(&Parsed::new(""), 0).is_some());
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

    /// Which `def`s [`Shapes::raising`] marks: the ones whose own code writes `raise` or `fail`,
    /// wherever in it, and not the one around a nested `def` that does.
    #[test]
    fn a_def_is_marked_where_its_own_code_writes_raise() {
        let source = "\
def a; raise; end
def b; fail \"x\"; end
def c(x = raise(ArgumentError)); x; end
def d; [1].each { raise }; end
def e; begin; 1; rescue; raise; end; end
def f; self.raise; end
def g; other.raise; end
def h; self&.raise; end
def i; def j; raise; end; 1; end
def k; ->(x) { fail }; end
def l; raised = 1; raised; end
";
        let marked: std::collections::BTreeSet<&str> = shapes(source)
            .raising
            .iter()
            .map(|(start, _)| &source[*start as usize + 4..*start as usize + 5])
            .collect();
        assert_eq!(
            marked,
            ["a", "b", "c", "d", "e", "f", "j", "k"]
                .into_iter()
                .collect()
        );
    }

    /// `raise` and `fail`, with no receiver or on `self`, bare or with arguments, are no exit: a
    /// `def` whose every exit raises has none. Nothing else of those names is dropped. The `||`
    /// operand [`never_returns`] reads is the same call, as a shape.
    #[test]
    fn only_ruby_s_own_raise_is_no_exit() {
        let exits = |source: &str| returns_in(source).into_values().next().unwrap_or_default();
        for raising in [
            "def a; raise; end",
            "def a; raise ArgumentError, \"x\"; end",
            "def a; fail(\"x\"); end",
            "def a; self.raise; end",
            "def a; return raise(\"x\"); end",
        ] {
            assert_eq!(exits(raising), Vec::new(), "{raising}");
        }
        for returning in [
            "def a; other.raise; end",
            "def a; self&.raise; end",
            "def a; raised; end",
            "def a; \"raise\"; end",
        ] {
            assert_eq!(exits(returning).len(), 1, "{returning}");
        }
        // One exit left beside the raise.
        assert_eq!(exits("def a(x); return 1 if x; raise; end").len(), 1);
        for (operand, raises) in [
            ("x || raise", true),
            ("x || fail(\"no\")", true),
            ("x || other.raise", false),
        ] {
            let found = exits(&format!("def a(x); {operand}; end"));
            let [Receiver::Shortcut { right, .. }] = found.as_slice() else {
                panic!("{operand}: {found:?}");
            };
            assert_eq!(never_returns(right), raises, "{operand}");
        }
    }

    /// `case … in` is one exit per arm, like `case … when`; with no `else` there is no `nil` exit,
    /// since a value no pattern matches raises.
    #[test]
    fn a_pattern_match_in_tail_position_is_one_exit_per_arm() {
        let source =
            "def a(x)\n  case x\n  in Integer then \"i\"\n  in String then 1\n  end\nend\n";
        assert_eq!(
            returns_in(source).into_values().next().unwrap_or_default(),
            vec![Receiver::literal("String"), Receiver::literal("Integer")]
        );
        let otherwise = "def a(x)\n  case x\n  in Integer then \"i\"\n  else nil\n  end\nend\n";
        assert_eq!(
            returns_in(otherwise)
                .into_values()
                .next()
                .unwrap_or_default(),
            vec![Receiver::literal("String"), Receiver::literal("NilClass")]
        );
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
    fn nesting_is_what_the_branch_bound_counts() {
        /// The exits of the one `def` in a body written as `body`.
        fn exits(body: &str) -> Vec<Receiver> {
            let source = format!("class Story\n  def title\n{body}\n  end\nend\n");
            returns_in(&source)[&def_span(&source, "title")].clone()
        }
        let strings = |count| vec![Receiver::literal("String"); count];
        // An `elsif` is one more branch of the same conditional, as a `when` is: a long chain is
        // flat, and every branch is read.
        assert_eq!(
            exits(
                "    if a?\n      \"a\"\n    elsif b?\n      \"b\"\n    elsif c?\n      \"c\"\n    \
                 elsif d?\n      \"d\"\n    else\n      \"e\"\n    end"
            ),
            strings(5)
        );
        // An `else` costs what the branch before it cost, so a ternary inside one is one level
        // down, not two.
        assert_eq!(
            exits("    if a?\n      \"a\"\n    else\n      b? ? \"b\" : \"c\"\n    end"),
            strings(3)
        );
        // A `begin`/`rescue` as the tail is read at the depth one on the `def` itself is.
        let rescue = "      a? ? b? ? \"y\" : \"z\" : \"w\"";
        assert_eq!(
            exits(&format!(
                "    begin\n      \"x\"\n    rescue\n{rescue}\n    end"
            )),
            exits(&format!("    \"x\"\n  rescue\n{rescue}")),
        );
        assert_eq!(exits(&format!("    \"x\"\n  rescue\n{rescue}")), strings(4));
        // A `retry` runs the body again, so it is no exit of its own, on the `def` or a tail
        // `begin`.
        assert_eq!(exits("    \"x\"\n  rescue Timeout\n    retry"), strings(1));
        assert_eq!(
            exits("    begin\n      \"x\"\n    rescue\n      retry\n    end"),
            strings(1)
        );
        // The bound still holds where conditionals really nest, as a guard: four deep, the deepest
        // the reference corpora write, is read, and so is everything short of the bound. At the
        // bound the value is unreadable, which declines the method.
        let nested = |depth: usize| {
            let mut body = "\"x\"".to_owned();
            for level in 0..depth {
                body = format!("if c{level}?\n{body}\nend");
            }
            exits(&body)
        };
        let nils = |count| vec![Receiver::literal("NilClass"); count];
        assert_eq!(nested(4), [strings(1), nils(4)].concat());
        assert_eq!(
            nested(MAX_BRANCHES - 1),
            [strings(1), nils(MAX_BRANCHES - 1)].concat()
        );
        assert_eq!(
            nested(MAX_BRANCHES),
            [vec![Receiver::Unknown], nils(MAX_BRANCHES)].concat()
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
        /// The exits of the one `def` in a body written as `body`, and where `needle` is in it.
        fn exits(body: &str) -> Vec<Receiver> {
            let source = format!("class Story\n  def title\n{body}\n  end\nend\n");
            returns_in(&source)[&def_span(&source, "title")].clone()
        }
        fn at(body: &str, needle: &str) -> u32 {
            format!("class Story\n  def title\n{body}\n  end\nend\n")
                .find(needle)
                .unwrap() as u32
        }
        // Every plain assignment [`assigned_value`] reads, written as a value, not a `def`'s tail,
        // because a constant assigned inside a method is a syntax error. `||=` on a constant or a
        // global is the new value too: nothing here tracks their old one.
        for written in [
            "@t = true",
            "t = true",
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
                stored(Receiver::literal("TrueClass")),
                "{written}"
            );
        }
        // A variable's `||=` is its old value where that was truthy: the old value read where the
        // name is written, or the new one.
        for written in ["@t ||= true", "t ||= true"] {
            assert_eq!(
                receiver(&format!("({written}).~")),
                stored(Receiver::Shortcut {
                    left: Box::new(Receiver::Variable(1)),
                    right: Box::new(Receiver::literal("TrueClass")),
                    and: false,
                }),
                "{written}"
            );
        }
        // The common shape: `def show_title_h1; @title_h1 = true; end` must answer `TrueClass`,
        // like the same method without `@title_h1 =`.
        assert_eq!(
            exits("    @title_h1 = true"),
            vec![stored(Receiver::literal("TrueClass"))]
        );
        // The memoisation idiom: the cached value, or the one built now.
        let body = "    @periods ||= [\"1d\"]";
        assert_eq!(
            exits(body),
            vec![stored(Receiver::Shortcut {
                left: Box::new(Receiver::Variable(at(body, "@periods"))),
                right: Box::new(holding("Array", &[Some("String")])),
                and: false,
            })]
        );
        // `+=` returns what the *operator* returned: the old value's `+`.
        let body = "    @count += 1";
        assert_eq!(
            exits(body),
            vec![stored(Receiver::Returned {
                on: Box::new(Receiver::Variable(at(body, "@count"))),
                method: "+".to_owned(),
                block: Block::None,
                arity: Arity::Exactly(1),
                arguments: vec![integer()],
                keywords: Some(Vec::new()),
                safe: false,
            })]
        );
        // `&&=` returns the old value where it is falsy, and the new one otherwise.
        let body = "    @title &&= \"x\"";
        assert_eq!(
            exits(body),
            vec![stored(Receiver::Shortcut {
                left: Box::new(Receiver::Variable(at(body, "@title"))),
                right: Box::new(Receiver::literal("String")),
                and: true,
            })]
        );
        // Not a method-body rule: the same arm answers a chain on an assignment, and an assignment
        // as another's value.
        assert_eq!(
            receiver("(@x = \"s\").~"),
            stored(Receiver::literal("String"))
        );
        assert_eq!(
            exits("    outer = inner = \"s\""),
            vec![stored(stored(Receiver::literal("String")))]
        );
        // Each branch is read through the assignment it ends in, so two that agree are an answer
        // and two that do not are declined.
        assert_eq!(
            exits("    if plain?\n      @a = \"x\"\n    else\n      @b = \"y\"\n    end"),
            vec![
                stored(Receiver::literal("String")),
                stored(Receiver::literal("String"))
            ]
        );
    }

    #[test]
    fn a_method_parameter_travels_as_its_def_and_its_slot_because_no_write_introduces_one() {
        /// What reaches the read a `~` marks, inside one `def` written as `header`.
        fn inside(header: &str, body: &str) -> Receiver {
            typed(&format!(
                "class Shelf\n  def {header}\n    {body}\n  end\nend\n"
            ))
        }
        fn parameter(at: u32, method: &str, slot: ParameterSlot) -> Receiver {
            Receiver::Parameter {
                at,
                method: method.to_owned(),
                slot,
            }
        }
        // The `def` starts at byte 14: `class Shelf\n  ` is fourteen characters.
        //
        // A parameter is no write: it binds before the body runs, which is the one value reaching
        // a read that no write kills first.
        assert_eq!(
            inside("show(held)", "held.~"),
            parameter(14, "show", ParameterSlot::Positional(0))
        );
        // Counted from the left, required and optional alike.
        assert_eq!(
            inside("show(first, held)", "held.~"),
            parameter(14, "show", ParameterSlot::Positional(1))
        );
        // A default is not the parameter's type: a caller may pass anything.
        assert_eq!(
            inside("show(first, held = 1)", "held.~"),
            parameter(14, "show", ParameterSlot::Positional(1))
        );
        // **A destructured positional holds its place without a name.** `def f((a, b), held)` binds
        // `a` and `b`, which this does not name, and `held` is still at index one, since a caller
        // counts the destructure as one argument.
        assert_eq!(
            inside("show((first, second), held)", "held.~"),
            parameter(14, "show", ParameterSlot::Positional(1))
        );
        // A keyword is called by name, never counted, because Ruby binds it by name; a slot
        // numbered across both would answer `held` with whatever sits at position one.
        assert_eq!(
            inside("show(first, held:)", "held.~"),
            parameter(14, "show", ParameterSlot::Keyword("held".to_owned()))
        );
        // **`*rest`, `**rest` and `&block` get no slot**, nor does a positional after a rest: its
        // position depends on how many arguments the call wrote. The first three hold a class Ruby
        // fixes; the positional binds a value nothing here can read, which refuses.
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
        assert_eq!(inside("show(*held)", "held.~"), Receiver::literal("Array"));
        assert_eq!(inside("show(**held)", "held.~"), Receiver::literal("Hash"));
        assert_eq!(
            inside("show(&held)", "held.~"),
            Receiver::Either(vec![
                Receiver::literal("Proc"),
                Receiver::literal("NilClass")
            ])
        );
        assert_eq!(inside("show(*rest, held)", "held.~"), Receiver::Unknown);
        // **A write that runs on every path kills the parameter's value**: the name was
        // reassigned, so the header no longer says what it holds.
        assert_eq!(
            inside("show(held)", "held = \"s\"\n    held.~"),
            Receiver::literal("String")
        );
        // A read outside every `def` is nobody's parameter.
        assert!(!slotted(&receiver("class Shelf\n  held.~\nend\n")));
    }

    #[test]
    fn a_write_that_relays_a_parameter_is_the_write_that_ran() {
        // `held = passed` runs after `held = "s"` on every path, so it is what `held` holds: the
        // parameter's value, whatever that is. The old slots kept the `String` (#11). This is
        // A real engine, reduced: `preference_store_class = Spree::Config` in one branch and
        // `= prefs_or_conf_class` in the other, where a branch adds instead.
        let source = "class Shelf\n  def stow(passed)\n    held = \"s\"\n    held = passed\n    held.~\n  end\nend\n";
        let passed = source.find("passed\n    held.~").unwrap() as u32;
        assert_eq!(typed(source), read_of(passed, "passed"));
        let branched = "class Shelf\n  def stow(passed)\n    held = \"s\"\n    held = passed if c\n    held.~\n  end\nend\n";
        let passed = branched.find("passed if").unwrap() as u32;
        assert_eq!(
            reached(branched),
            (
                vec![Receiver::literal("String"), read_of(passed, "passed")],
                false,
                None
            )
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
        // An operand is carried whatever it is: the untaken side is never read, and
        // `nil && whatever` is `nil`. `@count += 1` is the old value's `+`.
        let count = "class Story\n  def title\n    nil && @count += 1\n  end\nend\n"
            .find("@count")
            .unwrap() as u32;
        assert_eq!(
            exits("    nil && @count += 1"),
            shortcut(
                Receiver::literal("NilClass"),
                stored(Receiver::Returned {
                    on: Box::new(Receiver::Variable(count)),
                    method: "+".to_owned(),
                    block: Block::None,
                    arity: Arity::Exactly(1),
                    arguments: vec![integer()],
                    keywords: Some(Vec::new()),
                    safe: false,
                }),
                true
            )
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
            vec![stored(Receiver::Shortcut {
                left: Box::new(Receiver::literal("String")),
                right: Box::new(Receiver::literal("Integer")),
                and: true,
            })]
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
            vec![stored(Receiver::literal("String"))]
        );
        // The index spelling is the same node with the name `[]=`. `report[key] = v` evaluates to
        // `v` too; the subscripts are the arguments before it.
        assert_eq!(
            exits("    report[:a] = 1"),
            vec![stored(Receiver::literal("Integer"))]
        );
        assert_eq!(
            exits("    grid[1, 2] = [\"x\"]"),
            vec![stored(holding("Array", &[Some("String")]))]
        );
        // Assignment syntax either way, per Prism's own flag, not the name: `obj.x=(v)` is an
        // assignment and returns `v`.
        assert_eq!(
            receiver("(story.name=(\"x\")).~"),
            stored(Receiver::literal("String"))
        );
        // Not a method-body rule, like the assignments above: the same arm answers a chain on one,
        // and one as another's value.
        assert_eq!(
            receiver("(story.name = \"x\").~"),
            stored(Receiver::literal("String"))
        );
        assert_eq!(
            exits("    held = story.name = \"x\""),
            vec![stored(stored(Receiver::literal("String")))]
        );
        // **Safe navigation is a union**. `story&.name = "x"` returns `nil` where `story`
        // is `nil`, so it is `String | nil`.
        assert_eq!(
            exits("    story&.name = \"x\""),
            vec![stored(Receiver::Either(vec![
                Receiver::literal("String"),
                Receiver::literal("NilClass")
            ]))]
        );
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
    fn a_write_of_super_reaches_like_any_other() {
        // `ActionController::Instrumentation#render` in miniature: `out = nil`, then `out = super`
        // inside a block. The block may run, so both writes reach. The old slots kept the `nil`
        // and labelled every Rails action ending in `render` `-> nil`.
        let source = concat!(
            "class Story\n  def title\n    out = nil\n",
            "    [1].each { out = super }\n    out.~\n  end\nend\n"
        );
        let (writes, nil, bound) = reached(source);
        assert_eq!(writes.len(), 2, "{writes:?}");
        assert_eq!(writes[0], Receiver::literal("NilClass"));
        assert!(matches!(writes[1], Receiver::Super { .. }), "{writes:?}");
        assert!(!nil && bound.is_none());
        // With no write above it, the `super` alone reaches.
        let alone = "class Story\n  def title\n    out = super\n    out.~\n  end\nend\n";
        assert!(matches!(typed(alone), Receiver::Super { .. }));
    }

    #[test]
    fn a_return_inside_a_lambda_belongs_to_the_lambda_and_not_the_method() {
        /// The exits of the one `def` in a body written as `body`.
        fn exits(body: &str) -> Vec<Receiver> {
            let source = format!("class Story\n  def title\n{body}\n  end\nend\n");
            returns_in(&source)[&def_span(&source, "title")].clone()
        }
        // A real `code_column` in miniature, and why the fence exists: a `return` inside `->`
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
    fn a_chain_of_aliases_is_one_reference_per_hop() {
        /// `a = "x"` and then `hops` aliases of it, read at the last one.
        fn aliased(hops: usize) -> (String, Receiver) {
            let mut source = String::from("a = \"x\"\n");
            let mut previous = "a".to_owned();
            for step in 0..hops {
                source.push_str(&format!("v{step} = {previous}\n"));
                previous = format!("v{step}");
            }
            source.push_str(&format!("{previous}.~\n"));
            let answered = typed(&source);
            (source, answered)
        }
        // Each hop is the next variable's read, however long the chain: no width is spent, and
        // resolving it is `types`' depth guard, not this walk's.
        for hops in [1, 6, 40] {
            let (source, answered) = aliased(hops);
            let name = if hops == 1 {
                "a".to_owned()
            } else {
                format!("v{}", hops - 2)
            };
            let at = source.rfind(&format!("= {name}\n")).unwrap() as u32 + 2;
            assert_eq!(answered, read_of(at, &name), "{hops} hops");
        }
        // Links are still bounded by the width: twenty type fine, one question each.
        assert!(matches!(
            receiver("\"hi\".upcase.upcase.upcase.upcase.upcase.upcase.~"),
            Receiver::Returned { .. }
        ));
    }

    #[test]
    fn every_read_is_one_row_and_every_write_one_entry_however_the_writes_chain() {
        // The first version of the reaching rule copied each write's answer into every read it
        // reached, and `q = q.map { |v| v } if c` written n times held 2^n copies. A read now
        // points at its writes, so a table is as big as its text, not as its paths.
        let mut source = String::from("q = [1]\n");
        for _ in 0..24 {
            source.push_str("q = q.map { |v| v } if c\n");
        }
        source.push_str("q\n");
        let table = shapes(&source).variables;
        // 25 writes of `q`, and 25 reads of it: one in each value, one at the end.
        assert_eq!(table.writes.len(), 25);
        let last = source.rfind('q').unwrap() as u32;
        // Every conditional write reaches the last read, and so does the first, which nothing kills.
        assert_eq!(table.reaching(last).unwrap().writes.len(), 25);

        // Every write of an instance variable reaches every read, in any order the methods run.
        let source = "@x = \"s\"\n@x = whatever\n@x.~";
        assert_eq!(
            reached(source),
            (
                vec![
                    written(0, Receiver::literal("String")),
                    written(9, self_call(14, "whatever"))
                ],
                true,
                None
            )
        );
    }

    #[test]
    fn a_conditional_read_as_a_value_is_every_branch_it_can_hand_back() {
        // Each branch's last value in written order, an unwritten one `nil`; `Receiver::Either`.
        let (int, text, nil) = (
            Receiver::literal("Integer"),
            Receiver::literal("String"),
            Receiver::literal("NilClass"),
        );
        let either =
            |arms: &[&Receiver]| Receiver::Either(arms.iter().map(|arm| (*arm).clone()).collect());
        assert_eq!(receiver("(ok ? 1 : \"s\").~"), either(&[&int, &text]));
        assert_eq!(
            receiver("(if a then 1 elsif b then \"s\" else nil end).~"),
            either(&[&int, &text, &nil])
        );
        // No `else` is `nil`, and so is an empty branch.
        assert_eq!(receiver("(if a then 1 end).~"), either(&[&int, &nil]));
        assert_eq!(receiver("(if a then else 1 end).~"), either(&[&nil, &int]));
        assert_eq!(receiver("(unless a then 1 end).~"), either(&[&int, &nil]));
        assert_eq!(
            receiver("(unless a then 1 else \"s\" end).~"),
            either(&[&int, &text])
        );
        assert_eq!(
            receiver("(case x when 1 then 1 end).~"),
            either(&[&int, &nil])
        );
        assert_eq!(
            receiver("(case x when 1 then 1 else \"s\" end).~"),
            either(&[&int, &text])
        );
        // A `case … in` with no `else` raises where nothing matches, so it adds no `nil`.
        assert_eq!(
            receiver("(case x\nin 1 then 1\nin 2 then \"s\"\nend).~"),
            either(&[&int, &text])
        );
        assert_eq!(
            receiver("(case x\nin 1 then 1\nelse nil\nend).~"),
            either(&[&int, &nil])
        );
        // `begin`: every `rescue` is a branch, and an `else` replaces the statements' value.
        assert_eq!(
            receiver("(begin\n  1\nrescue A\n  \"s\"\nrescue B\n  nil\nend).~"),
            either(&[&int, &text, &nil])
        );
        assert_eq!(
            receiver("(begin\n  1\nrescue\n  nil\nelse\n  \"s\"\nend).~"),
            either(&[&text, &nil])
        );
        assert_eq!(receiver("(1 rescue \"s\").~"), either(&[&int, &text]));
        // A branch that raises or jumps away hands nothing back; every branch doing so is no value.
        assert_eq!(receiver("(ok ? 1 : raise(\"x\")).~"), either(&[&int]));
        assert_eq!(
            receiver("(if ok then return 1 else next end).~"),
            Receiver::Unknown
        );
        // A `begin` with nothing rescued is not a conditional.
        assert_eq!(receiver("(begin\n  1\nend).~"), int);
    }

    #[test]
    fn a_begin_that_rescues_nothing_is_its_last_statement() {
        // One value, as parentheses are, whatever ran before it. An `ensure`'s value is thrown
        // away, and an empty `begin` is `nil`.
        assert_eq!(receiver("(begin\n  \"s\"\n  1\nend).~"), integer());
        assert_eq!(receiver("(begin\n  1\nensure\n  \"s\"\nend).~"), integer());
        assert_eq!(receiver("(begin\nend).~"), Receiver::literal("NilClass"));
        assert_eq!(typed("x = begin\n  1\nend\nx.~"), integer());
        // An `else` with no `rescue` is a syntax error, and is not read.
        assert_eq!(
            receiver("(begin\n  1\nelse\n  \"s\"\nend).~"),
            Receiver::Unknown
        );
    }

    #[test]
    fn a_match_reference_is_a_string_or_nil() {
        // A numbered group or a back reference reads the last match, which may not have happened.
        let string_or_nil = Receiver::Either(vec![
            Receiver::literal("String"),
            Receiver::literal("NilClass"),
        ]);
        for spelled in ["$1", "$9", "$&", "$'", "$`", "$+"] {
            assert_eq!(
                receiver(&format!("{spelled}.~")),
                string_or_nil,
                "{spelled}"
            );
        }
    }

    #[test]
    fn a_rescue_binds_the_classes_it_names() {
        // `rescue A, B => e` is an instance of one of them; a bare `rescue => e` is Ruby's default,
        // `StandardError` (`Receiver::Rescued` of nothing); a splat names nothing readable.
        let source = "begin\nrescue Net::ReadTimeout, KeyError => e\n  e.~\nend\n";
        let named = source.find("ReadTimeout").unwrap() + "ReadTimeout".len();
        let other = source.find("KeyError").unwrap() + "KeyError".len();
        assert_eq!(
            reached(source).0,
            vec![Receiver::Rescued(vec![named as u32, other as u32])]
        );
        assert_eq!(
            reached("begin\nrescue => e\n  e.~\nend\n").0,
            vec![Receiver::Rescued(Vec::new())]
        );
        assert_eq!(
            reached("begin\nrescue *ERRORS => e\n  e.~\nend\n").0,
            vec![Receiver::Unknown]
        );
        assert_eq!(
            reached("begin\nrescue A, *ERRORS => e\n  e.~\nend\n").0,
            vec![Receiver::Unknown]
        );
    }

    #[test]
    fn a_splat_a_double_splat_and_a_block_parameter_hold_the_class_ruby_fixes() {
        // `*rest` is an `Array`, `**rest` a `Hash` and `&block` a `Proc` or `nil`, whatever the
        // caller passed, in a block and a lambda as in a `def`. A `*rest` target of a multiple
        // assignment is an `Array` whatever the value is, for a local and an instance variable; the
        // names beside it still read nothing.
        let proc_or_nil = Receiver::Either(vec![
            Receiver::literal("Proc"),
            Receiver::literal("NilClass"),
        ]);
        assert_eq!(
            typed("items.each { |*rest| rest.~ }\n"),
            Receiver::literal("Array")
        );
        assert_eq!(
            typed("items.each { |**options| options.~ }\n"),
            Receiver::literal("Hash")
        );
        assert_eq!(typed("items.each { |&block| block.~ }\n"), proc_or_nil);
        assert_eq!(
            typed("run = ->(*rest) { rest.~ }\n"),
            Receiver::literal("Array")
        );
        assert_eq!(
            typed("head, *rest = 1\nrest.~\n"),
            Receiver::literal("Array")
        );
        assert_eq!(
            typed("*rest, tail = pair\nrest.~\n"),
            Receiver::literal("Array")
        );
        assert_eq!(
            reached("head, *rest = pair\nhead.~\n").0,
            vec![Receiver::Unknown]
        );
        // A target that is neither a local nor an instance variable (`*self.rest`, a setter) is
        // written through its own method, and nothing here tracks it.
        assert_eq!(
            reached("head, *self.rest = pair\nhead.~\n").0,
            vec![Receiver::Unknown]
        );
        let source = "class Box\n  def fill\n    @head, *@rest = pair\n    @rest.~\n  end\nend\n";
        let at = source.find("@rest").unwrap() as u32;
        assert_eq!(
            reached(source).0,
            vec![written(at, Receiver::literal("Array"))]
        );
    }

    #[test]
    fn a_proc_literal_is_a_proc_that_knows_which_one_it_is() {
        // `->`, `lambda { }`, `proc { }` and `Proc.new { }` are `Proc`s that remember where they
        // start, and their parameters are what a call of them passes; `lambda(&b)` and a
        // `lambda` with a receiver are ordinary calls.
        // `->` is syntax; the others are also the calls they are written as, which a class can
        // answer for itself.
        assert_eq!(receiver("-> { }.~"), Receiver::Proc { at: 0, call: None });
        for (source, method) in [("(lambda { |x| x }).~", "lambda"), ("(proc { }).~", "proc")] {
            let Receiver::Proc {
                at: 1,
                call: Some(call),
            } = receiver(source)
            else {
                panic!("{source} is a literal");
            };
            assert!(
                matches!(*call, Receiver::Returned { method: ref named, .. } if named == method),
                "{source}"
            );
        }
        let Receiver::Proc {
            at: 1,
            call: Some(call),
        } = receiver("(Proc.new { }).~")
        else {
            panic!("Proc.new is a literal");
        };
        assert!(matches!(*call, Receiver::Instance { .. }));
        assert!(!matches!(receiver("(lambda(&b)).~"), Receiver::Proc { .. }));
        assert!(!matches!(
            receiver("(x.lambda { }).~"),
            Receiver::Proc { .. }
        ));
        assert_eq!(
            typed("run = ->(a, b = 1) { b.~ }\n"),
            Receiver::ProcParameter { at: 6, index: 1 }
        );
        assert_eq!(
            typed("run = lambda { |a| a.~ }\n"),
            Receiver::ProcParameter { at: 6, index: 0 }
        );
        // What each literal binds and hands back: a lambda's `return` is its value; a proc's
        // leaves the method, so its body cannot be read.
        let found =
            shapes("f = ->(a, b = 1, *c) { return 1 if a\n \"x\" }\ng = proc { |a| return 1 }\n");
        let lambda = &found.procs[&4];
        assert!(lambda.lambda);
        assert_eq!(
            lambda.parameters,
            Some(ProcParameters {
                required: 1,
                defaults: vec![Receiver::literal("Integer")],
                rest: true,
                spreads: true,
            })
        );
        assert_eq!(
            lambda.exits,
            vec![Receiver::literal("String"), Receiver::literal("Integer")]
        );
        let proc = found.procs.values().find(|shape| !shape.lambda).unwrap();
        assert_eq!(proc.exits, vec![Receiver::Unknown]);
        // A list positions cannot follow binds nothing.
        let keyed = shapes("f = ->(a, k:) { a }\n");
        assert_eq!(keyed.procs[&4].parameters, None);
    }

    #[test]
    fn defined_names_what_it_asks_about_or_is_nil() {
        // A keyword, so no method answers it: the name of what the expression is, or `nil`
        //.
        let string_or_nil = Receiver::Either(vec![
            Receiver::literal("String"),
            Receiver::literal("NilClass"),
        ]);
        assert_eq!(receiver("defined?(@cache).~"), string_or_nil);
        assert_eq!(receiver("(defined?(super)).~"), string_or_nil);
    }

    #[test]
    fn a_block_or_lambda_is_filed_with_its_call_and_the_argument_it_is() {
        // Every block, and every lambda or `proc {}` / `lambda {}` written as an argument, with its
        // call; a position past a splat is not known, and a lambda anywhere else is no argument.
        let source = "\
class Widget
  scope :recent, -> { where }
  validates :name, if: -> { ok }, unless: proc { no }
  opts *rest, lambda { later }
  hook(stored = -> { never }) do
    item
  end
  def run
    [1].each { |n| n }
  end
end
";
        let found = shapes(source);
        let sites: Vec<(String, BlockSlot)> = found
            .blocks
            .iter()
            .map(|site| (site.method.clone(), site.slot.clone()))
            .collect();
        assert_eq!(
            sites,
            vec![
                ("scope".to_owned(), BlockSlot::Positional(1)),
                ("validates".to_owned(), BlockSlot::Keyword("if".into())),
                ("validates".to_owned(), BlockSlot::Keyword("unless".into())),
                ("hook".to_owned(), BlockSlot::Block),
                ("each".to_owned(), BlockSlot::Block),
            ]
        );
        // A receiverless call's receiver is `self` where the call starts.
        let hook = found
            .blocks
            .iter()
            .find(|site| site.method == "hook")
            .unwrap();
        assert_eq!(hook.on, Receiver::SelfObject(hook.call));
        // Inside `hook`'s block its site is asked; inside `run`'s `def` only `each`'s is, because
        // the `def` has its own `self`.
        let item = source.find("item").unwrap() as u32;
        let asked: Vec<&str> = found
            .rebinding(item)
            .iter()
            .map(|site| site.method.as_str())
            .collect();
        assert_eq!(asked, vec!["hook"]);
        let inner = source.find("n }").unwrap() as u32;
        let asked: Vec<&str> = found
            .rebinding(inner)
            .iter()
            .map(|site| site.method.as_str())
            .collect();
        assert_eq!(asked, vec!["each"]);
        // Nothing is around the class body itself.
        assert!(found.rebinding(0).is_empty());
    }

    /// A block that makes a class is a body of its own: a block around the call does not decide
    /// its `self`. Only the constant itself makes one.
    #[test]
    fn a_block_that_makes_a_class_is_a_body() {
        for (maker, makes) in [
            ("Class.new(Base)", true),
            ("::Module.new", true),
            ("Struct.new(:a)", true),
            ("Data.define(:a)", true),
            ("Foo::Class.new", false),
            ("Class.old", false),
            ("Data.new", false),
            ("new", false),
            ("helper.new", false),
        ] {
            let source = format!("before do\n  {maker} do\n    x = 1\n  end\nend\n");
            let inside = source.find("x = 1").expect("the probe") as u32;
            let found = shapes(&source);
            let asked: Vec<&str> = found
                .rebinding(inside)
                .iter()
                .map(|site| site.method.as_str())
                .collect();
            assert_eq!(asked.contains(&"before"), !makes, "{maker}: {asked:?}");
        }
    }

    /// What each written block hands back, by its call; one with a `break` is no method's body.
    #[test]
    fn every_written_block_is_read_for_what_it_hands_back() {
        let source = "let(:a) { 1 }\nlet(:b) { break 2 }\nlet(:c, &blk)\nlet(:d) do\n  next 3 if x\n  4\nend\n";
        let blocks = blocks_handed_back(source);
        let at = |needle: &str| source.find(needle).expect("the call") as u32;
        assert_eq!(blocks[&at("let(:a)")].len(), 1);
        assert!(
            !blocks.contains_key(&at("let(:b)")),
            "a break raises in a method"
        );
        assert!(
            !blocks.contains_key(&at("let(:c")),
            "a passed block has no body here"
        );
        assert_eq!(blocks[&at("let(:d)")].len(), 2, "a next is one more value");
    }
}
