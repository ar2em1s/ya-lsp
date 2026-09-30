//! What a method returns, for the methods RBS says it about.
//!
//! # Why the table is keyed by a `DeclarationId`
//!
//! - **rubydex only answers "which declaration is this name".** It drops a signature's types at
//!   index time, so ya-lsp keeps its own table beside the graph. Only the key is a rubydex type,
//!   which keeps the rubydex pin cheap to move.
//! - **The key is the declaration rubydex found, not `(class, method)`.** `[].tap` is owned by
//!   `Kernel`, not `Array`, so a receiver-keyed table would redo rubydex's ancestor walk.
//! - **`DeclarationId::from("String#upcase()")` is a pure hash**, so the harvest builds keys
//!   without a graph. `tests::rubydex_spells_a_signature_the_way_this_keys_it` pins the spelling:
//!   get it wrong and every lookup misses silently.
//!
//! # What is kept and what is dropped
//!
//! | RBS | example | table |
//! | --- | --- | --- |
//! | a class instance | `-> String` | `String` |
//! | a generic | `-> Array[Integer]` | `Array` — the erased head |
//! | optional | `-> String?`, `-> (String \| nil)` | `String`, marked |
//! | the boolean pair | `-> bool`, `-> (true \| false)` | `TrueClass`, marked |
//! | `self` | `-> self` | whatever the call was made *on*, resolved at lookup |
//! | `instance` | `-> instance` | an instance of the owner |
//! | `class`, `singleton(Foo)` | `-> class` | the singleton class |
//! | a literal | `-> false`, `-> 0`, `-> "x"`, `-> :sym` | the one class it names |
//! | any other union, an interface, `untyped`, `void`, a proc, a tuple, a record, a type variable, a type alias | | **dropped** |
//!
//! 1. **A generic keeps its head.** `Array[Integer]` and `Array[String]` reach the same members, so
//!    `Array` is exact for a `.`. A type variable such as `Array#first`'s `E` is dropped, so a
//!    chain through `first` stops.
//! 2. **Two unions are folded, however they are spelled.** `X | nil` is `X` with a mark, and
//!    `true | false` is the pair.
//! 3. **Every other union is dropped.** A list built from two classes is half wrong, with nothing
//!    saying which half.
//! 4. **A literal names one class.** `-> false` is `FalseClass`, which is how
//!    `Kernel#nil?: () -> false` makes `.nil?` resolve.
//! 5. **A mark is not a type.** `String?` offers `String`'s members to a completion, and a lookup
//!    reads `String`'s member. This is the one inexact entry, since a value that was `nil` has no
//!    `upcase`, and the margin says so. [`Typed`] holds the rule. Two exceptions, both in
//!    [`returned_by`]:
//!    - **Where `NilClass` answers the same public name, both halves are asked and folded.**
//!      `x.nil?` is `bool` and `x.dup` is `String?`.
//!    - **`x&.m` carries the `nil` that `&.` adds**, so it is `M?`.
//!
//! # Where one signature is not trusted alone
//!
//! - **A `bool` receiver asks both halves.** ActiveSupport defines `blank?` on `TrueClass` and
//!   `FalseClass` with opposite bodies. Where the halves name two declarations, both resolve and
//!   fold, so `"x".empty?.blank?` is `bool`. [`split_bool`].
//! - **A `T?` receiver asks `NilClass` too.** `Kernel#nil?` is `-> false` and `NilClass#nil?` is
//!   `-> true`, and ActiveSupport's `present?` splits the same way. Where `NilClass` has a public
//!   member of the name, both resolve and fold, so `x.nil?` is `bool`. [`from_nil`].
//! - **`!` is folded against its operand, never looked up.** `!x` is `false` when `x` is truthy,
//!   `true` when falsy, and `bool` when `x` does not resolve or only a name guess types it. That
//!   types `Object#blank?`'s `!!empty?`. [`negated`].
//! - **A signature two gems' bodies dispute is joined with them.** `stdlib/json` declares `as_json`
//!   on thirteen core classes, and ActiveSupport redefines nine of them. Where more than one Ruby
//!   `def` answers a declaration, the answers are unioned and [`Typed::one`] makes the pair
//!   terminal. [`disputed`].
//! - **Two arms of one arity are told apart by the arguments written.** `Integer#+` has five
//!   one-argument arms. This runs only where the arity partition already refused, so it adds
//!   answers and changes none. An arm it cannot read blocks it. [`pick_by_argument`].
//!
//! # Overloads are a union, unless a block tells them apart
//!
//! ```text
//! def bytes: () -> Array[Integer]
//!          | () { (Integer byte) -> void } -> self
//! ```
//!
//! - **Whether the caller wrote a block picks the arm.** `"x".bytes.` is `Array` and
//!   `"x".bytes { }.` is `String`, both exact.
//! - **An optional block (`?{ ... } -> T`) counts on both sides.**
//! - Read as unions, `bytes`, `chars`, `lines` and `split` would all be lost.
//!
//! # The receiver rungs, in order
//!
//! The order is what makes the last rung safe to ship.
//!
//! 1. rubydex named the receiver. Nothing here runs.
//! 2. A signature or an assignment in the same class.
//! 3. **The class a template's path names.** `app/views/stories/show.html.erb` is rendered by
//!    `StoriesController`, or by the mailer the directory names when there is no controller, which
//!    is Rails' own order. The card names the class and the line it read, so a reader can check.
//! 4. **The receiver's own spelling.** `@user` is a `User`. The only answer allowed to be wrong: it
//!    is always labelled, and [`Sources::guess`] turns it off.
//! 5. The name-based list.
//!
//! A lower rung never displaces a higher one, and none runs until rubydex comes back imprecise.
//! Three places enforce the order:
//!
//! - [`locator::resolve_typed`] gates the ladder.
//! - [`named`] asks rung 3 before rung 4.
//! - `cursor` refuses to let a bare name count as an assignment's answer.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;

use ruby_rbs::node::{
    AttrAccessorNode, AttrReaderNode, AttributeKind, ClassNode, MethodDefinitionKind,
    MethodDefinitionNode, ModuleNode, Node, SymbolNode, Visit, parse,
};
use rubydex::model::{
    declaration::{Ancestor, Ancestors, Declaration, Namespace},
    definitions::{Definition, Parameter, Receiver as DefinitionReceiver},
    graph::Graph,
    ids::{DeclarationId, NameId, StringId, UriId},
    name::{Name, ParentScope},
    string_ref::StringRef,
    visibility::Visibility,
};

use super::{
    cursor::{self, Arity, Block, ParameterSlot, Receiver},
    environment, erb,
    indexed::Indexed,
    locator,
    position::{ByteSpan, Rebase},
    scopes,
    synthesized::{Origin, Synthesized},
    views,
};
use crate::generated::{
    self, BLOCK, COLLECTION, DEFINED, ELEMENT, FORWARDED, HELD, KEYED, OWN_DEF, Runs, SCOPED, SENT,
    SHARED, WRITTEN,
};
use crate::knowledge;
use crate::workspace::{DocUri, rails};

/// What RBS declares each method to return, keyed the way rubydex keys the method.
///
/// The value is a class *name*, not a `DeclarationId`: signatures arrive in walk order, so `Array`
/// may be harvested before `array.rbs` is read.
#[derive(Debug, Default)]
pub struct Types {
    /// What a call reaches, settled. The only map a lookup reads.
    ///
    /// Derived from [`contributions`](Self::contributions), never written directly. When two
    /// documents declare the same method, the answer is what their arms agree on, not whichever was
    /// read last.
    returns: HashMap<DeclarationId, Overloads>,
    /// The arms behind each row of [`returns`](Self::returns), per document.
    ///
    /// - **One method is often declared in several documents.** `vendor/rbs/core/float.rbs` and
    ///   bigdecimal both declare `Float#+`. With one row per method, the last read would win. See
    ///   [`Contribution`].
    /// - **Settled on insert, not on lookup.** A lookup runs on every keystroke; an insert runs
    ///   once per `def` per document.
    contributions: HashMap<DeclarationId, Vec<Contribution>>,
    /// What a method's **block** is handed, by position.
    ///
    /// - **Separate from [`Overloads`], because it does not depend on the call site.** A call that
    ///   reaches this already wrote the block. One entry per method, only where every arm with a
    ///   block agrees.
    /// - **`None` in a slot** is a type the policy refuses (`untyped`, a type variable). The slot
    ///   stays so later positions line up.
    yields: HashMap<DeclarationId, Box<[Option<Returned>]>>,
    /// What a method returns as a **tuple**, by position: `IO.pipe` is `-> [IO, IO]`.
    ///
    /// - **[`Return`] has no tuple variant**, and [`class_of`] refuses one: `IO.pipe.` is an
    ///   `Array` question. A position exists only on the left of a multiple assignment.
    /// - **One entry per method, only where every arm agrees**, like [`yields`](Self::yields).
    /// - **One refused element refuses the whole tuple.** A reader counting names from the left
    ///   cannot see which position was dropped.
    tuples: HashMap<DeclarationId, Box<[Box<str>]>>,
    /// What a **constant** holds, for the constants a signature types.
    ///
    /// - **`ENV` and `URI::RFC2396_PARSER` hold an object, not a class**, and rubydex records a
    ///   constant's name, never its value. `ENV: RBS::Unnamed::ENVClass` states the type the way
    ///   `-> String` does.
    /// - **Keyed by the constant's own [`DeclarationId`]**, the hash of its qualified name, so
    ///   `Float::INFINITY` and another class's `INFINITY` are different keys.
    /// - **No overloads and no call site**: a constant is one thing.
    constants: HashMap<DeclarationId, Held>,
    /// What a signature says a method's **own parameters** are.
    ///
    /// - **Read from inside the body, not at a call.** It types a bare `story` in
    ///   `def show(story)`. `cursor::Finder::locals` cannot, because a parameter is not a write.
    /// - **One entry per method, only where every arm agrees**, like [`yields`](Self::yields) and
    ///   [`tuples`](Self::tuples). Arity picks which arm a call reaches, but not what a name inside
    ///   the body is bound to.
    /// - **No rung of its own.** Sorbet `sig`s, YARD `@param`s, gem `sig/`s, the project's `sig/`
    ///   and Ruby's core all arrive through [`Types::harvest`], so this module learns none of their
    ///   words (`types.md`).
    parameters: HashMap<DeclarationId, Parameters>,
    /// What `self` is inside a block or lambda a method is handed, where its signature says
    /// (`{ () [self: instance] -> void }`, `^() [self: T] -> untyped`), by the argument it arrives
    /// as.
    ///
    /// - **What RBS writes for a DSL.** `Rails.application.configure do … end` runs its block
    ///   against the application, `scope :recent, -> { … }` against a relation, and
    ///   `before_save do … end` against a record. Without this, `self` in each is the class body the
    ///   block is written in, and every receiverless call there asks the wrong class.
    /// - **Filled independently of returns**, like [`yields`](Self::yields): a method whose return
    ///   is refused may still say what its block runs against. Every arm that binds a slot must
    ///   agree, or the slot is dropped.
    selves: HashMap<DeclarationId, Box<[(SelfSlot, Rebound)]>>,
    /// The module whose Ruby `def` of the same name answers for a generated member
    /// ([`generated::DEFINED`]), read as the receiver's own method ([`defined_as_own`]).
    ///
    /// - **Its arm is no vote** ([`class_of`] refuses the sentinel), so a row a generator did type,
    ///   such as a framework table's, still answers alone. This is asked only where the returns
    ///   table said nothing, at the body seam.
    defined: HashMap<DeclarationId, Box<str>>,
    /// The generated members a call defines from its block ([`generated::BLOCK`]), whose value is
    /// that block's ([`made_from_block`]).
    ///
    /// A set, because the sentinel names nothing: the block is found at the member's own place.
    /// Its arm is no vote, as [`Self::defined`]'s is, and is asked at the same seam.
    blocked: HashSet<DeclarationId>,
    /// The generated members standing for the `def` written at their own place
    /// ([`generated::OWN_DEF`]), whose value is that `def`'s alone ([`own_defs`]). A set for
    /// [`Self::blocked`]'s reason.
    bodied: HashSet<DeclarationId>,
    /// The generated members that call the method their call's first argument names
    /// ([`generated::SENT`], [`sent`]). A set for [`Self::blocked`]'s reason; asked before any
    /// arm, since the arm is no vote.
    sent: HashSet<DeclarationId>,
    /// The generated members that look up the literal key their call passes first
    /// ([`generated::KEYED`], [`keyed`]): what it names is a body of knowledge's table, asked at
    /// the call. A set for [`Self::blocked`]'s reason; its arm is no vote.
    keyed: HashSet<DeclarationId>,
    /// The methods every overload of whose signature returns `void`, or every one `bot`
    /// ([`Nothing`]): no value a reader could use, which the card says ([`Types::nothing`]).
    nothing: HashMap<DeclarationId, Nothing>,
}

/// What a method declared to hand back no value hands back: nothing a caller may use (`void`), or
/// never returning at all (`bot`, and a `def` whose every path raises, [`never_returns`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nothing {
    Void,
    Bot,
}

impl Nothing {
    /// As RBS spells it.
    #[must_use]
    pub fn spelled(self) -> &'static str {
        match self {
            Self::Void => "void",
            Self::Bot => "bot",
        }
    }
}

/// Which argument of a call a signature's `[self: T]` belongs to ([`Types::selves`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SelfSlot {
    /// The call's own block.
    Block,
    /// A positional argument, counted from zero over required then optional ones.
    Positional(usize),
    /// A keyword argument, by name without the colon.
    Keyword(Box<str>),
    /// Any keyword `**rest` takes, for a keyword not declared by name.
    AnyKeyword,
}

/// What a signature's `[self: T]` says `self` is ([`Types::selves`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rebound {
    /// `[self: instance]`: an instance of the class the call is sent to. RBS reads `instance`, like
    /// `self`, against the receiver, so `before_save` declared on a base class still runs its block
    /// against the subclass it is written in.
    Instance,
    /// Anything a return can name, read against the receiver the same way: `self`, a class,
    /// [`Return::Element`], [`Return::Collection`].
    Returned(Returned),
    /// A `[self: T]` this cannot read (`top`, a `self` that may be `nil`, a type variable nothing
    /// binds), or arms that disagree about it. The signature says `self` is something else and not
    /// what, so `self` is refused there: the body's `self` would be a wrong answer.
    Unread,
}

/// The method whose block, or proc argument, becomes a method of the receiver
/// ([`declared_selves`]), so `self` there is an instance of it. RBS can only write that as
/// `[self: top]`.
const MAKES_A_METHOD: &str = "Module#define_method()";

/// One method's declared parameters, in the two ways Ruby binds them.
///
/// A positional is matched by *where* it sits and a keyword by its *name*;
/// [`cursor::ParameterSlot`] is the other half of this split. Positional names are never read:
/// position is all a caller and a callee must agree on.
#[derive(Debug, Default, Clone)]
struct Parameters {
    /// Required then optional, in written order. `None` is a type the policy refuses; the slot
    /// stays so later positions line up.
    positional: Box<[Option<Returned>]>,
    /// By name, without the colon. Unordered, because a keyword has no position.
    keyword: Box<[(Box<str>, Returned)]>,
}

/// One constant a signature types: its name and what it holds.
///
/// The name is kept beside its hash key because a hash cannot be read back, and the card must name
/// what it followed. `ENV.fetch("HOME").upcase` carries the derivation two rungs past `ENV`.
#[derive(Debug)]
struct Held {
    constant: Box<str>,
    class: Box<str>,
}

/// What one method returns, which can depend on how the call is written.
///
/// Two facts about the call site pick the arm. Both are syntax the cursor already read, so neither
/// can make an answer wrong:
///
/// 1. whether a block was written;
/// 2. how many positional arguments were written.
///
/// Arity can take answers **away**: a zero-argument call to a method that requires an argument gets
/// nothing. `types.md` argues why that trade is right. A method whose every partition is empty is
/// not stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Overloads {
    plain: Arms,
    with_block: Arms,
}

impl Overloads {
    fn get(&self, arity: Arity, with_block: bool) -> Option<&Returned> {
        if with_block {
            self.with_block.at(arity)
        } else {
            self.plain.at(arity)
        }
    }

    fn is_empty(&self) -> bool {
        self.plain.is_empty() && self.with_block.is_empty()
    }
}

/// One side of the block (with or without one), as a function of how many arguments were written.
///
/// `Float#round` is why: `(?half: ...) -> Integer` beside
/// `(int digits, ?half: ...) -> (Integer | Float)`. As one partition the two disagree and both are
/// dropped. By arity, `3.7.round.` is exactly `Integer`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Arms {
    /// Indexed by how many positional arguments were written. `None` means no arm takes that many,
    /// or the arms that do disagree. Long enough for every exact arity an arm names.
    by_arity: Vec<Option<Returned>>,
    /// What a call with more arguments than `by_arity` covers reaches. Set only where an arm has a
    /// rest parameter, which applies at every arity, as `?{ }` applies on both sides of the block.
    beyond: Option<Returned>,
    /// What a call whose arguments cannot be counted reaches: every arm on this side, agreeing.
    ///
    /// This is for `foo(*args)`. Holding the unpartitioned answer makes arity a partition, not a
    /// filter: such a call gets the answer it would get if arity were never read.
    any: Option<Returned>,
    /// [`Self::by_arity`] for a call that also wrote a keyword hash ([`Arity::Keyed`]): an arm
    /// that takes keywords at the positional count, and any other at one more, since Ruby hands it
    /// the hash as a positional.
    keyed: Vec<Option<Returned>>,
    /// [`Self::beyond`] for the same call.
    keyed_beyond: Option<Returned>,
}

impl Arms {
    fn at(&self, arity: Arity) -> Option<&Returned> {
        match arity {
            Arity::Unknown => self.any.as_ref(),
            // Past the end is the rest arms, not the nearest bucket, and nothing when there are
            // none. No arm accepts that call, which is what RBS says too.
            Arity::Exactly(written) => match self.by_arity.get(written as usize) {
                Some(bucket) => bucket.as_ref(),
                None => self.beyond.as_ref(),
            },
            // A spread's plain count is checked at the call, against the arms ([`Types::returns_to`]).
            Arity::Keyed(written) | Arity::Spread(written) => {
                match self.keyed.get(written as usize) {
                    Some(bucket) => bucket.as_ref(),
                    None => self.keyed_beyond.as_ref(),
                }
            }
        }
    }

    /// Whether no call can reach an answer here. The keyed buckets count: `(path, headers: true)
    /// -> Table | (path) -> Array` agrees only for a call that writes keywords.
    fn is_empty(&self) -> bool {
        self.by_arity.iter().chain(&self.keyed).all(Option::is_none)
            && self.beyond.is_none()
            && self.any.is_none()
            && self.keyed_beyond.is_none()
    }
}

/// What one method hands back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Return {
    /// A class by name (`String`, `Enumerator::Lazy`, `Widget::<Widget>`), with its type arguments.
    Class {
        /// The class itself: what a `.` asks. `Array[Integer]` and `Array[String]` reach the same
        /// members, so member lookup, chains and completion read only this.
        name: Box<str>,
        /// The namespace `name` was written in, unless it was written `::`-absolute. Empty for
        /// every `Class` this module builds itself (`self`, `instance`, `class`, a singleton),
        /// since those are already fully qualified.
        ///
        /// - **RBS resolves a bare name the way Ruby resolves a constant:** the enclosing scope,
        ///   then each outer one, then the top level. `Error` inside `class Errors` under
        ///   `module ActiveModel` is `ActiveModel::Error`.
        /// - **[`type_name`] cannot do that walk.** It has no graph, and the file may be read
        ///   before the class it names. The walk happens in [`declared_in`], which has the graph,
        ///   and this field carries the starting scope there.
        scope: Box<str>,
        /// The type arguments, by position: `Array[String]`'s `String`, `Hash[Symbol, Integer]`'s
        /// two.
        ///
        /// - **This is what a block asks, not the head.** `"a,b".split(",")` is `-> Array[String]`,
        ///   and the `String` types the block's parameter.
        /// - **A position this module cannot name is `None`**, so later positions keep counting:
        ///   `Hash[untyped, String]` holds `String` at 1.
        /// - **An argument's own `nil` mark is dropped.** `Array[String?]` holds `String`. That is
        ///   the head's inexactness again, and no surface reads a nested mark.
        /// - **`Box<[_]>`, not `Vec`**: it is almost always empty, and the table has tens of
        ///   thousands of rows.
        arguments: Box<[Option<Return>]>,
    },
    /// `self`: the *receiver's* type, not the declaring class.
    ///
    /// That is why it is resolved at lookup, not at harvest. `Kernel#tap` is
    /// `() { (self) -> void } -> self`, so `"x".tap` is a `String`. Filed as `Kernel`, `"x".tap.`
    /// would offer `Kernel`'s methods and none of `String`'s.
    Same,
    /// The **model** the receiver is about: `Story::Relation` and `Story`'s class object both mean
    /// `Story`.
    ///
    /// - **[`Return::Same`] one step further.** One signature of `first` serves every relation and
    ///   every model, so the generated declaration count does not grow with the model count. See
    ///   [`RELATION_BASE`](crate::workspace::rails::RELATION_BASE).
    /// - **Spelled [`ELEMENT`], only in RBS this crate generates.** No shipped signature uses it,
    ///   so reading it means reading ya-lsp's own text.
    Element,
    /// The **relation** of the model the receiver is about: `Story` and `Story::Relation` both mean
    /// `Story::Relation`.
    ///
    /// [`Return::Element`]'s twin, for what `self` cannot say: on a relation `where` returns
    /// `self`, but on the class object it returns the relation, a different type. Spelled
    /// [`COLLECTION`].
    Collection,
    /// `true` or `false`, with the type not saying which.
    ///
    /// RBS spells it `bool`, meaning `true | false`. Ruby has no boolean class, so the pair is its
    /// own answer:
    ///
    /// - **Where a `Return` becomes a declaration**, it resolves to `TrueClass`. Both halves carry
    ///   the same six core members, so a `.` reaches the same declarations.
    /// - **A reader is shown** RBS' own word, `bool`.
    Bool,
    /// The **receiver's own type argument**: the `E` of `class Array[E]`, by position.
    ///
    /// - **Most receivers cannot answer it.** A receiver typed `Array` says nothing about `E`, so
    ///   this resolves to nothing, like any refused type variable.
    /// - **A literal can.** In `[1, 2].each { |n| ... }` the element is in the source, and
    ///   [`cursor::Receiver::Literal`](super::cursor::Receiver::Literal) carries it, so
    ///   `Array#each`'s `(E element)` is `Integer`.
    ///
    /// # Why the declaring class travels with the position
    ///
    /// A position only means something against the parameter list it counts along. `Enumerable[E]`
    /// declares `map`; `Hash` includes it as `Enumerable[[K, V]]` and `Array` as `Enumerable[E]`.
    /// So the same `Parameter { at: 0, of: "Enumerable" }` is an array's element but a hash's whole
    /// **pair**.
    ///
    /// `of` is therefore compared with the receiver's own class, and a member reached through an
    /// ancestor is refused. Otherwise `{ a: 1 }.map { |x| }` would hand `x` a `Symbol`.
    /// [`declared_tuple`] applies the same rule to the other table.
    Parameter {
        /// Which of `of`'s type parameters, counted from the left.
        at: usize,
        /// The class or module that declared the method. `at` counts along its parameter list.
        of: Box<str>,
    },
    /// **What the block returned**: the `U` of `[U] () { (Elem) -> U } -> Array[U]`.
    ///
    /// - **Only the call can answer it.** `U` is bound per call, not per class, so the receiver
    ///   says nothing about what `map` returns. The block's last expression does:
    ///   [`cursor::Block`](super::cursor::Block) carries it and [`block_return`] resolves it.
    /// - **Filed only when the same method-level variable is in both slots.** The arm declares
    ///   `[U]`, the block returns `U`, and the method's return mentions `U`. That is the whole
    ///   safety argument.
    /// - **`sort_by` never reaches this.** It is `[U] { (Elem) -> U } -> Array[Elem]`: its return
    ///   names the receiver's parameter, which is [`Return::Parameter`].
    /// - **A class's variable in the block's return slot is left alone.** It is a fact about the
    ///   receiver, and a block must not decide what the receiver holds.
    Block,
    /// **Any of several**, as a signature wrote it: `Integer | Hash[untyped, Integer]`.
    ///
    /// - **Answered only at a call** ([`returned_from`], [`join_arms`]): each member resolves on
    ///   its own and the answers are joined, as a method's exits are ([`Join`]). A call on the
    ///   union then runs on each class ([`narrowed`]).
    /// - **Refused wherever one class is needed**: a block's parameter, a type argument, a
    ///   parameter's type. [`resolved`] answers `None` for it.
    /// - **Never holds `nil` or both booleans' halves apart:** `nil` folds into the mark and
    ///   `true | false` into [`Return::Bool`], one member.
    Union(Box<[Return]>),
    /// **Whatever the accessor's writer is given, or `nil`**: [`WRITTEN`], which a generator
    /// writes for a reader whose storage only that writer fills. Answered at a call by
    /// reading every call of the writer on the same object ([`written_to`]).
    Written,
    /// **What the lambda the declaring call was passed hands back, where truthy**: [`SCOPED`], only
    /// ever one member of a union whose rest is the value otherwise (a `scope`'s relation).
    /// Answered at a call by reading that lambda at the member's own place ([`scoped_beside`]).
    Scoped,
    /// **Whatever the writer is given on any instance of the declaring class**: [`SHARED`], which
    /// a generator writes for a reader whose storage every instance shares. Answered at a call by
    /// reading every call of the writer on a receiver typed as that class or a descendant
    /// ([`shared_by`]).
    Shared,
    /// **Whatever the writer is given on the receiver's class object or its one instance**:
    /// [`HELD`], an `ActiveSupport::CurrentAttributes` attribute. Answered at a call
    /// by reading every call of the writer, and every `set(name: value)`, on either
    /// ([`current_held`]).
    Held,
    /// **What `method` answers on what `through` answers**, both asked of the receiver:
    /// [`FORWARDED`], which a generator writes for a member that hands its call on to another
    /// object. `through` is a method the receiver answers, `self` included, or a constant spelled
    /// from the receiver's class. Answered at a call as those two calls ([`forwarded`]).
    Forwarded { through: Box<str>, method: Box<str> },
    /// **What the call passed at position `at`**: a method's own type variable that one argument
    /// binds whole, as `X` in `ENV.fetch`'s `[X] (String, X) -> (String | X)`.
    ///
    /// - **Only the call can answer it**, like [`Return::Block`]: `ENV.fetch("PORT", 3000)` is
    ///   `String | Integer` and `ENV.fetch("HOST", "x")` a `String`. [`bound_to_arguments`] puts
    ///   the argument's type in its place before anything resolves.
    /// - **Only a type the argument has**, resolved or derived: an argument nothing types, or only
    ///   a guess does, leaves the call with no answer, as before.
    /// - **Filed only where one argument says all of the variable** ([`passed_variables`]).
    Argument { at: usize },
}

impl Return {
    /// A class with no type arguments, which is what almost every signature declares.
    ///
    /// `name` is already fully qualified (`self`, `instance`, `class` and singletons are resolved
    /// against `owner` where they are read), so `scope` is empty.
    fn class(name: &str) -> Self {
        Self::Class {
            name: name.into(),
            scope: Box::default(),
            arguments: Box::default(),
        }
    }

    /// Two arms' answers merged, or `None` where they differ.
    ///
    /// [`Agreement::saw`] uses this because the type arguments make `PartialEq` too strict.
    /// `Array[String]` and `Array[Integer]` agree on the head, so the merge keeps `Array` and drops
    /// the position they differ at. Every other `Return` is compared whole.
    fn agreed(&self, other: &Self) -> Option<Self> {
        match (self, other) {
            (
                Self::Class {
                    name,
                    scope,
                    arguments,
                },
                Self::Class {
                    name: seen,
                    arguments: also,
                    ..
                },
            ) if name == seen => Some(Self::Class {
                name: name.clone(),
                // Both arms come from the same method's overloads, so they share a scope.
                scope: scope.clone(),
                arguments: arguments
                    .iter()
                    .zip(also.iter())
                    .map(|(held, seen)| held.clone().filter(|held| Some(held) == seen.as_ref()))
                    .collect(),
            }),
            _ => (self == other).then(|| self.clone()),
        }
    }
}

/// A return type as the table stores it: the class, and whether `nil` is one of the values.
///
/// - **`nil` rides beside the class.** Every step (lookup, chain, completion, jump) wants the class
///   without `nil`. Only a label reads the mark. `types.md` calls this the one inexact entry.
/// - **It keeps agreement honest.** `-> String` and `-> String?` agree on the class, so they settle
///   on `String?` instead of dropping each other. See [`Agreement::saw`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Returned {
    pub of: Return,
    /// `nil` was one of the values and was folded out of [`Self::of`].
    pub nilable: bool,
}

impl Returned {
    fn plain(of: Return) -> Self {
        Self { of, nilable: false }
    }

    fn or_nil(mut self) -> Self {
        self.nilable = true;
        self
    }
}

impl Types {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read every method definition in one RBS document, and keep the ones the policy takes.
    ///
    /// Returns whether the document parsed. An unparseable signature contributes nothing, and
    /// rubydex will not index it either. Only
    /// [`synthesized::Synthesized::record`](super::synthesized::Synthesized::record) reads the
    /// answer: it writes its own RBS, so a rejection there is this crate's bug.
    ///
    /// - **Harvesting one document twice replaces its contribution**, so re-indexing an edited
    ///   buffer cannot leave two answers. A method deleted from a buffer leaves its entry behind,
    ///   but nothing reaches it: rubydex no longer has the member.
    /// - **Two different documents declaring one method both count.** `vendor/rbs/core/float.rbs`
    ///   and bigdecimal both declare `Float#+`. See [`Contribution`].
    /// - **`document` is the document's URI, not its text**, so two harvests of one URI are one
    ///   contribution. Pass the URI rubydex indexed.
    pub fn harvest(&mut self, document: &str, source: &str) -> bool {
        let Ok(signature) = parse(source) else {
            return false;
        };
        let mut walk = Harvest {
            nesting: Vec::new(),
            aliases: Vec::new(),
            source,
            document: document_key(document),
            types: self,
        };
        walk.visit(&signature.as_node());
        let aliases = std::mem::take(&mut walk.aliases);
        for (wrote, means) in aliases {
            self.copy_rows(&wrote, &means);
        }
        true
    }

    /// What a method returns when called as `arity` and `with_block` say, or `None` where the
    /// policy has no answer for that call shape.
    ///
    /// Both are facts about the call site, and they are why `"x".bytes` and `3.7.round` can be
    /// answered at all.
    #[must_use]
    pub fn returns(
        &self,
        method: DeclarationId,
        arity: Arity,
        with_block: bool,
    ) -> Option<&Returned> {
        self.returns.get(&method)?.get(arity, with_block)
    }

    /// [`Self::returns`] for a keyword call, checked against the arms the call really reaches.
    ///
    /// The partition is built before any call, so a keyed bucket holds every arm that takes
    /// keywords at that count, whatever their names. Two calls it cannot tell apart:
    ///
    /// - **A call writing a name no arm takes**, or missing one each requires, reaches no arm, like
    ///   a count past every arm, and gets what that gets.
    /// - **`**opts` passes no keywords when `opts` is empty** ([`Arity::Spread`]), so it also
    ///   reaches the arms of its plain count, and those must agree too.
    fn returns_to(
        &self,
        method: DeclarationId,
        arity: Arity,
        with_block: bool,
        keywords: Option<&[(String, Receiver)]>,
    ) -> Option<&Returned> {
        let returns = self.returns(method, arity, with_block)?;
        if !matches!(arity, Arity::Keyed(_) | Arity::Spread(_)) {
            return Some(returns);
        }
        let arms = self.arms(method, with_block);
        let reached = arms
            .iter()
            .copied()
            .filter(|arm| arm.reached_by(arity, keywords));
        (agreed(reached).as_ref() == Some(returns)).then_some(returns)
    }

    /// Every arm every document wrote for this method, on one side of the block.
    ///
    /// - **Raw arms, not settled ones.** [`Self::returns`] is what the arms agree on; this is for
    ///   when they do not. See [`pick_by_argument`].
    /// - **Nothing extra is stored.** [`Contribution`] already keeps arms per document. The union
    ///   is taken here in read order, the order [`Self::insert`] settles in.
    /// - **Empty for a method no signature declares**, which costs one missed hash.
    fn arms(&self, method: DeclarationId, with_block: bool) -> Vec<&Arm> {
        self.contributions
            .get(&method)
            .map(|held| {
                held.iter()
                    .flat_map(|held| {
                        if with_block {
                            &held.with_block
                        } else {
                            &held.plain
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every arm of a method on one side of the block, **in the order it was written**, where one
    /// document wrote them all.
    ///
    /// [`Self::arms`] is the union over documents, whose order means nothing. An RBS overload set
    /// is tried in order, which only one document can say. `None` where two documents contribute.
    fn arms_in_order(&self, method: DeclarationId, with_block: bool) -> Option<&[Arm]> {
        match self.contributions.get(&method)?.as_slice() {
            [only] => Some(if with_block {
                &only.with_block
            } else {
                &only.plain
            }),
            _ => None,
        }
    }

    /// How many methods the table answers for. For the log line and the policy tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.returns.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.returns.is_empty()
    }

    /// Throw the table away, for a graph that is being built again from nothing.
    pub fn clear(&mut self) {
        self.returns.clear();
        self.contributions.clear();
        self.yields.clear();
        self.tuples.clear();
        self.constants.clear();
        self.parameters.clear();
        self.selves.clear();
        self.defined.clear();
        self.blocked.clear();
        self.bodied.clear();
        self.sent.clear();
    }

    /// The class a signature says this constant holds, if one does.
    ///
    /// Asked only of a constant used as a receiver. rubydex records a constant's name, never its
    /// value, so the graph cannot answer this.
    #[must_use]
    fn held(&self, constant: DeclarationId) -> Option<&Held> {
        self.constants.get(&constant)
    }

    /// What the block of `method` is handed at position `index`, if the signature says.
    ///
    /// `Story::Relation#each` is `() { (Story) -> void } -> Story::Relation`, so
    /// `.each do |instance|` is typed by the signature, not guessed from the parameter's name.
    #[must_use]
    pub fn yielded(&self, method: DeclarationId, index: usize) -> Option<&Returned> {
        self.yields.get(&method)?.get(index)?.as_ref()
    }

    /// The class at position `index` of the tuple `method` returns, if a signature says.
    ///
    /// Asked only of a multiple assignment's target. Past the end answers `None`:
    /// `a, b, c = IO.pipe` leaves `c` untyped, as Ruby does.
    #[must_use]
    pub fn tupled(&self, method: DeclarationId, index: usize) -> Option<&str> {
        Some(&**self.tuples.get(&method)?.get(index)?)
    }

    /// What a method returns **however the call is written**, or `None` where that depends on the
    /// call.
    ///
    /// - **[`Self::returns`] partitions by what a call wrote.** This is asked of the `def` itself,
    ///   where there is no call.
    /// - **It takes the arm every side agrees on.** `String#bytes` is `Array[Integer]` plainly and
    ///   `self` with a block, so a label on the `def` would be wrong half the time. It gets none.
    /// - **One side answering and the other empty counts as agreement.** A method whose every arm
    ///   takes a block has no plain arm to contradict it.
    #[must_use]
    pub fn declared_return(&self, method: DeclarationId) -> Option<&Returned> {
        let overloads = self.returns.get(&method)?;
        match (
            overloads.get(Arity::Unknown, false),
            overloads.get(Arity::Unknown, true),
        ) {
            (Some(plain), Some(with_block)) => (plain == with_block).then_some(plain),
            (answer, None) | (None, answer) => answer,
        }
    }

    /// Record one document's arms for one method, then settle the method again.
    ///
    /// - **Accumulate across documents, replace within one.** Re-reading an edited `sig/*.rbs`
    ///   replaces only that document's arms.
    /// - **Settle the union; never merge settled [`Arms`]** (see [`Contribution`]). A method whose
    ///   every partition is empty is removed, after the merge ([`Overloads::is_empty`]).
    ///
    /// # A document that says nothing readable gets no vote
    ///
    /// - **`-> untyped` means "no claim".** The generator that copies a gem's `module ClassMethods`
    ///   writes it for every member. The query interface declares
    ///   `def self.find_by!: (*untyped) -> Element` on `ActiveRecord::Base`, and the concern beside
    ///   it declares the same member `(*untyped) -> untyped`.
    /// - **A refused arm breaks [`Agreement`].** If the concern voted, the merge would delete the
    ///   row, and `@story = Story.find(...)` in every controller would fall from *derived* to
    ///   *guessed*.
    /// - **So a document whose every arm is unreadable is filed empty.** It is still filed, so a
    ///   later harvest of that document can replace it.
    /// - **Within one document, a refused arm still breaks agreement.**
    ///   `(X) -> untyped | (Y) -> String` made a claim, and one of its arms is unreadable.
    fn insert(&mut self, document: u64, method: &str, plain: Vec<Arm>, with_block: Vec<Arm>) {
        let key = DeclarationId::from(method);
        let held = self.contributions.entry(key).or_default();
        held.retain(|held| held.document != document);
        let mute = |arms: &[Arm]| arms.iter().all(|arm| arm.returns.is_none());
        let (plain, with_block) = if mute(&plain) && mute(&with_block) {
            (Vec::new(), Vec::new())
        } else {
            (plain, with_block)
        };
        held.push(Contribution {
            document,
            plain,
            with_block,
        });
        let arms = |side: fn(&Contribution) -> &Vec<Arm>| {
            settle(
                &held
                    .iter()
                    .flat_map(|held| side(held).clone())
                    .collect::<Vec<_>>(),
            )
        };
        let settled = Overloads {
            plain: arms(|held| &held.plain),
            with_block: arms(|held| &held.with_block),
        };
        if settled.is_empty() {
            self.returns.remove(&key);
        } else {
            self.returns.insert(key, settled);
        }
    }

    fn insert_yield(&mut self, method: &str, yields: Box<[Option<Returned>]>) {
        self.yields.insert(DeclarationId::from(method), yields);
    }

    fn insert_tuple(&mut self, method: &str, classes: Box<[Box<str>]>) {
        self.tuples.insert(DeclarationId::from(method), classes);
    }

    fn insert_parameters(&mut self, method: &str, parameters: Parameters) {
        self.parameters
            .insert(DeclarationId::from(method), parameters);
    }

    fn insert_selves(&mut self, method: &str, selves: Box<[(SelfSlot, Rebound)]>) {
        self.selves.insert(DeclarationId::from(method), selves);
    }

    fn insert_defined(&mut self, method: &str, module: Box<str>) {
        self.defined.insert(DeclarationId::from(method), module);
    }

    fn insert_blocked(&mut self, method: &str) {
        self.blocked.insert(DeclarationId::from(method));
    }

    fn insert_bodied(&mut self, method: &str) {
        self.bodied.insert(DeclarationId::from(method));
    }

    fn insert_sent(&mut self, method: &str) {
        self.sent.insert(DeclarationId::from(method));
    }

    /// Whether every overload of `method`'s signature says it hands back no usable value.
    #[must_use]
    pub fn nothing(&self, method: DeclarationId) -> Option<Nothing> {
        self.nothing.get(&method).copied()
    }

    /// What a signature says `self` is inside the block or lambda `method` is handed at `slot`, if
    /// one does. A keyword the signature does not name falls to its `**rest`, if that binds one.
    fn rebound(&self, method: DeclarationId, slot: &cursor::BlockSlot) -> Option<&Rebound> {
        let selves = self.selves.get(&method)?;
        let find = |wanted: &SelfSlot| {
            selves
                .iter()
                .find(|(held, _)| held == wanted)
                .map(|(_, rebound)| rebound)
        };
        match slot {
            cursor::BlockSlot::Block => find(&SelfSlot::Block),
            cursor::BlockSlot::Positional(index) => find(&SelfSlot::Positional(*index)),
            cursor::BlockSlot::Keyword(name) => {
                find(&SelfSlot::Keyword(name.clone())).or_else(|| find(&SelfSlot::AnyKeyword))
            }
        }
    }

    /// What a signature says the parameter at `slot` of `method` is, if one does.
    ///
    /// Asked only from inside a body. A positional past the end answers `None`. An undeclared
    /// keyword answers `None`, never a positional: Ruby binds the two differently.
    #[must_use]
    fn parameter(&self, method: DeclarationId, slot: &ParameterSlot) -> Option<&Returned> {
        let parameters = self.parameters.get(&method)?;
        match slot {
            ParameterSlot::Positional(index) => parameters.positional.get(*index)?.as_ref(),
            ParameterSlot::Keyword(name) => parameters
                .keyword
                .iter()
                .find(|(declared, _)| **declared == **name)
                .map(|(_, returned)| returned),
        }
    }

    /// Give `wrote` every row `means` has.
    ///
    /// Every table, because an alias *is* the method: `alias slice []` renames a return and
    /// `alias each_pair each` a block. A target with no rows copies nothing and leaves `wrote` as
    /// it was. That covers an alias of a refused method, and one whose target is in another
    /// document.
    fn copy_rows(&mut self, wrote: &str, means: &str) {
        let (wrote, means) = (DeclarationId::from(wrote), DeclarationId::from(means));
        if let Some(returns) = self.returns.get(&means).cloned() {
            self.returns.insert(wrote, returns);
        }
        if let Some(yields) = self.yields.get(&means).cloned() {
            self.yields.insert(wrote, yields);
        }
        if let Some(tuple) = self.tuples.get(&means).cloned() {
            self.tuples.insert(wrote, tuple);
        }
        // An alias binds the same parameters, so `alias collect map` gets `map`'s parameter types.
        if let Some(parameters) = self.parameters.get(&means).cloned() {
            self.parameters.insert(wrote, parameters);
        }
        // And runs its blocks against the same `self`.
        if let Some(selves) = self.selves.get(&means).cloned() {
            self.selves.insert(wrote, selves);
        }
    }

    /// Give every Ruby `alias` and `alias_method` the rows of the method it renames.
    ///
    /// - **The Ruby version of [`Self::copy_rows`].** `Array#blank?` is the case: ActiveSupport
    ///   writes `alias_method :blank?, :empty?`, and `Array#empty? -> bool` is declared in
    ///   `vendor/rbs/core`.
    /// - **Read from the graph, not from a second Prism walk.** rubydex files each alias as a
    ///   `Definition::MethodAlias` carrying the old name, so no Ruby syntax lives in this module.
    /// - **Runs after the resolve**, beside [`place_generated_members`](super::Analysis). The
    ///   target's row exists only once every signature is harvested. The RBS half can run per
    ///   document because RBS writes an alias next to its method; Ruby does not.
    /// - **Never displaces a row the alias already has.** A signature naming the alias is stronger
    ///   evidence than the rename.
    /// - **One pass.** `alias a b` beside `alias b c` answers `b` and not `a`, the same bound the
    ///   RBS half has.
    pub fn adopt_aliases(&mut self, graph: &Graph) {
        for definition in graph.definitions().values() {
            let Definition::MethodAlias(alias) = definition else {
                continue;
            };
            let Some(wrote) = graph
                .definition_to_declaration_id(definition)
                .and_then(|id| graph.declarations().get(id))
                .map(Declaration::name)
            else {
                continue;
            };
            // A signature already states it, and that is the stronger fact: an `alias` says two
            // names are one method, while a `def:` line says what the method returns.
            if self.returns.contains_key(&DeclarationId::from(wrote)) {
                continue;
            }
            // Only the owner of `Array#blank?()` is wanted: the target is the same class under the
            // old name. A singleton owner keeps its `::<Name>`, because the `#` is the last one
            // either way.
            let Some((owner, _)) = wrote.rsplit_once('#') else {
                continue;
            };
            let Some(old) = graph
                .strings()
                .get(alias.old_name_str_id())
                .map(StringRef::as_str)
            else {
                continue;
            };
            // **rubydex's old name already carries parentheses** (`size()`), while `ruby_rbs` hands
            // over a bare `collect`. Both sides must be spelled the way rubydex keys a member
            // (`core-invariants.md`), so normalise here. Appending blindly gives `Widget#size()()`,
            // which matches nothing, silently.
            let means = format!("{owner}#{}()", old.trim_end_matches("()"));
            self.copy_rows(wrote, &means);
        }
    }

    fn insert_constant(&mut self, constant: &str, class: &str) {
        self.constants.insert(
            DeclarationId::from(constant),
            Held {
                constant: constant.into(),
                class: class.into(),
            },
        );
    }
}

/// One document's identity, as the number [`Contribution`] keeps.
///
/// A hash, not the string: there is one per method per document, and it is only compared for
/// equality. A collision would merge two documents' arms into one contribution.
fn document_key(document: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    document.hash(&mut hasher);
    hasher.finish()
}

/// The walk, carrying the lexical nesting RBS declarations sit in.
struct Harvest<'t> {
    /// The enclosing `class`/`module` declarations, qualified, outermost first.
    nesting: Vec<Declared>,
    /// `alias map collect`, as the two member keys, collected rather than acted on.
    ///
    /// A document does not declare in order: `array.rbs` writes `alias map collect` long after
    /// `def collect`. So the copy runs once the walk is done and every `def` has its rows. An alias
    /// whose target is in another document is not resolved; RBS writes an alias beside its method.
    aliases: Vec<(String, String)>,
    /// The document being read, for the one name the RBS tree gives only as a span.
    ///
    /// A keyword's name is a hash key whose node carries a location and nothing else, so `foo:` is
    /// read back out of the text. Every other name here is a `SymbolNode` with a string.
    source: &'t str,
    /// Which document this walk reads, hashed. [`Types::harvest`] computes it and [`Types::insert`]
    /// keys a contribution by it.
    document: u64,
    types: &'t mut Types,
}

/// One `class` or `module`, as the types written inside it see it.
///
/// - **The name** is what `instance` and `class` mean.
/// - **The parameters** are what a type variable means. `class Array[unchecked out E]` declares
///   `each: () { (E element) -> void }`, and `E` is "the receiver's first type argument", a fact
///   about the receiver.
struct Declared {
    name: String,
    /// The declaration's own type parameters, in the order it wrote them.
    parameters: Vec<Box<str>>,
}

/// What a type written inside a declaration can learn about that declaration.
///
/// Passed to [`class_of`] instead of a bare name, because the enclosing declaration answers two
/// kinds of type, not the type itself.
struct Owner<'a> {
    /// What `instance` and `class` name, and what a receiver must *be* for a type variable in here
    /// to mean anything.
    name: &'a str,
    /// The declaration's own type parameters, by position.
    ///
    /// - **Its own, never an ancestor's.** `Hash` includes `Enumerable[[K, V]]`, so `Enumerable`'s
    ///   `E` is the *pair*; read as the first argument, it would hand `hash.map { |x| }` a key. So
    ///   a position resolves against the declaration it was written in, and the lookup checks that
    ///   is the receiver's own class.
    /// - **Never a method's own.** A method's `[U]` is bound by the call, not the receiver, so a
    ///   variable missing from this list is refused.
    /// - **Empty** for a constant, and for an arm whose method shadows one of the class's names
    ///   ([`Owner::under`]).
    parameters: &'a [Box<str>],
    /// The type variable this arm's **block** returns, where the arm declares that variable itself.
    ///
    /// `None` everywhere except an arm's own return type, the one position a block's answer can be
    /// substituted into. See [`Return::Block`] and [`Owner::returning`].
    block_returns: Option<Box<str>>,
    /// The method's own type variables a call passes, by the position of the argument that binds
    /// each: `[X] (String, X) -> (String | X)` holds `X` at 1.
    ///
    /// Empty everywhere except an arm's own return type, like [`Self::block_returns`]; see
    /// [`Return::Argument`] and [`passed_variables`].
    passed: Box<[Option<Box<str>>]>,
}

impl<'a> Owner<'a> {
    /// The owner as one arm of a method sees it.
    ///
    /// A method may declare its own type parameter with a name the class already uses; inside that
    /// arm the name is the method's. Nothing in `vendor/rbs` does this. The arm drops the whole
    /// parameter list rather than the one name: shadowing is rare, and the cheap answer is the safe
    /// one.
    fn under(&self, method_type: &ruby_rbs::node::MethodTypeNode<'_>) -> Self {
        let shadowed = method_type
            .type_params()
            .iter()
            .any(|parameter| match parameter {
                Node::TypeParam(parameter) => {
                    let name = parameter.name();
                    self.parameters.iter().any(|held| &**held == name.as_str())
                }
                _ => false,
            });
        Self {
            name: self.name,
            parameters: if shadowed { &[] } else { self.parameters },
            block_returns: None,
            passed: Box::default(),
        }
    }

    /// The owner as this arm's **return type** sees it: [`Self::under`] plus the one variable a
    /// block can answer.
    ///
    /// Kept apart from `under` because the block's parameters and a tuple use the same owner and
    /// must not see this variable. Otherwise `{ (U) -> U }` would hand the block's parameter the
    /// block's own answer, a circle.
    fn returning(&self, method_type: &ruby_rbs::node::MethodTypeNode<'_>, source: &str) -> Self {
        Self {
            name: self.name,
            parameters: self.parameters,
            block_returns: block_variable(method_type),
            passed: passed_variables(method_type, source),
        }
    }
}

/// The type variable an arm's **block** returns, where the arm declares that variable itself.
///
/// The narrow half of [`Return::Block`], and mostly refusals. A block returning a class, `self`,
/// `void`, a union or the receiver's own parameter answers nothing, and so does an arm with no
/// block. What is left is `[U] () { (Elem) -> U } -> ...U...`, mostly `map` and `collect`.
fn block_variable(method_type: &ruby_rbs::node::MethodTypeNode<'_>) -> Option<Box<str>> {
    let Node::FunctionType(function) = method_type.block()?.type_() else {
        return None;
    };
    let Node::VariableType(variable) = function.return_type() else {
        return None;
    };
    let written = variable.name();
    let written = written.as_str();
    // **The method's own variable, never the class's.** `Enumerable#each_entry` yields and returns
    // the receiver's `Elem`; taking that as the block's answer would let a block decide what the
    // receiver holds. [`class_of`] asks the class's list first for the same reason.
    method_type
        .type_params()
        .iter()
        .any(|parameter| match parameter {
            Node::TypeParam(parameter) => parameter.name().as_str() == written,
            _ => false,
        })
        .then(|| written.into())
}

/// The method's own type variables a call's positional arguments bind, by position: `X` at 1 for
/// `[X] (String name, X default) -> (String | X)`.
///
/// **A variable is bound only where one argument says all of it**: it is the whole type of one
/// leading required positional and appears in no other parameter. `clamp`'s `[T] (T, T) -> T`
/// takes two, and the first alone would say nothing of the second; `(Array[X]) -> X` would ask
/// the argument for an element. A trailing positional's position depends on the call, so only the
/// leading ones count. The block's own parameters are not the method's and may repeat it.
fn passed_variables(
    method_type: &ruby_rbs::node::MethodTypeNode<'_>,
    source: &str,
) -> Box<[Option<Box<str>>]> {
    let Node::FunctionType(function) = method_type.type_() else {
        return Box::default();
    };
    let own: Vec<String> = method_type
        .type_params()
        .iter()
        .filter_map(|parameter| match parameter {
            Node::TypeParam(parameter) => Some(parameter.name().as_str().to_owned()),
            _ => None,
        })
        .collect();
    if own.is_empty() {
        return Box::default();
    }
    let spelled = |written: &Node<'_>| {
        let range = written.location();
        source
            .get(usize::try_from(range.start()).ok()?..usize::try_from(range.end()).ok()?)
            .map(str::to_owned)
    };
    fn typed<'n>(parameter: &Node<'n>) -> Option<Node<'n>> {
        match parameter {
            Node::FunctionParam(parameter) => Some(parameter.type_()),
            _ => None,
        }
    }
    // Every parameter's type as written, keywords and rests included, to count mentions in.
    let every: Vec<String> = function
        .required_positionals()
        .iter()
        .chain(function.optional_positionals().iter())
        .chain(function.rest_positionals())
        .chain(function.trailing_positionals().iter())
        .chain(function.required_keywords().iter().map(|(_, value)| value))
        .chain(function.optional_keywords().iter().map(|(_, value)| value))
        .chain(function.rest_keywords())
        .filter_map(|parameter| spelled(&typed(&parameter)?))
        .collect();
    let mentions = |name: &str| {
        every
            .iter()
            .map(|written| {
                written
                    .split(|character: char| !(character.is_alphanumeric() || character == '_'))
                    .filter(|word| *word == name)
                    .count()
            })
            .sum::<usize>()
    };
    function
        .required_positionals()
        .iter()
        .map(|parameter| {
            let Some(Node::VariableType(variable)) = typed(&parameter) else {
                return None;
            };
            let name = variable.name();
            let name = name.as_str();
            (own.iter().any(|held| held == name) && mentions(name) == 1).then(|| name.into())
        })
        .collect()
}

impl Visit for Harvest<'_> {
    fn visit_class_node(&mut self, node: &ClassNode<'_>) {
        let declared = Declared {
            name: self.qualify(&type_name(&node.name())),
            parameters: parameters_of(&node.type_params()),
        };
        self.nesting.push(declared);
        ruby_rbs::node::visit_class_node(self, node);
        self.nesting.pop();
    }

    fn visit_module_node(&mut self, node: &ModuleNode<'_>) {
        let declared = Declared {
            name: self.qualify(&type_name(&node.name())),
            parameters: parameters_of(&node.type_params()),
        };
        self.nesting.push(declared);
        ruby_rbs::node::visit_module_node(self, node);
        self.nesting.pop();
    }

    /// `alias map collect`: one member that is another member.
    ///
    /// - **rubydex indexes an alias as a member of the class that wrote it**, so `Array#map` hides
    ///   `Enumerable#map` exactly as Ruby does. Without a row under the alias' name, `[1, 2].map`,
    ///   `[1, 2].size`, `"x".slice(0)` and `{}.each_pair` would answer nothing.
    /// - **The kind comes from whether `self.` was written** (`alias self.parse self.load`). RBS
    ///   makes both sides the same kind, so one flag covers the pair.
    fn visit_alias_node(&mut self, node: &ruby_rbs::node::AliasNode<'_>) {
        let Some(owner) = self.nesting.last() else {
            return;
        };
        let receiver = if node.new_kind_location().is_some() {
            singleton_name(&owner.name)
        } else {
            owner.name.clone()
        };
        self.aliases.push((
            format!("{receiver}#{}()", node.new_name().as_str()),
            format!("{receiver}#{}()", node.old_name().as_str()),
        ));
    }

    /// Deliberately empty. **ya-lsp does not index RBS interfaces** (the same rule
    /// [`signatures`](super::signatures) enforces by editing the text), so their members have no
    /// declaration to key a row by. Harvesting them would file `_Rand#rand` under the enclosing
    /// scope, for a method that is not there.
    fn visit_interface_node(&mut self, _: &ruby_rbs::node::InterfaceNode<'_>) {}

    /// `ENV: RBS::Unnamed::ENVClass`: the one RBS declaration that types something other than a
    /// method.
    ///
    /// - **Only a class instance type is kept**, as [`class_of`] decides. `String | Symbol`,
    ///   `untyped` or a literal names no single class. [`Return::Same`], [`Return::Element`] and
    ///   [`Return::Collection`] need a receiver, and a constant has none.
    /// - **The owner passed is the constant's own name, and is never read.** Only `instance` and
    ///   `class` use it, and RBS refuses both in a constant's type: such a document does not parse
    ///   (`what_a_signature_says_a_constant_holds`).
    fn visit_constant_node(&mut self, node: &ruby_rbs::node::ConstantNode<'_>) {
        let name = self.qualify(&type_name(&node.name()));
        let owner = Owner {
            name: &name,
            parameters: &[],
            block_returns: None,
            passed: Box::default(),
        };
        // The type arguments are skipped: nothing steps off a constant's type to ask what it holds.
        let Some(Returned {
            of: Return::Class { name: held, .. },
            ..
        }) = class_of(&node.type_(), &owner)
        else {
            return;
        };
        self.types.insert_constant(&name, &held);
    }

    fn visit_attr_reader_node(&mut self, node: &AttrReaderNode<'_>) {
        self.attribute(&node.name(), &node.type_(), node.kind());
    }

    fn visit_attr_accessor_node(&mut self, node: &AttrAccessorNode<'_>) {
        self.attribute(&node.name(), &node.type_(), node.kind());
    }

    fn visit_method_definition_node(&mut self, node: &MethodDefinitionNode<'_>) {
        let Some(declared) = self.nesting.last() else {
            // A method at the top level of a signature file. RBS has no such thing, so there is
            // nothing to own it.
            return;
        };
        let owner = Owner {
            name: &declared.name,
            parameters: &declared.parameters,
            block_returns: None,
            passed: Box::default(),
        };
        let symbol = node.name();
        let name = symbol.as_str();

        // `def self?.foo` declares the method on both the instance side and the singleton, and
        // rubydex files two declarations. Both get the entry, because a call may reach either.
        let kinds: &[MethodDefinitionKind] = match node.kind() {
            MethodDefinitionKind::Instance => &[MethodDefinitionKind::Instance],
            MethodDefinitionKind::Singleton => &[MethodDefinitionKind::Singleton],
            MethodDefinitionKind::SingletonInstance => &[
                MethodDefinitionKind::Instance,
                MethodDefinitionKind::Singleton,
            ],
        };

        for kind in kinds {
            let is_singleton = matches!(kind, MethodDefinitionKind::Singleton);
            let receiver = if is_singleton {
                singleton_name(owner.name)
            } else {
                owner.name.to_owned()
            };
            let member = format!("{receiver}#{name}()");
            // The tables are filled independently. A method whose return is refused may still say
            // exactly what its block is handed: `each` is `() { (Story) -> void } -> void` all over
            // Ruby's signatures.
            if let Some(yields) = declared_yield(node, &owner) {
                self.types.insert_yield(&member, yields);
            }
            // The tuple table, independent for the same reason: a method whose return is refused
            // *because* it is a tuple is exactly the one this answers.
            if let Some(classes) = declared_tuple(node, &owner) {
                self.types.insert_tuple(&member, classes);
            }
            // The parameter table. A method whose return type nobody states may still say what it
            // is *handed*, as an application's own `def` often does.
            if let Some(parameters) = declared_parameters(node, &owner, self.source) {
                self.types.insert_parameters(&member, parameters);
            }
            // The block-`self` table, independent for the same reason: a DSL's method returns
            // nothing worth a type, and says what its block runs against.
            if let Some(selves) =
                declared_selves(node, &owner, self.source, member == MAKES_A_METHOD)
            {
                self.types.insert_selves(&member, selves);
            }
            // The Ruby `def` a generated member stands for, where the generator said which.
            if let Some(module) = declared_def(node) {
                self.types.insert_defined(&member, module);
            }
            // A member whose value is the block of the call that declared it.
            if declares(node, BLOCK) {
                self.types.insert_blocked(&member);
            }
            // A member whose value is the `def` written at its own place.
            if declares(node, OWN_DEF) {
                self.types.insert_bodied(&member);
            }
            // A member that calls the method its call names.
            if declares(node, SENT) {
                self.types.insert_sent(&member);
            }
            // A member that looks a key up in a body of knowledge's table.
            if declares(node, KEYED) {
                self.types
                    .keyed
                    .insert(DeclarationId::from(member.as_str()));
            }
            // A method whose every overload hands back `void`, or every one `bot`.
            if let Some(nothing) = declared_nothing(node) {
                self.types
                    .nothing
                    .insert(DeclarationId::from(member.as_str()), nothing);
            }
            // **No emptiness check here, on purpose.** Arms this document refuses are still
            // recorded: they are part of what the partitions must agree on once another document's
            // arms are read. [`Types::insert`] drops the row if the union settles to nothing.
            let (plain, with_block) = declared_return(node, &owner, self.source);
            self.types.insert(self.document, &member, plain, with_block);
        }
    }
}

impl Harvest<'_> {
    /// **An RBS reader returns its declared type**: `attr_reader title: String` is
    /// `def title: () -> String`, one arm that takes nothing.
    ///
    /// - **Why:** a gem's reader often exists only in its signature (aws-sdk's response fields are
    ///   `Struct` members in Ruby), and a Ruby reader of an untyped variable says nothing either.
    /// - **Keyed as a `def` is**, on the singleton for `attr_reader self.x`, which is how rubydex
    ///   files the declaration (`rubydex_spells_a_signature_the_way_this_keys_it`).
    /// - **An `attr_accessor`'s reader is the same row.** Its setter needs none: `obj.x = v` is `v`
    ///   by Ruby's rule (`attribute_written`), and `attr_writer` declares no reader.
    /// - **A custom variable name** (`attr_reader title (@raw): String`) changes what is stored,
    ///   not what the reader returns.
    fn attribute(&mut self, name: &SymbolNode<'_>, type_: &Node<'_>, kind: AttributeKind) {
        let Some(declared) = self.nesting.last() else {
            // At the top level of a signature file, as for a `def`: nothing owns it.
            return;
        };
        let owner = Owner {
            name: &declared.name,
            parameters: &declared.parameters,
            block_returns: None,
            passed: Box::default(),
        };
        let receiver = match kind {
            AttributeKind::Instance => owner.name.to_owned(),
            AttributeKind::Singleton => singleton_name(owner.name),
        };
        let member = format!("{receiver}#{}()", name.as_str());
        let arm = Arm {
            least: 0,
            most: Some(0),
            returns: class_of(type_, &owner),
            takes: Vec::new(),
            symbols: Vec::new(),
            keywords: false,
            required: Box::default(),
            named: Some(Box::default()),
        };
        self.types
            .insert(self.document, &member, vec![arm], Vec::new());
    }

    /// A declaration's name, under whatever it is written inside.
    fn qualify(&self, written: &Written) -> String {
        match (written.absolute, self.nesting.last()) {
            // `class ::Foo::Bar` names the top level however deep it is written.
            (true, _) | (false, None) => written.path.clone(),
            (false, Some(outer)) => format!("{}::{}", outer.name, written.path),
        }
    }
}

/// rubydex's name for a class's singleton: the qualified name, then the *unqualified* name in angle
/// brackets. `Shelf::Book` becomes `Shelf::Book::<Book>`.
///
/// This is rubydex's spelling, not a choice made here. `hover::attached_name` is the other place
/// that must know it.
fn singleton_name(owner: &str) -> String {
    let last = owner.rsplit("::").next().unwrap_or(owner);
    format!("{owner}::<{last}>")
}

/// A declaration's type parameters, by name and in the order they were written.
fn parameters_of(written: &ruby_rbs::node::NodeList<'_>) -> Vec<Box<str>> {
    written
        .iter()
        .filter_map(|parameter| match parameter {
            Node::TypeParam(parameter) => Some(parameter.name().as_str().into()),
            _ => None,
        })
        .collect()
}

/// What a generic was written holding, by position. The other half of [`parameters_of`].
///
/// - **One list is declared, the other filled in.** They meet at [`Return::Parameter`]: a position
///   along `class Array[E]`'s list is answered by the argument at that position of the receiver.
/// - **Each position is read with the same [`class_of`] as the whole type.** So in
///   `Array#first(n) -> Array[E]`, the `E` becomes a question about the receiver at the depth it
///   was written.
/// - **A position `class_of` refuses** (a tuple, an interface, `untyped`) is `None` and holds its
///   place, so later positions still count.
fn arguments_of(
    written: &ruby_rbs::node::NodeList<'_>,
    owner: &Owner<'_>,
) -> Box<[Option<Return>]> {
    // **An argument is held as a bare class**, so one that is not exactly a class is not held:
    // `Array[String?]` holding `String` would say its elements are never `nil`, and `bool` would
    // be held as `TrueClass`. The position stays, unknown.
    written
        .iter()
        .map(|argument| {
            class_of(&argument, owner)
                .filter(|returned| !returned.nilable && !matches!(returned.of, Return::Bool))
                .map(|returned| returned.of)
        })
        .collect()
}

/// An RBS type name as it was written: the path, and whether it led with `::`.
struct Written {
    path: String,
    absolute: bool,
}

fn type_name(name: &ruby_rbs::node::TypeNameNode<'_>) -> Written {
    let namespace = name.namespace();
    let mut path = String::new();
    for segment in namespace.path().iter() {
        if let Node::Symbol(symbol) = segment {
            path.push_str(symbol.as_str());
            path.push_str("::");
        }
    }
    path.push_str(name.name().as_str());
    Written {
        path,
        absolute: namespace.absolute(),
    }
}

/// What a method's overloads agree it returns, for each way a call can be written.
///
/// The arms are partitioned twice: by whether the call wrote a block, and by how many positional
/// arguments it wrote. Each partition must agree with itself. A partition whose arms name two
/// classes is a union and is dropped, and so is one holding a single refused arm.
///
/// - **`String#gsub`**: three arms of one arity (two `String`, one `Enumerator`), none taking a
///   block. Nothing to offer.
/// - **`String#bytes`** is told apart by the block, **`Float#round`** by the arity. Both answer.
fn declared_return(
    node: &MethodDefinitionNode<'_>,
    owner: &Owner<'_>,
    source: &str,
) -> (Vec<Arm>, Vec<Arm>) {
    let mut plain: Vec<Arm> = Vec::new();
    let mut with_block: Vec<Arm> = Vec::new();

    for method_type in method_types(node) {
        let arm = arm_of(&method_type, &owner.under(&method_type), source);

        match method_type.block() {
            // A required block: only a call *with* one reaches this arm.
            Some(block) if block.required() => with_block.push(arm),
            // `?{ ... }`: an optional block, so the arm applies either way.
            Some(_) => {
                plain.push(arm.clone());
                with_block.push(arm);
            }
            None => plain.push(arm),
        }
    }

    // **Raw, and settled one level out.** Another document may declare the same method on the same
    // class (bigdecimal, a default gem, reopens `Float` to declare `+`), so the partition runs over
    // every document's arms together. [`Types::insert`] is where they meet; see [`Contribution`].
    (plain, with_block)
}

/// What every arm of `node` that declares a block agrees its block is handed, by position.
///
/// - **`None`** where they disagree, where no arm has a block, or where the block's function type
///   is `(?) -> T` (the refusal [`arm_of`] makes).
/// - **Agreement is the whole safety argument.** `Enumerable#each_entry` yields an element in one
///   arm and an array of them in another; an answer that depends on which overload the reader meant
///   is no answer.
/// - **Required and optional blocks are both read.** The call that reaches this wrote a block, so
///   `?{ (Story) -> void }` says what that block receives as much as `{ (Story) -> void }` does.
fn declared_yield(
    node: &MethodDefinitionNode<'_>,
    owner: &Owner<'_>,
) -> Option<Box<[Option<Returned>]>> {
    let mut agreed: Option<Vec<Option<Returned>>> = None;
    for method_type in method_types(node) {
        let Some(block) = method_type.block() else {
            continue;
        };
        let Node::FunctionType(function) = block.type_() else {
            return None;
        };
        let under = owner.under(&method_type);
        let parameters: Vec<Option<Returned>> = function
            .required_positionals()
            .iter()
            .filter_map(|parameter| match parameter {
                Node::FunctionParam(parameter) => Some(class_of(&parameter.type_(), &under)),
                _ => None,
            })
            .collect();
        match &agreed {
            Some(held) if *held != parameters => return None,
            Some(_) => {}
            None => agreed = Some(parameters),
        }
    }
    agreed
        .filter(|parameters| parameters.iter().any(Option::is_some))
        .map(Vec::into_boxed_slice)
}

/// A keyword's name, read from the signature's source text.
///
/// RBS gives a keyword parameter's key as a node with a location and no string, so this is the one
/// name the harvest reads from the source. The trailing colon is trimmed, as `scopes::record` does:
/// it is punctuation, not part of the name.
fn keyword_name(source: &str, key: &Node<'_>) -> Option<Box<str>> {
    let range = key.location();
    let (start, end) = (
        usize::try_from(range.start()).ok()?,
        usize::try_from(range.end()).ok()?,
    );
    let written = source.get(start..end)?.trim_end_matches(':');
    (!written.is_empty()).then(|| written.into())
}

/// What a method's overloads agree its **own parameters** are.
///
/// [`declared_yield`]'s shape on the other side of the arrow: iterate the arms, require agreement,
/// refuse otherwise. This is the only table asked from inside a body rather than at a call site.
///
/// 1. **Required and optional positionals, and keywords. Nothing else.** `*rest`, `**rest` and
///    `&block` type the container (`Array`, `Hash`, `Proc`), not the value. Positionals after a
///    rest are refused too: their position depends on the call. [`cursor::ParameterSlot`] makes the
///    same three refusals, so the two lists line up by construction.
/// 2. **Every arm must agree, and a different arity disagrees.** `(String) -> void` beside
///    `(Integer, Integer) -> void` has no single answer for position zero. Picking the longest arm
///    is the "nearest arm" guess this module refuses.
/// 3. **`untyped` is a `None` slot, not a refusal.** A signature that types three of four
///    parameters is worth three answers, and dropping the fourth would renumber the rest.
fn declared_parameters(
    node: &MethodDefinitionNode<'_>,
    owner: &Owner<'_>,
    source: &str,
) -> Option<Parameters> {
    let mut agreed: Option<Parameters> = None;
    for method_type in method_types(node) {
        let Node::FunctionType(function) = method_type.type_() else {
            return None;
        };
        let under = owner.under(&method_type);
        let mut positional: Vec<Option<Returned>> = Vec::new();
        for parameter in function
            .required_positionals()
            .iter()
            .chain(function.optional_positionals().iter())
        {
            match parameter {
                Node::FunctionParam(parameter) => {
                    positional.push(class_of(&parameter.type_(), &under));
                }
                // A positional RBS spells another way holds its place, so later ones keep their
                // numbers.
                _ => positional.push(None),
            }
        }
        let mut keyword: Vec<(Box<str>, Returned)> = Vec::new();
        for (key, declared) in function
            .required_keywords()
            .iter()
            .chain(function.optional_keywords().iter())
        {
            let Node::FunctionParam(parameter) = declared else {
                continue;
            };
            let (Some(name), Some(returned)) = (
                keyword_name(source, &key),
                class_of(&parameter.type_(), &under),
            ) else {
                continue;
            };
            keyword.push((name, returned));
        }
        keyword.sort_by(|left, right| left.0.cmp(&right.0));
        let arm = Parameters {
            positional: positional.into_boxed_slice(),
            keyword: keyword.into_boxed_slice(),
        };
        match &agreed {
            Some(held) if held.positional != arm.positional || held.keyword != arm.keyword => {
                return None;
            }
            Some(_) => {}
            None => agreed = Some(arm),
        }
    }
    agreed.filter(|parameters| {
        parameters.positional.iter().any(Option::is_some) || !parameters.keyword.is_empty()
    })
}

/// What every arm of `node` agrees `self` is inside each block or lambda it takes, by the argument
/// it arrives as ([`Types::selves`]).
///
/// - **Three places RBS writes a self binding:** the method's block (`{ () [self: T] -> void }`),
///   and a proc-typed positional or keyword (`^() [self: T] -> untyped`), optional or not, alone or
///   the one proc of a union. A `**rest` of proc type binds every keyword not declared by name.
/// - **An arm that binds nothing at a slot does not vote**, since a call reaching that arm would
///   not pass a block there. Two arms binding one slot differently make it [`Rebound::Unread`]:
///   which one a call reached is not known here, and either says the body's `self` is wrong.
/// - **`[self: instance]` is kept apart from the class it would name** ([`Rebound::Instance`]):
///   read against the receiver, as RBS reads it, and not against the declaration it is written in,
///   which is what [`class_of`] does for a return.
/// - **`makes_a_method`** ([`MAKES_A_METHOD`]) reads every binding the method writes as
///   `instance`: what it is handed runs as a method of the receiver.
fn declared_selves(
    node: &MethodDefinitionNode<'_>,
    owner: &Owner<'_>,
    source: &str,
    makes_a_method: bool,
) -> Option<Box<[(SelfSlot, Rebound)]>> {
    let mut agreed: Vec<(SelfSlot, Rebound)> = Vec::new();
    let mut vote = |slot: SelfSlot, rebound: Rebound| match agreed
        .iter_mut()
        .find(|(held, _)| *held == slot)
    {
        Some((_, held)) if *held != rebound => *held = Rebound::Unread,
        Some(_) => {}
        None => agreed.push((slot, rebound)),
    };
    for method_type in method_types(node) {
        let under = owner.under(&method_type);
        let read = |written: &Node<'_>| {
            if makes_a_method {
                Rebound::Instance
            } else {
                rebound_of(written, &under)
            }
        };
        if let Some(written) = method_type.block().and_then(|block| block.self_type()) {
            vote(SelfSlot::Block, read(&written));
        }
        let Node::FunctionType(function) = method_type.type_() else {
            continue;
        };
        for (index, parameter) in function
            .required_positionals()
            .iter()
            .chain(function.optional_positionals().iter())
            .enumerate()
        {
            if let Some(written) = proc_self(&parameter) {
                vote(SelfSlot::Positional(index), read(&written));
            }
        }
        for (key, parameter) in function
            .required_keywords()
            .iter()
            .chain(function.optional_keywords().iter())
        {
            if let (Some(name), Some(written)) = (keyword_name(source, &key), proc_self(&parameter))
            {
                vote(SelfSlot::Keyword(name), read(&written));
            }
        }
        if let Some(written) = function
            .rest_keywords()
            .and_then(|parameter| proc_self(&parameter))
        {
            vote(SelfSlot::AnyKeyword, read(&written));
        }
    }
    (!agreed.is_empty()).then(|| agreed.into_boxed_slice())
}

/// The `[self: T]` a proc-typed parameter writes (`^() [self: T] -> untyped`, that type made
/// optional, or the one proc type of a union: `define_method` takes a proc, a `Method` or an
/// `UnboundMethod`).
fn proc_self<'a>(parameter: &Node<'a>) -> Option<Node<'a>> {
    let Node::FunctionParam(parameter) = parameter else {
        return None;
    };
    let written = match parameter.type_() {
        Node::OptionalType(optional) => optional.type_(),
        other => other,
    };
    let proc = match written {
        Node::ProcType(proc) => proc,
        Node::UnionType(union) => {
            let mut procs = union.types().iter().filter_map(|member| match member {
                Node::ProcType(proc) => Some(proc),
                _ => None,
            });
            match (procs.next(), procs.next()) {
                (Some(proc), None) => proc,
                _ => return None,
            }
        }
        _ => return None,
    };
    proc.self_type()
}

/// A `[self: T]` read: `instance` against the receiver ([`Rebound::Instance`]), and anything else
/// as a return is read. A `self` that may be `nil` is [`Rebound::Unread`]: a block runs against an
/// object.
fn rebound_of(written: &Node<'_>, owner: &Owner<'_>) -> Rebound {
    if let Node::InstanceType(_) = written {
        return Rebound::Instance;
    }
    class_of(written, owner)
        .filter(|returned| !returned.nilable)
        .map_or(Rebound::Unread, Rebound::Returned)
}

/// The module a generated member's [`generated::DEFINED`] return names, the one type argument
/// written absolute: `AnsweredByItsDef[::Account::Finder]` is `Account::Finder`.
///
/// One overload only, as the generator writes it; anything else is not the sentinel.
fn declared_def(node: &MethodDefinitionNode<'_>) -> Option<Box<str>> {
    let mut overloads = method_types(node);
    let (Some(method_type), None) = (overloads.next(), overloads.next()) else {
        return None;
    };
    let Node::FunctionType(function) = method_type.type_() else {
        return None;
    };
    let Node::ClassInstanceType(class) = function.return_type() else {
        return None;
    };
    let written = type_name(&class.name());
    if written.absolute || written.path != DEFINED {
        return None;
    }
    let arguments: Vec<Node<'_>> = class.args().iter().collect();
    let [Node::ClassInstanceType(module)] = arguments.as_slice() else {
        return None;
    };
    let module = type_name(&module.name());
    module.absolute.then(|| module.path.into())
}

/// [`Nothing`] where every overload of a signature returns `void`, or every one `bot`.
fn declared_nothing(node: &MethodDefinitionNode<'_>) -> Option<Nothing> {
    let mut agreed: Option<Nothing> = None;
    for method_type in method_types(node) {
        let Node::FunctionType(function) = method_type.type_() else {
            return None;
        };
        let nothing = match function.return_type() {
            Node::VoidType(_) => Nothing::Void,
            Node::BottomType(_) => Nothing::Bot,
            _ => return None,
        };
        if agreed.is_some_and(|agreed| agreed != nothing) {
            return None;
        }
        agreed = Some(nothing);
    }
    agreed
}

/// Whether a call of `found` never returns: no signature types it, and every Ruby `def` of it
/// raises on every path, so none has an exit ([`cursor`] files none for a `raise` or `fail`).
///
/// **Syntax only**, as the exit walk is: a `def` whose last line calls a method that always raises
/// is not one. The margin draws `-> bot` for it ([`super::hints`]), and the card says so.
#[must_use]
pub fn never_returns(sources: &Sources<'_>, found: DeclarationId) -> bool {
    if sources.types.returns.contains_key(&found) {
        return false;
    }
    let graph = sources.graph;
    let written: Vec<(String, u32, u32)> = locator::definitions_of(graph, found)
        .iter()
        .filter(|definition| matches!(definition, Definition::Method(_)))
        .filter_map(|definition| {
            let uri = graph.documents().get(definition.uri_id())?.uri().to_owned();
            let offset = definition.offset();
            (!uri.ends_with(".rbs")).then(|| (uri, offset.start(), offset.end()))
        })
        .collect();
    !written.is_empty()
        && written.iter().all(|(uri, start, end)| {
            let exits = || {
                let document = sources.memo.reads.documents.of(uri, sources.read)?;
                let span = document.rebase.span_to_buffer(ByteSpan {
                    start: *start,
                    end: *end,
                })?;
                Some(
                    document
                        .shapes(uri, sources.held_exits)
                        .returns
                        .get(&(span.start, span.end))?
                        .is_empty(),
                )
            };
            exits().unwrap_or(false)
        })
}

/// Whether a generated member returns `sentinel` ([`generated::BLOCK`], [`generated::OWN_DEF`]):
/// one overload, the sentinel written bare
/// and with no arguments, as the generator writes it. Anything else is not the sentinel.
fn declares(node: &MethodDefinitionNode<'_>, sentinel: &str) -> bool {
    let mut overloads = method_types(node);
    let (Some(method_type), None) = (overloads.next(), overloads.next()) else {
        return false;
    };
    let Node::FunctionType(function) = method_type.type_() else {
        return false;
    };
    let Node::ClassInstanceType(class) = function.return_type() else {
        return false;
    };
    let written = type_name(&class.name());
    !written.absolute && written.path == sentinel && class.args().iter().next().is_none()
}

/// What a method's overloads agree it returns **as a tuple**, by position.
///
/// [`declared_yield`]'s shape, read off the return type. `IO.pipe` (`() -> [IO, IO]`) is why, and a
/// destructure on its left is the only caller.
///
/// - **Every element must be a plain class, or the whole tuple is refused.** Positions count from
///   the left, so a hole at one silently shifts two, and a reader of `a, b = f` cannot see a hole.
/// - **Only arms a blockless call reaches.** `IO.pipe` also has
///   `[X] (...) { ([IO, IO]) -> X } -> X`; reading both would refuse the method this exists for. A
///   required block's arm is skipped, and an optional `?{ ... }` still has to agree, as in
///   [`declared_return`].
/// - **The skip is sound** because [`returned_element`] refuses a call that wrote a block.
fn declared_tuple(node: &MethodDefinitionNode<'_>, owner: &Owner<'_>) -> Option<Box<[Box<str>]>> {
    let mut agreed: Option<Vec<Box<str>>> = None;
    for method_type in method_types(node) {
        if method_type.block().is_some_and(|block| block.required()) {
            continue;
        }
        let Node::FunctionType(function) = method_type.type_() else {
            return None;
        };
        // `[String, Integer]?` is a tuple the way `String?` is a class: the module's one inexact
        // entry. A call that returned `nil` has no members at any position, and the mark makes that
        // visible. `Method#source_location` is declared this way.
        let returned = match function.return_type() {
            Node::OptionalType(optional) => optional.type_(),
            other => other,
        };
        let Node::TupleType(tuple) = returned else {
            return None;
        };
        let under = owner.under(&method_type);
        let mut classes = Vec::new();
        for element in tuple.types().iter() {
            match class_of(&element, &under) {
                Some(Returned {
                    of: Return::Class { name, .. },
                    ..
                }) => classes.push(name),
                // `self`, `instance`, a union, a type variable (including the receiver's own type
                // argument): not one class whose members can be offered. A tuple has no `None`
                // slot, so any of these refuses it.
                _ => return None,
            }
        }
        match &agreed {
            Some(held) if *held != classes => return None,
            Some(_) => {}
            None => agreed = Some(classes),
        }
    }
    agreed
        .filter(|classes| !classes.is_empty())
        .map(Vec::into_boxed_slice)
}

/// One overload, as a partition needs it: which calls reach it, and what it returns.
#[derive(Clone, Debug)]
struct Arm {
    /// The fewest positional arguments a call must write to reach this arm.
    least: usize,
    /// The most it may write, or `None` for a rest parameter, which has no most.
    most: Option<usize>,
    /// `None` where the policy refuses the return type. The arm still counts: a partition with one
    /// usable arm and one silent arm has no answer.
    returns: Option<Returned>,
    /// What each positional parameter is declared to be, in written order.
    ///
    /// - **What the partition cannot see.** [`settle`] separates arms by how many things they take.
    ///   `Integer#+: (Integer) -> Integer` and bigdecimal's `(BigDecimal) -> BigDecimal` both take
    ///   one, so the bucket disagrees and `1 + 2` has no type. The parameter tells them apart. It
    ///   is read once, at harvest, since the RBS source is not kept.
    /// - **Empty means the arm may not be picked.** That is every arm with an optional or a rest
    ///   positional: given fewer arguments than parameters, `(Integer, ?String, Symbol)` binds the
    ///   `Symbol` at position one, so the positions have no fixed map. Required and trailing
    ///   positionals line up exactly, which covers every operator this is for.
    /// - **`None` at a position** is a parameter that is not one class (a union, an interface,
    ///   `untyped`, an alias). The slot stays, as in [`Parameters`], and such an arm is never
    ///   picked: a parameter this cannot read is one it cannot rule out.
    takes: Vec<Option<Returned>>,
    /// [`Self::takes`]' positions again, where a parameter is written as **one symbol** (`title`
    /// for `(:title)`), as `untyped`, or as anything else ([`LiteralSlot`]). Empty where `takes`
    /// is, except for an arm whose every optional and rest positional is `untyped` and that has no
    /// trailing one: its required positionals come first at every call, so they line up
    /// (`(:user, untyped amount, *untyped) -> Array[User]`).
    ///
    /// `takes` holds a symbol literal's class, `Symbol`, which every symbol argument fits, so it
    /// cannot tell `(:title) -> String` from `(:id) -> Integer`. The name can
    /// ([`pick_by_literal`]).
    symbols: Vec<LiteralSlot>,
    /// Whether the arm takes keywords (required, optional or `**rest`), so a keyword hash a call
    /// writes is keywords to it, not one more positional.
    keywords: bool,
    /// The keywords a call must write to reach this arm, by name without the colon. Ruby raises
    /// on a call missing one, so a call that writes no keywords never runs such an arm.
    required: Names,
    /// Every keyword the arm takes by name, or `None` where a `**rest` takes any name. A call
    /// writing a name outside the list raises too.
    named: Option<Names>,
}

/// Every arm one **document** wrote for one method, kept raw so another document merges with it
/// instead of replacing it.
///
/// - **Why:** `vendor/rbs/core/float.rbs` declares
///   `Float#+: (Complex) -> Complex | (Numeric) -> Float`, and bigdecimal (a default gem) reopens
///   `Float` to declare `def +: (BigDecimal) -> BigDecimal`. With one `Overloads` per method, the
///   last read wins, and `1.5 + 1` answers `BigDecimal` in every project. Any method two loaded RBS
///   sources declare on one class has the same problem.
/// - **Raw arms, not the settled answer.** [`settle`] partitions by block and arity. Merging two
///   settled [`Arms`] cannot reproduce that: an empty partition and a disagreeing one are both
///   `None`, so one document's silence would erase the other's answer. Settling the union is the
///   single-document computation, one level out.
/// - **Keyed by document** so that re-harvesting a document **replaces** its arms. Otherwise an
///   edited `sig/*.rbs` would leave both its old and new claims. This is the invariant
///   [`Types::harvest`] documents.
#[derive(Debug)]
struct Contribution {
    /// The document these arms were read from, hashed. See [`Types::harvest`].
    document: u64,
    plain: Vec<Arm>,
    with_block: Vec<Arm>,
}

/// Every method type in a `def:`'s overload list.
///
/// The two `else` arms are unreachable: `overloads()` yields only `MethodDefinitionOverload`, and
/// each holds only a `MethodType`. Doing the narrowing once here keeps four callers from each
/// carrying the unreachable pair. `coverage.md` excludes this file from the 100% list for this kind
/// of arm.
fn method_types<'a>(
    node: &MethodDefinitionNode<'a>,
) -> impl Iterator<Item = ruby_rbs::node::MethodTypeNode<'a>> {
    node.overloads()
        .iter()
        .filter_map(|overload| {
            let Node::MethodDefinitionOverload(overload) = overload else {
                return None;
            };
            let Node::MethodType(method_type) = overload.method_type() else {
                return None;
            };
            Some(method_type)
        })
        .collect::<Vec<_>>()
        .into_iter()
}

/// One arm's shape, read off its method type.
///
/// rbs's `UntypedFunctionType`, `(?) -> T`, is refused on purpose. It has a return type, but it has
/// given up on its parameters and its arity. It would reach every partition and poison each one.
fn arm_of(
    method_type: &ruby_rbs::node::MethodTypeNode<'_>,
    owner: &Owner<'_>,
    source: &str,
) -> Arm {
    let Node::FunctionType(function) = method_type.type_() else {
        return Arm {
            least: 0,
            most: None,
            returns: None,
            takes: Vec::new(),
            symbols: Vec::new(),
            keywords: true,
            required: Box::default(),
            named: None,
        };
    };
    // Trailing positionals (`(?String, Integer)`) are required, like leading ones. RBS lists them
    // separately only to mark where the optional ones went.
    let least = function.required_positionals().iter().count()
        + function.trailing_positionals().iter().count();
    // The return type is the one position a block's answer can be substituted into, so only it is
    // read with that owner. See [`Owner::returning`].
    let owner = owner.returning(method_type, source);
    let fixed = function.rest_positionals().is_none()
        && function.optional_positionals().iter().next().is_none();
    let (required, named) = keywords_of(&function, source);
    let positionals = || {
        function
            .required_positionals()
            .iter()
            .chain(function.trailing_positionals().iter())
            .map(|parameter| match parameter {
                Node::FunctionParam(parameter) => Some(parameter.type_()),
                _ => None,
            })
    };
    Arm {
        least,
        most: (function.rest_positionals().is_none())
            .then(|| least + function.optional_positionals().iter().count()),
        returns: returned_type(&function.return_type(), &owner),
        // Read against the method type's own owner, not the returning one. A parameter written
        // `self` still means the receiver, and the block substitution has no bearing on what goes
        // in.
        takes: if fixed {
            positionals()
                .map(|written| class_of(&written?, &owner))
                .collect()
        } else {
            Vec::new()
        },
        symbols: if fixed {
            positionals()
                .map(|written| {
                    written.map_or(LiteralSlot::Other, |written| literal_slot(&written, source))
                })
                .collect()
        } else if open_after_required(&function) {
            function
                .required_positionals()
                .iter()
                .map(|parameter| match parameter {
                    Node::FunctionParam(parameter) => literal_slot(&parameter.type_(), source),
                    _ => LiteralSlot::Other,
                })
                .collect()
        } else {
            Vec::new()
        },
        keywords: function.required_keywords().iter().next().is_some()
            || function.optional_keywords().iter().next().is_some()
            || function.rest_keywords().is_some(),
        required,
        named,
    }
}

/// Whether everything after a function's required positionals takes anything: no trailing
/// positional, and every optional and rest one `untyped`. Only then do the required ones line up
/// with a call's first arguments whatever else it passes ([`Arm::symbols`]).
fn open_after_required(function: &ruby_rbs::node::FunctionTypeNode<'_>) -> bool {
    let untyped = |parameter: &Node<'_>| matches!(parameter, Node::FunctionParam(parameter) if matches!(parameter.type_(), Node::AnyType(_)));
    function.trailing_positionals().iter().next().is_none()
        && function
            .optional_positionals()
            .iter()
            .all(|parameter| untyped(&parameter))
        && function
            .rest_positionals()
            .is_none_or(|parameter| untyped(&parameter))
}

/// Keyword names, without their colons.
type Names = Box<[Box<str>]>;

/// An arm's keywords by name: which a call must write ([`Arm::required`]), and every name it takes
/// ([`Arm::named`]).
///
/// A name the source cannot give makes the arm rule nothing out: none is required, and any name
/// is taken, as with `**rest`.
fn keywords_of(
    function: &ruby_rbs::node::FunctionTypeNode<'_>,
    source: &str,
) -> (Names, Option<Names>) {
    let required: Option<Vec<Box<str>>> = function
        .required_keywords()
        .iter()
        .map(|(key, _)| keyword_name(source, &key))
        .collect();
    let optional: Option<Vec<Box<str>>> = function
        .optional_keywords()
        .iter()
        .map(|(key, _)| keyword_name(source, &key))
        .collect();
    let (Some(required), Some(optional)) = (required, optional) else {
        return (Box::default(), None);
    };
    let named = function
        .rest_keywords()
        .is_none()
        .then(|| required.iter().chain(&optional).cloned().collect());
    (required.into_boxed_slice(), named)
}

/// An arm's return type: [`class_of`], and a tuple as the `Array` it is.
///
/// - **A tuple is an `Array` of fixed length**, so a call returning `[Integer, Integer]` returns an
///   `Array`. `Kernel#Array` is why: `(nil) -> [] | (array[T]) -> Array[T] | (T) -> [T]` answers an
///   `Array` on every arm, and read as three answers of which two are refused, `Array(x)` answered
///   nothing.
/// - **What it holds is not said here.** Each position is the tuple table's
///   ([`declared_tuple`]), read at a destructure.
/// - **Only a return.** A block's parameter written as a tuple is one value Ruby unpacks across
///   several parameters (`each { |k, v| }`); read as an `Array` it would hand `k` the pair.
fn returned_type(written: &Node<'_>, owner: &Owner<'_>) -> Option<Returned> {
    match written {
        Node::TupleType(_) => Some(Returned::plain(Return::class("Array"))),
        Node::OptionalType(optional) if matches!(optional.type_(), Node::TupleType(_)) => {
            Some(Returned::plain(Return::class("Array")).or_nil())
        }
        _ => class_of(written, owner),
    }
}

/// One positional of an arm, as [`pick_by_literal`] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LiteralSlot {
    /// Written as one symbol literal: `title` for `(:title)`.
    Symbol(Box<str>),
    /// `untyped`, which takes any argument, so it rules none in or out.
    Anything,
    /// Any other type, which may or may not take an argument: this cannot tell.
    Other,
}

/// What one positional parameter is to [`pick_by_literal`].
fn literal_slot(written: &Node<'_>, source: &str) -> LiteralSlot {
    if matches!(written, Node::AnyType(_)) {
        return LiteralSlot::Anything;
    }
    symbol_named(written, source).map_or(LiteralSlot::Other, LiteralSlot::Symbol)
}

/// The name a parameter written as **one symbol literal** names: `title` for `(:title)`.
///
/// Read off the source, since the parser keeps only the literal's place. A quoted symbol
/// (`:"a b"`) names nothing here: the unquoted spelling is the only one a generator writes.
fn symbol_named(written: &Node<'_>, source: &str) -> Option<Box<str>> {
    let Node::LiteralType(literal) = written else {
        return None;
    };
    let Node::Symbol(symbol) = literal.literal() else {
        return None;
    };
    let range = symbol.location();
    let spelled =
        source.get(usize::try_from(range.start()).ok()?..usize::try_from(range.end()).ok()?)?;
    let name = spelled.strip_prefix(':')?;
    (!name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
    .then(|| name.into())
}

/// The arms on one side of the block, partitioned by arity, each partition settled.
///
/// - **`by_arity`** runs to the largest arity any arm names exactly. Past that, every arm that
///   still applies is a rest arm, and they all answer the same thing: `beyond`.
/// - **`any`** is every arm together, for a call whose arity cannot be counted.
fn settle(arms: &[Arm]) -> Arms {
    // A rest arm names its `least` exactly and everything above it through `beyond`, so it decides
    // how far the buckets run.
    let widest = arms.iter().map(|arm| arm.most.unwrap_or(arm.least)).max();
    // Which names a keyword call writes is not known here, so a keyed bucket holds every arm that
    // takes keywords at that count ([`Arm::reached_by`] with no names).
    let bucket = |arity: Arity| agreed(arms.iter().filter(move |arm| arm.reached_by(arity, None)));
    let buckets = |arity: fn(u32) -> Arity| {
        widest.map_or_else(Vec::new, |widest| {
            (0..=widest)
                .map(|written| bucket(arity(u32::try_from(written).unwrap_or(u32::MAX))))
                .collect()
        })
    };
    let rest = |arm: &&Arm| arm.most.is_none();
    Arms {
        by_arity: buckets(Arity::Exactly),
        beyond: agreed(
            arms.iter()
                .filter(rest)
                .filter(|arm| arm.required.is_empty()),
        ),
        any: agreed(arms.iter()),
        keyed: buckets(Arity::Keyed),
        keyed_beyond: agreed(arms.iter().filter(rest)),
    }
}

/// Whether a positional declared as `declared` can be handed a keyword hash: a `Hash`, a class a
/// `Hash` is, or a type this cannot read.
fn takes_a_hash(declared: &Option<Returned>) -> bool {
    match declared {
        Some(Returned {
            of: Return::Class { name, .. },
            ..
        }) => matches!(&**name, "Hash" | "Object" | "BasicObject"),
        _ => true,
    }
}

/// What a set of arms agrees on, or `None` where they disagree. No arms at all is a partition no
/// call reaches, and gets `None` too.
fn agreed<'a>(arms: impl Iterator<Item = &'a Arm>) -> Option<Returned> {
    let mut agreement = Agreement::default();
    for arm in arms {
        agreement.saw(arm.returns.clone());
    }
    agreement.settled()
}

impl Arm {
    fn accepts(&self, written: usize) -> bool {
        self.least <= written && self.most.is_none_or(|most| written <= most)
    }

    /// Whether a call written this way can run this arm, by Ruby's binding.
    ///
    /// - **Positionals by count.** A splat (`Arity::Unknown`) rules no arm out.
    /// - **No keywords written:** an arm that requires one raises, so it is not reached.
    ///   `CSV.read(path)` never runs the `headers:` arm.
    /// - **A keyword hash is keywords to an arm that takes them**: each required name must be
    ///   written, and each written name must be one the arm takes, unless a `**rest` takes any.
    ///   `keywords` are the names the call wrote, `None` where they cannot be read (`**opts`),
    ///   which rules nothing out by name.
    /// - **And one more positional to any other arm**: a `Hash`, so an arm that declares that
    ///   position as another class cannot receive it.
    /// - **`**opts` alone may be empty** ([`Arity::Spread`]), and then passes nothing, so it reaches
    ///   the arms of its plain count too.
    fn reached_by(&self, arity: Arity, keywords: Option<&[(String, Receiver)]>) -> bool {
        let written = |count: u32| usize::try_from(count).unwrap_or(usize::MAX);
        match arity {
            Arity::Unknown => true,
            Arity::Spread(count) => {
                self.reached_by(Arity::Keyed(count), None)
                    || self.reached_by(Arity::Exactly(count), None)
            }
            Arity::Exactly(count) => self.accepts(written(count)) && self.required.is_empty(),
            Arity::Keyed(count) if !self.keywords => {
                let count = written(count);
                self.accepts(count.saturating_add(1))
                    && self.takes.get(count).is_none_or(takes_a_hash)
            }
            Arity::Keyed(count) => {
                let wrote = |name: &str| {
                    keywords.is_none_or(|keywords| keywords.iter().any(|(key, _)| key == name))
                };
                let takes = |key: &str| {
                    self.named
                        .as_ref()
                        .is_none_or(|named| named.iter().any(|name| &**name == key))
                };
                self.accepts(written(count))
                    && self.required.iter().all(|name| wrote(name))
                    && keywords.is_none_or(|keywords| keywords.iter().all(|(key, _)| takes(key)))
            }
        }
    }
}

/// One side of the block, collecting arms until they disagree.
#[derive(Default)]
struct Agreement {
    held: Option<Returned>,
    /// Set by a refused arm, or by two arms naming different classes. Sticky: a later agreeing arm
    /// does not undo it.
    broken: bool,
}

impl Agreement {
    /// **Compare the class; fold everything beside it.**
    ///
    /// - `-> String` beside `-> String?` is one method that sometimes returns `nil`, so the arms
    ///   settle on `String?`.
    /// - `-> Array[String]` beside `-> Array[Integer]` is one method returning an `Array`, so they
    ///   settle on the head and drop the differing position ([`Return::agreed`]).
    ///
    /// Comparing whole values would drop both arms and lose the label.
    fn saw(&mut self, returns: Option<Returned>) {
        match (returns, &mut self.held) {
            (None, _) => self.broken = true,
            (Some(seen), Some(held)) => match held.of.agreed(&seen.of) {
                Some(of) => {
                    held.of = of;
                    held.nilable |= seen.nilable;
                }
                None => self.broken = true,
            },
            (Some(seen), None) => self.held = Some(seen),
        }
    }

    fn settled(self) -> Option<Returned> {
        if self.broken { None } else { self.held }
    }
}

/// The class an RBS type names, under the policy in the module docs.
///
/// `owner` is what `instance` and `class` mean. They are resolved here, not stored, so a lookup is
/// just a hash. `self` cannot be resolved here; see [`Return::Same`].
fn class_of(node: &Node<'_>, owner: &Owner<'_>) -> Option<Returned> {
    match node {
        // **Head and arguments answer two questions; both are kept.** The head is what a `.` gets.
        // The element is what the next call's block gets: it is how
        // `"a,b".split(",").each { |part| ... }` knows `part` is a `String`.
        Node::ClassInstanceType(class) => {
            let written = type_name(&class.name());
            // The two names only the query interface writes. They are read here because this is the
            // one place an RBS type becomes a [`Return`]; recognising them anywhere else would be a
            // second table.
            //
            // Both are a whole type written where a class name goes, so neither takes arguments.
            // Only the real-class arm reads them.
            match written.path.as_str() {
                ELEMENT => Some(Returned::plain(Return::Element)),
                COLLECTION => Some(Returned::plain(Return::Collection)),
                WRITTEN => Some(Returned::plain(Return::Written)),
                SHARED => Some(Returned::plain(Return::Shared)),
                HELD => Some(Returned::plain(Return::Held)),
                SCOPED => Some(Returned::plain(Return::Scoped)),
                // Answered at the body seam from its own table, never as an arm: see
                // [`Types::defined`].
                DEFINED | BLOCK | OWN_DEF | SENT | KEYED => None,
                FORWARDED => forwarding(&class.args()).map(Returned::plain),
                _ => Some(Returned::plain(Return::Class {
                    name: written.path.into(),
                    // `::Foo` is absolute and searches nowhere else. A bare `Foo` is searched from
                    // here outward; see [`declared_in`].
                    scope: if written.absolute {
                        Box::default()
                    } else {
                        owner.name.into()
                    },
                    arguments: arguments_of(&class.args(), owner),
                })),
            }
        }
        // `String?` is `String | nil`, and nobody typing a `.` after it wants `nil`'s members. So
        // the class is the answer and `nil` becomes a mark on it. The table's one inexact entry,
        // and the mark is how the inexactness is **shown**.
        Node::OptionalType(optional) => Some(class_of(&optional.type_(), owner)?.or_nil()),
        // `bool` is `true | false`, the one union worth folding: Ruby has no class for it, every
        // Ruby reader knows the word, and either half answers a `.` identically.
        Node::BoolType(_) => Some(Returned::plain(Return::Bool)),
        // A written-out union. Only two shapes survive, both folds this module has a word for:
        // `X | nil` (`X?`) and `true | false` (`bool`). Anything else names more than one class and
        // is dropped. A union a reader sees comes from `body_return`, where every class is a Ruby
        // exit with its own declaration.
        Node::UnionType(union) => union_of(union, owner),
        // `self` is the receiver's type, which this side cannot know; see [`Return::Same`]. It is
        // what makes `.strip.strip` and `[].each.` work. No singleton case is needed: a singleton
        // method's receiver *is* the singleton class.
        Node::SelfType(_) => Some(Returned::plain(Return::Same)),
        // `instance` and `class` name the declaration the method is written in, so they resolve
        // here, unlike `self`: `def self.new: () -> instance`.
        Node::InstanceType(_) => Some(Returned::plain(Return::class(owner.name))),
        Node::ClassType(_) => Some(Returned::plain(Return::class(&singleton_name(owner.name)))),
        // `singleton(Foo)` names a singleton exactly, and rubydex has a declaration for it.
        Node::ClassSingletonType(singleton) => {
            let written = type_name(&singleton.name());
            Some(Returned::plain(Return::class(&singleton_name(
                &written.path,
            ))))
        }
        // A type variable. The list that holds it decides what it means:
        //
        // - **One of the enclosing declaration's own parameters** is the receiver's type argument
        //   at that position.
        // - **The method's own `[U]`** is the block's answer, but only in the return of an arm
        //   whose block returns that same variable.
        // - **Anything else** is refused.
        Node::VariableType(variable) => {
            let written = variable.name();
            let written = written.as_str();
            if let Some(at) = owner.parameters.iter().position(|held| &**held == written) {
                return Some(Returned::plain(Return::Parameter {
                    at,
                    of: owner.name.into(),
                }));
            }
            // A method's own `[U]`, in the one position anything can be substituted into. The block
            // produced the value, so its last expression is the answer; see [`Return::Block`].
            // Every other variable is refused.
            if owner.block_returns.as_deref() == Some(written) {
                return Some(Returned::plain(Return::Block));
            }
            // A method's own `[X]` that one argument binds whole; see [`Return::Argument`].
            owner
                .passed
                .iter()
                .position(|held| held.as_deref() == Some(written))
                .map(|at| Returned::plain(Return::Argument { at }))
        }
        // **A literal written on its own names exactly one class, so there is nothing to fold.**
        // RBS spells `true`, `false`, `0`, `:sym` and `"x"` as literal types, not as their classes,
        // which is why [`union_of`] reads them itself.
        //
        // - **Unlike `bool`**, a literal is not a union: `() -> false` is `FalseClass` and nothing
        //   else, so the ordinary [`Return::class`] loses nothing.
        // - **Why it matters:** `Kernel#nil?: () -> false` is how `.nil?` resolves on almost any
        //   receiver, and `TrueClass`, `FalseClass` and `NilClass` declare `!`, `&`, `===`, `^`,
        //   `|`, `to_s`, `to_i` and `inspect` this way.
        Node::LiteralType(literal) => {
            Some(Returned::plain(Return::class(match literal.literal() {
                // The half actually written, not the carrier for both. `render` draws a lone
                // `TrueClass` as `true` and a lone `FalseClass` as `false`; only the fold's flag
                // makes either read as `bool`.
                Node::Bool(value) if value.value() => "TrueClass",
                Node::Bool(_) => "FalseClass",
                Node::Integer(_) => "Integer",
                Node::String(_) => "String",
                Node::Symbol(_) => "Symbol",
                // RBS has no fifth literal. An unknown node is refused, not guessed at, like
                // everything the catch-all below refuses.
                _ => return None,
            })))
        }
        // Everything else: an interface, `untyped`, `void`, a bare `nil`, a proc, a tuple, a
        // record, a type alias. None names one class whose members can be offered.
        _ => None,
    }
}

/// A [`FORWARDED`] return's two string literals: what the receiver answers first, and the name
/// asked of that.
fn forwarding(arguments: &ruby_rbs::node::NodeList<'_>) -> Option<Return> {
    let written: Vec<String> = arguments
        .iter()
        .map(|argument| match argument {
            Node::LiteralType(literal) => match literal.literal() {
                Node::String(text) => Some(text.string().as_str().to_owned()),
                _ => None,
            },
            _ => None,
        })
        .collect::<Option<_>>()?;
    let [through, method] = written.as_slice() else {
        return None;
    };
    Some(Return::Forwarded {
        through: through.as_str().into(),
        method: method.as_str().into(),
    })
}

/// A written-out union, folded if it is one of the two this module has a word for.
///
/// - **Both spellings mean the same thing.** `String | nil` is `String?` and `true | false` is
///   `bool`. RBS uses both (`File#size?` is `Integer?`, `Comparable#==` is `bool`), and so does a
///   project's own `sig/`. Reading only the keyword would answer one spelling and drop the other.
/// - **Two classes left after the fold is a [`Return::Union`]**, answered member by member at a
///   call. `true` or `false` alone beside a class is that literal's class.
/// - **One unreadable member refuses the whole union**: it could be anything, and the answer would
///   rest on the members that happened to be readable.
fn union_of(union: &ruby_rbs::node::UnionTypeNode<'_>, owner: &Owner<'_>) -> Option<Returned> {
    let mut nilable = false;
    let (mut truth, mut falsehood) = (false, false);
    let mut classes: Vec<Return> = Vec::new();
    let add = |member: Return, classes: &mut Vec<Return>| {
        // A nested union is its members: `(A | B) | C` is three.
        let members = match member {
            Return::Union(members) => members.into_vec(),
            member => vec![member],
        };
        for member in members {
            if !classes.contains(&member) {
                classes.push(member);
            }
        }
    };
    for written in union.types().iter() {
        match &written {
            Node::NilType(_) => nilable = true,
            // `true` and `false` are **literal types** in RBS, parsed like `1` or `"x"`, not the
            // classes behind them. So neither arrives as a name, and neither reaches `class_of`.
            Node::LiteralType(literal) if matches!(literal.literal(), Node::Bool(_)) => {
                match literal.literal() {
                    Node::Bool(value) if value.value() => truth = true,
                    _ => falsehood = true,
                }
            }
            _ => {
                let seen = class_of(&written, owner)?;
                nilable |= seen.nilable;
                add(seen.of, &mut classes);
            }
        }
    }
    match (truth, falsehood) {
        (true, true) => add(Return::Bool, &mut classes),
        (true, false) => add(Return::class("TrueClass"), &mut classes),
        (false, true) => add(Return::class("FalseClass"), &mut classes),
        (false, false) => {}
    }
    let folded = match classes.len() {
        // A bare `nil`, however it was spelled, names no class.
        0 => return None,
        1 => classes.pop()?,
        _ => Return::Union(classes.into_boxed_slice()),
    };
    Some(Returned {
        of: folded,
        nilable,
    })
}

/// The lexical scope and the `self` type at an offset.
pub struct Scope {
    /// The innermost `class`/`module`/`class << self` around the offset, as rubydex names it.
    /// Top-level code is inside `Object`, as in Ruby.
    pub nesting: NameId,
    /// Set only where `self` is not the nesting: `def self.build` and `def Foo.build`.
    pub self_id: Option<DeclarationId>,
}

/// rubydex's name for the top-level scope.
///
/// - **Ruby's top level is `Object`**, and rubydex indexes a built-in `class Object`, so the name
///   exists in every graph. Building the id costs a hash and no lookup.
/// - **The graph is read only for the names map**, which `Name::new` needs to compute a name's
///   depth. `Name::id` does not hash the depth, so the id is the same either way, but a `Name` with
///   a wrong depth should never escape.
#[must_use]
pub fn object_name(graph: &Graph) -> NameId {
    Name::new(
        graph.names(),
        StringId::from("Object"),
        ParentScope::None,
        None,
    )
    .id()
}

impl Scope {
    #[must_use]
    pub fn at(graph: &Graph, uri_id: UriId, offset: u32) -> Self {
        // A point is inside a span or outside it and cannot straddle an edge, so the refusal below
        // is unreachable from here. The fallback is written, not asserted: a top-level scope is a
        // better way to be wrong than a panic.
        Self::covering(graph, uri_id, offset, offset).unwrap_or_else(|| Self {
            nesting: object_name(graph),
            self_id: None,
        })
    }

    /// The scope every offset in `lo..=hi` is written in, when one body holds all of them.
    ///
    /// - **Why a range.** While indexing is deferred, the commonest cursor (the end of what was
    ///   just typed) is in text the graph has never seen, so
    ///   [`Rebase::to_graph`](crate::analysis::position::Rebase::to_graph) refuses it. But a scope
    ///   is a coarse question: which `class` and which `def`. The body containing the whole changed
    ///   region answers it without naming a point inside.
    /// - **`None` when the region leaves a body. Widening is not safe.** `self_of` reads the
    ///   enclosing method to decide whether `self` is an instance or the class object. An edit
    ///   running from one `def` into the next is covered only by the class body, where `self.`
    ///   would offer the singleton's members while the caret is in an instance method: wrong, not
    ///   empty, and no fallback could notice.
    /// - **So a body that overlaps the region without containing it refuses the question.** This
    ///   holds for all four body kinds read here.
    #[must_use]
    pub fn covering(graph: &Graph, uri_id: UriId, lo: u32, hi: u32) -> Option<Self> {
        let object = object_name(graph);
        let Some(document) = graph.documents().get(&uri_id) else {
            return Some(Self {
                nesting: object,
                self_id: None,
            });
        };

        // Definitions span their whole body, so the ones covering the cursor are exactly the
        // constructs it is inside. The narrowest is the innermost.
        let mut namespace: Option<&Definition> = None;
        let mut method: Option<&Definition> = None;
        for definition in document
            .definitions()
            .iter()
            .filter_map(|id| graph.definitions().get(id))
        {
            let span = definition.offset();
            let target = match definition {
                Definition::Class(_) | Definition::Module(_) | Definition::SingletonClass(_) => {
                    &mut namespace
                }
                Definition::Method(_) => &mut method,
                _ => continue,
            };
            if span.start() > lo || hi > span.end() {
                // Overlapping but not containing: the region crosses this body's edge. The edit
                // deleted a `def` or ran past an `end`, so the graph's idea of which body the caret
                // is in no longer describes the buffer.
                if span.start() <= hi && lo <= span.end() {
                    return None;
                }
                continue;
            }
            if target.is_none_or(|held| wider(held.offset(), span)) {
                *target = Some(definition);
            }
        }

        let nesting = namespace
            .and_then(|definition| definition.name_id().copied())
            .unwrap_or(object);

        Some(Self {
            nesting,
            self_id: self_of(graph, namespace, method, nesting),
        })
    }

    /// Every body in one document, read once, so many offsets can be placed with one walk.
    ///
    /// [`Scope::at`] reads every definition in the document, so calling it once per candidate is
    /// quadratic in the file. `textDocument/inlayHint` has a hint per binding. This is the same
    /// question with the walk hoisted out of the loop.
    #[must_use]
    pub fn bodies(graph: &Graph, uri_id: UriId) -> Bodies<'_> {
        let mut namespaces = Vec::new();
        let mut methods = Vec::new();
        if let Some(document) = graph.documents().get(&uri_id) {
            for definition in document
                .definitions()
                .iter()
                .filter_map(|id| graph.definitions().get(id))
            {
                match definition {
                    Definition::Class(_)
                    | Definition::Module(_)
                    | Definition::SingletonClass(_) => {
                        namespaces.push(definition);
                    }
                    Definition::Method(_) => methods.push(definition),
                    _ => {}
                }
            }
        }
        Bodies {
            graph,
            namespaces,
            methods,
        }
    }

    /// The declaration the nesting names, if the graph resolved it.
    #[must_use]
    pub fn nesting_id(&self, graph: &Graph) -> Option<DeclarationId> {
        graph.name_id_to_declaration_id(self.nesting).copied()
    }

    /// Who is calling: what a receiver context checks visibility against, and what `self` *is* for
    /// an expression.
    ///
    /// rubydex derives nothing from the nesting when an `Expression` gets `None`: such a context
    /// collects no methods and no instance variables. So every context asks this. An unstated
    /// `self` is the nesting, as in Ruby.
    #[must_use]
    pub fn caller(&self, graph: &Graph) -> Option<DeclarationId> {
        self.self_id.or_else(|| self.nesting_id(graph))
    }
}

impl Sources<'_> {
    /// The lexical scope and the `self` at one offset of one document.
    ///
    /// Walks each document once per request ([`Memo`]), however many offsets are placed in it.
    #[must_use]
    pub fn scope_at(&self, uri_id: UriId, offset: u32) -> Scope {
        self.memo.walked.of(self.graph, uri_id).at(offset)
    }
}

/// One document's bodies, kept so a scope question costs a containment test, not a walk.
///
/// **Points only**, unlike [`Scope::covering`]. A point cannot straddle a body's edge, so a plain
/// innermost lookup is enough and the refusal `covering` has is unreachable. `Scope::at` falls back
/// for the same reason.
pub struct Bodies<'g> {
    graph: &'g Graph,
    namespaces: Vec<&'g Definition>,
    methods: Vec<&'g Definition>,
}

impl Bodies<'_> {
    /// The lexical scope and the `self` type at one offset.
    ///
    /// Innermost is the narrowest containing span, [`Scope::covering`]'s rule for a point: a nested
    /// definition is narrower than its parent, and two unnested definitions cannot both contain one
    /// offset.
    #[must_use]
    pub fn at(&self, offset: u32) -> Scope {
        let namespace = innermost(&self.namespaces, offset);
        let nesting = namespace
            .and_then(|definition| definition.name_id().copied())
            .unwrap_or_else(|| object_name(self.graph));
        Scope {
            nesting,
            self_id: self_of(
                self.graph,
                namespace,
                innermost(&self.methods, offset),
                nesting,
            ),
        }
    }
}

/// The narrowest of `bodies` that contains `offset`.
///
/// A nested definition is shorter than its parent, and two unnested ones cannot both contain one
/// offset. Ties keep the first, as [`Scope::covering`]'s strict [`wider`] does.
fn innermost<'g>(bodies: &[&'g Definition], offset: u32) -> Option<&'g Definition> {
    bodies
        .iter()
        .filter(|definition| {
            definition.offset().start() <= offset && offset <= definition.offset().end()
        })
        .min_by_key(|definition| definition.offset().end() - definition.offset().start())
        .copied()
}

/// `self`, where it is not the enclosing class.
///
/// 1. **A class or module body.** `self` is the class *object*, so what can be called is `Foo`'s
///    singleton methods: the entire Rails DSL (`validates`, `has_many`, `scope`, `belongs_to`).
///    Completing a model body against the instance side would offer `valid?` and never `validates`.
/// 2. **`def self.build` and `def Foo.build`.** The lexical scope stays the class while `self`
///    moves to the singleton: constants follow the first, methods the second.
///
/// The top level is neither. `self` is `main`, an ordinary `Object`, so rubydex's answer from the
/// nesting is right and this returns `None`.
///
/// **The inner body decides, and it is not always the method.** Ruby refuses the `class` keyword
/// inside a method, so `Class.new(base) do … end` is how one is written, and rubydex records that
/// block as a class. The block is `class_eval`'d, so `self` inside it is the new class object,
/// whatever the `def` around it says. Bodies nest and never overlap, so the one that starts later
/// is the inner one.
fn self_of(
    graph: &Graph,
    namespace: Option<&Definition>,
    method: Option<&Definition>,
    nesting: NameId,
) -> Option<DeclarationId> {
    let method = method.filter(|method| {
        namespace.is_none_or(|namespace| namespace.offset().start() <= method.offset().start())
    });
    let Some(Definition::Method(method)) = method else {
        return namespace
            .is_some()
            .then(|| singleton_of_name(graph, nesting))
            .flatten();
    };
    match method.receiver().as_ref()? {
        DefinitionReceiver::SelfReceiver(_) => singleton_of_name(graph, nesting),
        DefinitionReceiver::ConstantReceiver(name_id) => singleton_of_name(graph, *name_id),
    }
}

fn singleton_of_name(graph: &Graph, name: NameId) -> Option<DeclarationId> {
    singleton_of(graph, *graph.name_id_to_declaration_id(name)?)
}

fn wider(held: &rubydex::offset::Offset, candidate: &rubydex::offset::Offset) -> bool {
    held.end() - held.start() > candidate.end() - candidate.start()
}

/// A document's text and how its offsets map onto the graph's, by the URI rubydex files it under
/// ([`Sources::read`]). Shared, not copied: a held text reaches every request that reads it.
pub type ReadText<'a> = dyn Fn(&str) -> Option<(Rc<str>, Rebase)> + 'a;

/// What reads a template's markup, comments and all: the one question its blanked view cannot
/// answer, a partial's strict-locals comment.
pub trait Markup {
    /// `uri`'s text as the editor has it, or the disk where no editor does; `None` for none.
    fn markup(&self, uri: &str) -> Option<Rc<str>>;
}

/// Everything an answer may be drawn from: the graph, the signature table, the other files, and
/// whether the guess may speak.
///
/// - **One parameter, not several.** Two rungs need more than the cursor's file: a template's
///   instance variables are assigned in a controller, and the guess must be switchable off. Fields
///   that always travel together are one value; kept apart, signatures grow to eight arguments.
/// - **`Copy` for the hop counters' sake.** A rung raises a counter by handing the rungs below it a
///   *copy*, so a sibling branch never sees the change. See [`Sources::constant_hops`].
#[derive(Clone, Copy)]
pub struct Sources<'a> {
    pub graph: &'a Indexed,
    /// What RBS says each method returns. Kept beside the graph; see the module docs.
    pub types: &'a Types,
    /// Another document's text, by the URI rubydex filed it under, **plus how that text's offsets
    /// map to the graph's**.
    ///
    /// - **A closure, because reading belongs to [`analysis`](super).** An open buffer beats the
    ///   file on disk, and only the server knows which buffers are open. So a controller being
    ///   edited types its template before it is saved.
    /// - **`None`** for a document with no readable text. That is an answer, not an error.
    /// - **The [`Rebase`] travels with the text.** An edited buffer is exactly when its offsets
    ///   stop matching the graph's, so the text and its map are one value and a reader cannot
    ///   forget the map.
    pub read: &'a ReadText<'a>,
    /// A template's markup, for what its view blanks out ([`Markup`]).
    pub markup: &'a dyn Markup,
    /// What a template's implicit receiver can answer.
    ///
    /// Read by [`locator::resolve_typed`] and `completion`, not by any rung here: it answers a
    /// *member*, not a receiver's type. It rides along because both readers already take a
    /// `Sources`.
    pub views: &'a views::Views,
    /// Which bodies of knowledge this project asked for.
    ///
    /// - **Read by the three rungs outside the generator pass.** Switching a generator off empties
    ///   its list, but a rung that reads a path, not a declaration, would keep answering from a
    ///   convention nobody asked for.
    /// - **`rails::camelize` and `generated::element_of` are not gated.** Singularising a directory
    ///   name is not a Rails feature; see their call sites.
    pub features: crate::workspace::Features,
    /// Whether the name-based guess may answer at all.
    ///
    /// Off is supported, and is why the tier is shippable. Every other answer is defensible when
    /// wrong; this one is not. A user who wants only checkable answers can have them.
    pub guess: bool,
    /// Where this project's own files are, and what `require` can name.
    ///
    /// Nothing in this module reads it; it rides along like `views`.
    /// [`locator::resolve_typed`](super::locator) and `completion` build an
    /// [`environment::Fence`](super::environment::Fence) from it. A fence built without it calls a
    /// gem's `lib/rack/test/` a test suite.
    pub layout: environment::Layout<'a>,
    /// Everything this request remembers ([`Memo`]): built once per request, always present, so an
    /// answer never depends on whether a caller thought to pass a memo.
    pub memo: &'a Memo<'a>,
    /// How many constant assignments have been followed to get here.
    ///
    /// - **A cycle guard for `A = B.new` beside `B = A.new`.** The per-request bulkhead catches
    ///   panics, but a stack overflow aborts the process. [`assigned_to`] is the rung that crosses
    ///   documents and can come back to where it started.
    /// - **Zero at every call site but one.** `assigned_to` hands the rungs below it a raised copy,
    ///   so this is a field of a `Copy` struct, not a shared counter.
    pub constant_hops: u8,
    /// How many method **bodies** have been read to get here: the chain's *depth*.
    ///
    /// `cursor::MAX_WIDTH` bounds a receiver chain's width. This bounds how far a rung may nest
    /// *inside* the definitions the chain lands on: one body read is depth 1, and a body whose exit
    /// needs another body is depth 2.
    ///
    /// Raised like [`Self::constant_hops`], by handing a copy down, and bounded for the same
    /// reason: a recursion that passes something new each time is legal Ruby, and a stack overflow
    /// aborts the process. One that does not is refused sooner ([`Reads::bodies`]).
    pub body_hops: u8,
    /// The class of the object whose method body is being read, where the call named one.
    ///
    /// - **Why:** `record.errors` reads `ActiveModel::Validations#errors`, whose `@errors` any class
    ///   including the module may write. Asked from the module, the answer is every includer's
    ///   writes; asked for a `Story`, it is `Story`'s ([`instance_read`]).
    /// - **It is also what `self` is** in that body ([`method_receiver`]'s `SelfObject` arm), so a
    ///   self-call starts at the object's class and an override there wins.
    /// - **Set by [`from_body`], always**: a body read on a union or an unknown receiver hands
    ///   `None` down, never the class an outer body was read for. And by [`scoped_beside`], for the
    ///   class a scope was called on.
    /// - **Part of every read's memo key** ([`Reads`]), since the same read answers differently
    ///   per object.
    pub object: Option<DeclarationId>,
    /// What one call passed, for the body of the method it reached.
    ///
    /// - **Why:** `def self.bar(baz) = baz` has no return while `baz` has no type, but at
    ///   `Foo.bar(1)` it is `Integer`. The answer belongs to that call, and never to the method.
    /// - **Set by [`from_body`] only, and always**, like [`Self::object`]: a nested read of another
    ///   method carries that call's arguments, or none.
    /// - **It names its method**, so [`from_parameter`] binds only that method's parameters, never
    ///   another `def`'s met on the way (an instance variable's writer).
    /// - **Part of every read's memo key** ([`Reads`]), so `bar(1)`'s answer is never `bar("x")`'s.
    /// - **A key, not the arguments**: [`Reads::bindings`] holds them for the request, because a
    ///   reference here could not outlive the request's [`Memo`].
    pub bound: Option<u64>,
    /// What `new` passed to build the object whose body is being read, as a key into
    /// [`Reads::bindings`] like [`Self::bound`].
    ///
    /// - **Why:** `@user = user` in `initialize` has no type while `user` has none, but the object
    ///   `Service.new(story)` built holds a `Story` there, and its `call` reads it.
    /// - **Set by [`from_body`] only, and always**, from the receiver's [`Typed::made`], and only
    ///   where [`Self::object`] is set: it is that object's.
    /// - **It names the `initialize` the object ran**, so [`from_parameter`] binds only that
    ///   method's parameters, which is where its variables' writes read them.
    /// - **Part of every read's memo key** ([`Reads`]), since one object's variables are not
    ///   another's.
    pub made: Option<u64>,
    /// The module whose `def` this body is, read as [`Self::object`]'s own method
    /// ([`defined_as_own`]): a concern's class method, run on the including class.
    ///
    /// - **Why:** the class object descends from nothing the `def` is written in, so `self` there
    ///   would be the module, and every receiverless call would ask the wrong class.
    /// - **Set by [`from_body`] only, and always**: a nested read of another method is not one.
    pub extended: Option<DeclarationId>,
    /// The exits every document has been walked for, **across** requests.
    ///
    /// The split with [`Memo`]: that memo holds buffers and their [`Rebase`], so it dies with the
    /// request. This one holds only what the text decides, which is most of a read's
    /// cost. See [`HeldExits`].
    ///
    /// Set for every request, because a single cursor pays the same walk of the same unchanged gem
    /// file that `inlayHint` does.
    pub held_exits: &'a HeldExits,
    /// What the generators said about the text they read that is not RBS: which class a block's
    /// `self` is ([`rebound_self`]), and where a generated member was written.
    pub generated: &'a Synthesized,
    /// The bodies of knowledge, for what one keeps beside the RBS: a key's value ([`keyed`]).
    pub knowledge: &'a knowledge::Registry,
}

/// Everything one request remembers, and nothing longer: every memo here holds answers read against
/// buffers an edit changes.
///
/// - **Always present** ([`Sources::memo`]). Every request builds one where it builds its
///   [`Sources`], so no rung answers differently for a caller that forgot to pass one, and none
///   rebuilds a fresh one mid-question.
/// - **Tied to one `read`.** A caller that reads other text (completion's repaired buffer) builds
///   its own, since the documents here were read through the request's.
pub struct Memo<'a> {
    /// What each variable read, body and document resolved to ([`Reads`]).
    pub(super) reads: Reads,
    /// Each document's scopes, walked once however many offsets are placed in it
    /// ([`Sources::scope_at`]); the quadratic [`Scope::bodies`] avoids, one request wide.
    walked: Walked<'a>,
    /// Which `private` records survive a reread ([`locator::is_private`]).
    pub(super) modifiers: locator::Modifiers<'a>,
    /// Which `def`s are written inside a block ([`locator::declared_on_the_root`]).
    pub(super) blocks: locator::Blocks<'a>,
}

impl<'a> Memo<'a> {
    /// An empty memo for one request that reads through `read`, beside the half that outlives it.
    #[must_use]
    pub fn new(read: &'a ReadText<'a>, held: &'a HeldExits) -> Self {
        Self {
            reads: Reads::default(),
            walked: Walked::default(),
            modifiers: locator::Modifiers::new(read, held),
            blocks: locator::Blocks::new(read),
        }
    }
}

/// One document as [`body_return`] needs it: what each `def` returns, and the map from its text
/// onto the graph.
///
/// Both halves come from one [`Sources::read`] call, so the exits and the offsets they are keyed by
/// describe the same string.
struct Document {
    /// Every `def`'s exits, by the span rubydex filed the method under, and every variable read's
    /// reaching writes ([`cursor::Shapes`]). Walked the first time it is asked for: see
    /// [`Document::shapes`].
    ///
    /// Shared because [`HeldExits`] hands the same walk to every request that reads this text. The
    /// walk is the expensive half of a read, and depends only on the text.
    shapes: OnceCell<Rc<cursor::Shapes>>,
    /// The text the exits were read from.
    ///
    /// Kept after the walk because a derivation records the **line** a body was read at, and
    /// which span is asked about is only known when a caller asks. Shared with [`HeldExits::text`],
    /// so a held text is not copied into every request that reads it.
    source: Rc<str>,
    /// That text's map onto the graph's coordinates.
    rebase: Rebase,
    /// What the block written on each call hands back, by where the call starts
    /// ([`cursor::blocks_handed_back`]). Walked the first time a member made from a block is read
    /// here ([`made_from_block`]), which most texts never are.
    blocks: OnceCell<HashMap<u32, Box<[Receiver]>>>,
    /// What the one lambda each call is passed hands back, by where the call starts
    /// ([`cursor::lambdas_handed_back`]), for [`scoped_beside`], walked on first use.
    lambdas: OnceCell<HashMap<u32, Box<[Receiver]>>>,
    /// The render calls this text makes ([`rails::read_renders`]), with what each value they pass
    /// a partial is ([`cursor::values_at`]), for a partial's locals. Walked on first
    /// use; `view` is fixed by the path, so one reading is the only one.
    renders: OnceCell<Rc<Passing>>,
}

/// One text's render calls, and each value they pass a partial as a shape, by its span.
struct Passing {
    calls: Vec<rails::Render>,
    values: HashMap<(u32, u32), Receiver>,
}

impl Document {
    /// [`Document::blocks`], walked on first use.
    fn blocks(&self) -> &HashMap<u32, Box<[Receiver]>> {
        self.blocks
            .get_or_init(|| cursor::blocks_handed_back(&self.source))
    }

    /// [`Document::lambdas`], walked on first use.
    fn lambdas(&self) -> &HashMap<u32, Box<[Receiver]>> {
        self.lambdas
            .get_or_init(|| cursor::lambdas_handed_back(&self.source))
    }

    /// [`Document::renders`], read as a view or helper reads a call where `view`, on first use.
    fn renders(&self, view: bool) -> Rc<Passing> {
        Rc::clone(self.renders.get_or_init(|| {
            let calls = rails::read_renders(&self.source, view);
            let spans: Vec<(u32, u32)> = calls
                .iter()
                .filter_map(|call| match &call.locals {
                    rails::Locals::Named(locals) => Some(locals),
                    rails::Locals::Unread => None,
                })
                .flatten()
                .filter_map(|local| match local.value {
                    rails::Value::Written(span)
                    | rails::Value::Element(span)
                    | rails::Value::Object(span)
                    | rails::Value::Either(span) => Some(span),
                    rails::Value::Counter | rails::Value::Iteration => None,
                })
                .collect();
            let values = cursor::values_at(&self.source, &spans);
            Rc::new(Passing { calls, values })
        }))
    }

    /// The walk of this text, done the first time anyone asks for it.
    ///
    /// **Lazily, because most readers of a text never need it.** An instance variable's writes are
    /// looked for in every document of every class its object has, and a scan for the name rules
    /// out nearly all of them before any walk.
    ///
    /// `held` is the walk's own cache and is asked before `cursor::shapes` is. See [`HeldExits`] for
    /// why it outlives the request and this memo does not.
    fn shapes(&self, uri: &str, held: &HeldExits) -> &cursor::Shapes {
        self.shapes.get_or_init(|| held.of(uri, &self.source))
    }
}

impl HeldExits {
    /// [`HeldExits::foreign`] for one document, computed by `read` where the held entry is for
    /// another version of it.
    fn foreign(
        &self,
        document: UriId,
        hash: u64,
        read: impl FnOnce() -> Option<ForeignNames>,
    ) -> Option<ForeignNames> {
        if let Some((held, names)) = self.foreign.borrow().get(&document)
            && *held == hash
        {
            return Some(Rc::clone(names));
        }
        let names = read()?;
        self.foreign
            .borrow_mut()
            .insert(document, (hash, Rc::clone(&names)));
        Some(names)
    }
}

impl HeldExits {
    /// [`HeldExits::handed`] for one document, computed by `read` where the held entry is for
    /// another version of it.
    pub(crate) fn handed(
        &self,
        document: UriId,
        hash: u64,
        read: impl FnOnce() -> Option<HeldHanded>,
    ) -> Option<HeldHanded> {
        if let Some((held, symbols)) = self.handed.borrow().get(&document)
            && *held == hash
        {
            return Some(Rc::clone(symbols));
        }
        let symbols = read()?;
        self.handed
            .borrow_mut()
            .insert(document, (hash, Rc::clone(&symbols)));
        Some(symbols)
    }

    /// [`HeldExits::renders`] for one document, computed by `read` where the held entry is for
    /// another version of it.
    fn renders(
        &self,
        document: UriId,
        hash: u64,
        read: impl FnOnce() -> Option<HeldRenders>,
    ) -> Option<HeldRenders> {
        if let Some((held, calls)) = self.renders.borrow().get(&document)
            && *held == hash
        {
            return Some(Rc::clone(calls));
        }
        let calls = read()?;
        self.renders
            .borrow_mut()
            .insert(document, (hash, Rc::clone(&calls)));
        Some(calls)
    }
}

/// What [`body_return`] has already read, for the length of **one request**.
///
/// - **Why:** the rung reads a method's body from the document that declares it, and an `inlayHint`
///   asks once per `def`. Without this, each ask re-reads and re-parses the whole document.
/// - **A failed read is remembered as `None`**, not retried: the reasons (no such URI, an unsettled
///   edit) do not change within a request.
/// - **`RefCell`** because [`Sources`] is `Copy` and each rung hands the ones below it a copy. A
///   shared reference is what the copies have in common, and only the analysis thread holds one.
#[derive(Default)]
pub struct ReadBodies {
    documents: RefCell<HashMap<String, Option<Rc<Document>>>>,
}

impl ReadBodies {
    /// One document, read. **The only place a [`Document`] is made**, so the cached and uncached
    /// paths cannot drift apart. Its walk waits until it is asked for ([`Document::shapes`]).
    fn read(uri: &str, read: &ReadText<'_>) -> Option<Rc<Document>> {
        let (source, rebase) = read(uri)?;
        Some(Rc::new(Document {
            shapes: OnceCell::new(),
            source,
            rebase,
            blocks: OnceCell::new(),
            lambdas: OnceCell::new(),
            renders: OnceCell::new(),
        }))
    }

    /// The same, answered from the memo where this request has asked already.
    fn of(&self, uri: &str, read: &ReadText<'_>) -> Option<Rc<Document>> {
        if let Some(found) = self.documents.borrow().get(uri) {
            return found.clone();
        }
        let made = Self::read(uri, read);
        self.documents
            .borrow_mut()
            .insert(uri.to_owned(), made.clone());
        made
    }
}

/// How many documents' exits [`HeldExits`] keeps before it drops them all.
///
/// - **Above the widest single read, not a working size.** One [`instance_read`] walks every
///   writer document of its object that spells the name, and a read wider than this bound empties
///   the cache partway through and parses them all again on every request. One
///   application's `shopify_api` has 889 resource classes under one base, each writing its class-level
///   variables, which at 512 cost every hover there 190 ms. The bound is there so a *session*
///   cannot grow without limit.
/// - **Drop everything, not the oldest.** An eviction order is one more thing to get wrong. A
///   dropped entry costs one re-parse, so being wrong here means a slow request, never a wrong
///   answer.
const HELD_DOCUMENTS: usize = 2048;

/// How many bytes of documents' text [`HeldExits::text`] keeps before it drops them all.
///
/// The same headroom as [`HELD_DOCUMENTS`]: 889 `shopify_api` resource files are 5 MB. A full
/// audit reads 52–61 MB of distinct text on the larger corpora, so this lets go during
/// one, and a dropped text costs one read from disk.
const HELD_TEXT_BYTES: usize = 32 << 20;

/// Every document's exits, kept **across** requests and keyed by the text they were read from.
///
/// 1. **The split with [`ReadBodies`] is what depends on the graph.** A [`Document`] holds a
///    [`Rebase`], built from the buffer *and* the graph's text, so a settle moves it while the
///    buffer stays byte-identical. `cursor::returns_in` reads only the buffer. So the `Rebase` is
///    rebuilt per request, and the walk is kept.
/// 2. **The walk is most of the cost** of a body read, far more than the `ruby_prism::parse` under
///    it, and nothing it depends on changes while a user scrolls.
/// 3. **Keyed by URI and content hash.** [`scopes`](super::scopes) compares the whole string
///    instead, because a hash can collide. Here a wrong answer needs two *different texts of one
///    document* to collide, and `xxh3_64` already decides whether a document is indexed
///    (`Analysis::graph_holds`). A file edited and edited back is the same text, and rightly hits.
#[derive(Default)]
pub struct HeldExits {
    held: RefCell<HashMap<String, (u64, Rc<cursor::Shapes>)>>,
    /// The names each application document's reflective writes on another object can reach,
    /// by the content hash the graph holds for it ([`written_by_another`]). Read from that
    /// version of the text, so a request does not read every such file to learn nothing changed.
    foreign: RefCell<HashMap<UriId, (u64, ForeignNames)>>,
    /// What each method's callers pass at one position ([`passed`]), by the method and position,
    /// held while every calling document is the version it was read from.
    passed: RefCell<HashMap<(String, usize), (Versions, ForeignNames)>>,
    /// Each application document's render calls, by the content hash the graph holds for it
    /// ([`template_renderers`]), each placed in the graph's coordinates or `None` where it cannot
    /// be.
    renders: RefCell<HashMap<UriId, (u64, HeldRenders)>>,
    /// Each application document's symbols written as a call's first argument, with the call, by
    /// the content hash the graph holds for it, placed in the graph's coordinates: where a method's
    /// name is handed to `send` and its kin (`references::Named`). Every call's, so a change in
    /// which calls take a name reads nothing again.
    handed: RefCell<HashMap<UriId, (u64, HeldHanded)>>,
    /// Closed documents' texts, by the URI rubydex files them under and the content hash the graph
    /// holds for them ([`Self::text`]).
    texts: RefCell<HeldTexts>,
    /// Each document's `def`s rubydex records private only because a modifier escaped a block
    /// ([`locator::Modifiers`]), in the offsets of the text they were read from, by that text's
    /// hash ([`Self::escapes`]).
    escapes: RefCell<HashMap<String, (u64, HeldEscapes)>>,
}

/// Closed documents' texts and their maps onto the graph, with the bytes they take.
#[derive(Default)]
struct HeldTexts {
    held: HashMap<String, (u64, Rc<str>, Rebase)>,
    bytes: usize,
}

/// One document's escaped `def`s ([`HeldExits::escapes`]), in its text's offsets.
type HeldEscapes = Rc<[u32]>;

/// One document's render calls, each with where the graph places it.
type HeldRenders = Rc<[(Option<u32>, rails::Render)]>;

/// One document's first-argument symbols ([`HeldExits::handed`]): the call's name, the symbol's,
/// and the name's span in the graph's coordinates.
pub type HeldHanded = Rc<[(String, String, u32, u32)]>;

/// Each document an answer was read from, with the content hash of the version read.
type Versions = Vec<(UriId, u64)>;

/// The names reflective writes can reach: one document's on another object, or one parameter's
/// callers'.
type ForeignNames = Rc<[scopes::Spelled]>;

impl HeldExits {
    /// A fresh cache: the one thing that outlives a request.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The walk of `source`, done here or reused from a previous request.
    fn of(&self, uri: &str, source: &str) -> Rc<cursor::Shapes> {
        let hash = xxhash_rust::xxh3::xxh3_64(source.as_bytes());
        if let Some((held, shapes)) = self.held.borrow().get(uri)
            && *held == hash
        {
            return Rc::clone(shapes);
        }
        self.hold(uri, hash, cursor::shapes(source))
    }

    /// Keep one version's walk, dropping everything where the cache is full.
    fn hold(&self, uri: &str, hash: u64, shapes: cursor::Shapes) -> Rc<cursor::Shapes> {
        let shapes = Rc::new(shapes);
        let mut held = self.held.borrow_mut();
        if held.len() >= HELD_DOCUMENTS {
            held.clear();
        }
        held.insert(uri.to_owned(), (hash, Rc::clone(&shapes)));
        shapes
    }

    /// Whether the walk of this version of `source` is held already, so a caller reading the text
    /// for its own reasons need not make one ([`cursor::margin`]).
    #[must_use]
    pub fn holds(&self, uri: &str, source: &str) -> bool {
        let hash = xxhash_rust::xxh3::xxh3_64(source.as_bytes());
        self.held
            .borrow()
            .get(uri)
            .is_some_and(|(held, _)| *held == hash)
    }

    /// Hold a walk of `source` a caller made from its own parse, as [`Self::of`] would have.
    pub fn keep(&self, uri: &str, source: &str, shapes: cursor::Shapes) {
        self.hold(uri, xxhash_rust::xxh3::xxh3_64(source.as_bytes()), shapes);
    }

    /// A closed document's text and its map, where it was read before at the version the graph
    /// holds (`hash`, rubydex's `Document::content_hash`).
    ///
    /// - **What [`instance_read`] needs across requests**: every writer document of an object,
    ///   read to learn whether it spells the name. Opening and reading 889 files was most of a
    ///   hover once their walks were held.
    /// - **Only the graph's own version is held** ([`Self::keep_text`]), so the text is the one
    ///   every offset into that document is measured against. A file changed on disk and not yet
    ///   indexed answers with the text the graph still holds.
    /// - **An open buffer is never held**: it changes without the graph knowing. The caller
    ///   forgets everything when one opens ([`Self::forget_texts`]).
    #[must_use]
    pub fn text(&self, uri: &str, hash: u64) -> Option<(Rc<str>, Rebase)> {
        let texts = self.texts.borrow();
        let (held, text, rebase) = texts.held.get(uri)?;
        (*held == hash).then(|| (Rc::clone(text), *rebase))
    }

    /// Hold a closed document's text read at the version the graph holds, dropping everything
    /// once [`HELD_TEXT_BYTES`] is reached.
    pub fn keep_text(&self, uri: &str, hash: u64, text: &Rc<str>, rebase: Rebase) {
        let mut texts = self.texts.borrow_mut();
        if texts.bytes + text.len() > HELD_TEXT_BYTES {
            texts.held.clear();
            texts.bytes = 0;
        }
        if let Some((_, old, _)) = texts
            .held
            .insert(uri.to_owned(), (hash, Rc::clone(text), rebase))
        {
            texts.bytes -= old.len();
        }
        texts.bytes += text.len();
    }

    /// Drop every held text: a document just opened, and its buffer now answers for it.
    pub fn forget_texts(&self) {
        *self.texts.borrow_mut() = HeldTexts::default();
    }

    /// [`locator::Modifiers`]' walk of `source`, done by `walk` or reused from a previous request.
    ///
    /// - **Why:** the privacy gate asks it of every file declaring a private candidate, and a
    ///   hover re-parsed each of them per request: a fifth of a hover's time.
    /// - **Keyed by the text, like [`Self::of`]**, so it never goes stale: an open buffer typed
    ///   into is another text and is walked again. That makes it safe for buffers too, unlike
    ///   [`Self::text`].
    /// - **In the text's own offsets.** The map onto the graph's is built per request, for
    ///   [`HeldExits`]' first reason.
    pub(super) fn escapes(
        &self,
        uri: &str,
        source: &str,
        walk: impl FnOnce() -> Vec<u32>,
    ) -> HeldEscapes {
        let hash = xxhash_rust::xxh3::xxh3_64(source.as_bytes());
        if let Some((held, found)) = self.escapes.borrow().get(uri)
            && *held == hash
        {
            return Rc::clone(found);
        }
        let found: HeldEscapes = walk().into();
        let mut escapes = self.escapes.borrow_mut();
        if escapes.len() >= HELD_DOCUMENTS {
            escapes.clear();
        }
        escapes.insert(uri.to_owned(), (hash, Rc::clone(&found)));
        found
    }

    /// How many documents are held. For tests; the cache has no other observable state.
    #[must_use]
    pub fn len(&self) -> usize {
        self.held.borrow().len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.held.borrow().is_empty()
    }
}

/// How many times a cycle of reads is answered again before it is refused.
///
/// A loop that writes `x = x.succ` needs two rounds: one with the loop's own write not known yet,
/// one with it. Each round can only add a class, and a variable has only so many writes, so a real
/// cycle settles in a few. A value that keeps changing (`x = [x]`) is refused, not approximated.
const ROUNDS: usize = 6;

/// How many reads deep one answer may go: a guard against a stack overflow, not a bound on
/// answers. Each read waits on the ones its writes read, and a chain of assignments this long is
/// not written by hand.
const READS_DEEP: usize = 128;

/// What the variable reads of one request resolved to.
///
/// A read ([`Receiver::Variable`]) is every write that reaches it, and a write's value may read
/// other variables, so answering one read answers a graph of them. This holds that graph for the
/// length of one request.
///
/// - **Each read is answered once.** A read's answer does not depend on who asked, so the second
///   ask is a lookup. Copying answers into the question instead is what made the first version of
///   this grow as 2^n.
/// - **A cycle is answered by rounds.** `x = x + 1` in a loop reads `x` through its own write. The
///   first read asked is the cycle's head: its first round answers with the reads on the cycle
///   marked [`Self::pending`], which add nothing. Each later round uses the previous one's answer,
///   until two rounds agree ([`ROUNDS`] at most, or the head refuses).
/// - **What a round computed inside the cycle is kept only if the round was the last.** Earlier
///   rounds used an answer that was not final, so their answers are dropped with it.
pub struct Reads {
    /// Each document's text, map and walk, read once.
    documents: ReadBodies,
    /// Answers that are final.
    settled: RefCell<HashMap<ReadKey, Option<Typed>>>,
    /// Answers computed inside a cycle whose head is still working, with the lowest stack depth
    /// they reached back to.
    tentative: RefCell<HashMap<ReadKey, Tentative>>,
    /// A cycle head's answer from its previous round.
    provisional: RefCell<HashMap<ReadKey, Option<Typed>>>,
    /// The reads being answered, outermost first.
    stack: RefCell<Vec<ReadKey>>,
    /// The lowest stack depth the read being answered has reached back to.
    low: Cell<usize>,
    /// Set when the last `None` meant *not known yet*, not *nothing to say*: a read reached back
    /// into its own cycle before its head had a first answer.
    pending: Cell<bool>,
    /// Each object's classes and the documents their writes are in ([`Hierarchy`]), by the
    /// namespace, the side and the document asking: in front of [`Indexed::hierarchy`], which
    /// holds them across requests by the fence the document asks with.
    hierarchies: RefCell<Hierarchies>,
    /// What each `def initialize` does with one instance variable, by the method and the name.
    initializers: RefCell<HashMap<(DeclarationId, String), Initializes>>,
    /// What the callers of a method pass at one position ([`passed`]), by the method and position.
    passed: RefCell<HashMap<(String, usize), ForeignNames>>,
    /// Every class that renders a template, by the template's `directory/name`; `None` where one
    /// renders it with variables no class writes ([`template_renderers`]).
    renderers: RefCell<HashMap<String, Option<Rc<[DeclarationId]>>>>,
    /// What each call a body was read for passed, and what `new` passed to each object a body was
    /// read for, by [`Binding::key`] ([`Sources::bound`], [`Sources::made`]).
    bindings: RefCell<HashMap<u64, Rc<Binding>>>,
    /// What each call of a proc literal passed, by the key its body is read under
    /// ([`proc_value`]).
    proc_bindings: RefCell<HashMap<u64, Rc<ProcBinding>>>,
    /// The method bodies being read, outermost first ([`body_return`]), each with how many
    /// variable reads were open ([`Self::stack`]) when it began.
    ///
    /// - **A body asked for again with no variable read opened since is a recursion**: its answer
    ///   would need its own answer, and nothing between could settle it. It answers nothing at once,
    ///   the answer [`BODY_HOPS`] reached only after unrolling the loop that many times over.
    /// - **A loop through a variable read is the reads' own to settle.** `def filters = @filters ||=
    ///   []` beside `@filters = filters.first(3)` comes back to `@filters`, whose rounds re-read the
    ///   body with the last round's answer until two agree. Refusing the body there would make that
    ///   write one nothing types, which refuses the variable: 13 labels over the corpora.
    bodies: RefCell<Vec<(BodyKey, usize)>>,
    /// What each block around a `self` says it runs against ([`rebound_self`]), by where its call
    /// starts: `None` for a block passed over. Every bare call in a block asks its block again, and
    /// a `routes.draw` block holds hundreds of them.
    rebound: RefCell<HashMap<ReadKey, Option<Option<Typed>>>>,
    /// What each accessor's writer was given ([`written_to`]), by the reader and the document
    /// asking.
    written: RefCell<HashMap<WrittenKey, Option<Writes>>>,
    /// The accessors being answered, outermost first: one asked again is a loop.
    writing: RefCell<Vec<DeclarationId>>,
    /// The `attr_writer`s being answered ([`setter_values`]), by name and the objects' classes,
    /// outermost first: one asked again is a loop.
    setting: RefCell<Vec<(String, Vec<DeclarationId>)>>,
    /// The members handing a call on being answered ([`forwarded`]), outermost first: one asked
    /// again is a loop, which nothing else bounds. `delegate :x, to: :me` beside `def me = self`
    /// asks `x` of the same object forever.
    forwarding: RefCell<Vec<DeclarationId>>,
    /// What each bare name in a partial is as a local its render calls pass ([`partial_local`]),
    /// by the partial's document and the name.
    locals: RefCell<HashMap<(UriId, String), Local>>,
    /// The partial locals being answered: one asked again is a partial rendering itself.
    localizing: RefCell<Vec<(UriId, String)>>,
    /// The local reads every value reaching which a check rules out: control never
    /// gets there ([`dead`]).
    unreachable: RefCell<HashSet<ReadKey>>,
    /// Every application jbuilder view ([`jbuilder_documents`]), found once.
    jbuilders: RefCell<Option<Rc<[UriId]>>>,
}

/// One body read: the method, and the object, the call and the `new` it is read for
/// ([`Sources::object`], [`Sources::bound`], [`Sources::made`]). A read for another call is another
/// question, so `fetch(page + 1)` inside `fetch` is not refused unless it passes what the outer call
/// did.
type BodyKey = (
    DeclarationId,
    Option<DeclarationId>,
    Option<u64>,
    Option<u64>,
);

/// An answer computed inside a cycle, and the lowest stack depth it reached back to.
type Tentative = (Option<Typed>, usize);

/// One read of one request: its document, where its name starts, the object whose body is being
/// read ([`Sources::object`]), the call it is read for ([`Sources::bound`]) and what built that
/// object ([`Sources::made`]); `0` for none.
type ReadKey = (UriId, u32, Option<DeclarationId>, u64, u64);

/// [`Reads::hierarchies`]: by the namespace, whether it is the class object's side, and the
/// document asking.
type Hierarchies = HashMap<(DeclarationId, bool, UriId), Option<Rc<Hierarchy>>>;

/// [`Reads::written`]'s key: the reader, the document asking, and for a current attribute the
/// receiver's class, whose store it is.
type WrittenKey = (DeclarationId, UriId, Option<DeclarationId>);

impl Default for Reads {
    fn default() -> Self {
        Self {
            documents: ReadBodies::default(),
            settled: RefCell::default(),
            tentative: RefCell::default(),
            provisional: RefCell::default(),
            stack: RefCell::default(),
            low: Cell::new(usize::MAX),
            pending: Cell::new(false),
            hierarchies: RefCell::default(),
            initializers: RefCell::default(),
            passed: RefCell::default(),
            renderers: RefCell::default(),
            bindings: RefCell::default(),
            proc_bindings: RefCell::default(),
            bodies: RefCell::default(),
            rebound: RefCell::default(),
            written: RefCell::default(),
            writing: RefCell::default(),
            setting: RefCell::default(),
            forwarding: RefCell::default(),
            locals: RefCell::default(),
            localizing: RefCell::default(),
            unreachable: RefCell::default(),
            jbuilders: RefCell::default(),
        }
    }
}

/// Run `ask` with [`Reads::pending`] cleared first, so what it says afterwards is about `ask`.
fn pending_aware<T>(sources: &Sources<'_>, ask: impl FnOnce() -> T) -> T {
    sources.memo.reads.pending.set(false);
    ask()
}

/// Whether the last `None` was a read still being answered: see [`Reads::pending`].
fn pending(sources: &Sources<'_>) -> bool {
    sources.memo.reads.pending.get()
}

/// What one read of a variable holds: every write that can reach it, folded.
///
/// See [`Reads`] for how a graph of reads is answered. `at` is where the read's name starts in the
/// document's own text, the key [`cursor::Variables`] was built with.
fn variable(sources: &Sources<'_>, uri_id: UriId, at: u32) -> Option<Typed> {
    let reads = &sources.memo.reads;
    let key = (
        uri_id,
        at,
        sources.object,
        sources.bound.unwrap_or(0),
        sources.made.unwrap_or(0),
    );
    if let Some(answer) = reads.settled.borrow().get(&key) {
        return answer.clone();
    }
    if let Some((answer, _)) = reads.tentative.borrow().get(&key) {
        return answer.clone();
    }
    let open = reads.stack.borrow().iter().position(|held| *held == key);
    if let Some(depth) = open {
        // Back into a read still being answered: a cycle. Its previous round's answer, or nothing
        // yet.
        reads.low.set(reads.low.get().min(depth));
        return match reads.provisional.borrow().get(&key) {
            Some(answer) => answer.clone(),
            None => {
                reads.pending.set(true);
                None
            }
        };
    }
    let depth = reads.stack.borrow().len();
    if depth >= READS_DEEP {
        return None;
    }
    reads.stack.borrow_mut().push(key);
    let outer = reads.low.get();
    let mut rounds = 0;
    let (answer, low) = loop {
        reads.low.set(usize::MAX);
        let answer = reached(sources, uri_id, at);
        let low = reads.low.get();
        // Part of a cycle through a read further out: final only when that one settles.
        if low < depth {
            reads
                .tentative
                .borrow_mut()
                .insert(key, (answer.clone(), low));
            break (answer, low);
        }
        // No cycle came back through here.
        if low == usize::MAX {
            reads.settled.borrow_mut().insert(key, answer.clone());
            break (answer, low);
        }
        // This read heads a cycle.
        rounds += 1;
        let converged = reads
            .provisional
            .borrow()
            .get(&key)
            .is_some_and(|held| same_type(held.as_ref(), answer.as_ref()));
        if converged || rounds >= ROUNDS {
            let answer = if converged { answer } else { None };
            let mut tentative = reads.tentative.borrow_mut();
            let mut settled = reads.settled.borrow_mut();
            let inside: Vec<ReadKey> = tentative
                .iter()
                .filter(|(_, (_, reached))| *reached >= depth)
                .map(|(held, _)| *held)
                .collect();
            for held in inside {
                if let Some((value, _)) = tentative.remove(&held) {
                    settled.insert(held, if converged { value } else { None });
                }
            }
            reads.provisional.borrow_mut().remove(&key);
            settled.insert(key, answer.clone());
            break (answer, usize::MAX);
        }
        reads.provisional.borrow_mut().insert(key, answer);
        reads
            .tentative
            .borrow_mut()
            .retain(|_, (_, reached)| *reached < depth);
    };
    reads.stack.borrow_mut().pop();
    reads.low.set(outer.min(low));
    answer
}

/// Whether two rounds of a cycle agree: the same classes, facets and tier.
///
/// **What was followed may still grow when the type no longer does.** `total += 1` in a loop reads
/// `Integer#+` once more each round, so comparing whole answers would never settle.
fn same_type(held: Option<&Typed>, answer: Option<&Typed>) -> bool {
    match (held, answer) {
        (Some(held), Some(answer)) => {
            held.classes == answer.classes
                && held.nilable == answer.nilable
                && held.boolean == answer.boolean
                && held.arguments == answer.arguments
                && held.same == answer.same
                && held.derivation.tier() == answer.derivation.tier()
        }
        (held, answer) => held.is_none() && answer.is_none(),
    }
}

/// One round of [`variable`]: every write reaching the read, each resolved where it is written, and
/// the parameter's own value where it can reach.
///
/// - **One write nothing can type refuses the read.** Skipping it is how a label would rest on
///   the writes that happened to type.
/// - **A write still being answered in this cycle is skipped for this round**, which is the only
///   difference from nothing: see [`Reads`].
/// - **A write in text the graph has not seen refuses the read**, as every rung here refuses an
///   offset it cannot translate.
fn reached(sources: &Sources<'_>, uri_id: UriId, at: u32) -> Option<Typed> {
    let reads = &sources.memo.reads;
    let graph = sources.graph;
    let uri = graph.documents().get(&uri_id)?.uri().to_owned();
    let document = reads.documents.of(&uri, sources.read)?;
    let variables = &document.shapes(&uri, sources.held_exits).variables;
    let reaching = variables.reaching(at)?;
    if let Some(instance) = &reaching.instance {
        return instance_read(
            sources, uri_id, &document, variables, at, reaching, instance,
        );
    }
    let (typed, waiting, mut gone) =
        typed_members(sources, uri_id, &document, variables, at, reaching)?;
    // A check the read is under can rule out the `nil` no write hands over.
    let nil_reaches = reaching.nil
        && (reaching.narrowed.is_empty()
            || Folds::of(graph).nil.is_none_or(|nil| {
                !matches!(
                    narrowed_by(
                        sources,
                        uri_id,
                        &document,
                        Some(Typed::of(nil, Derivation::default())),
                        reaching,
                        None,
                    ),
                    Narrowed::Gone
                )
            }));
    gone |= reaching.nil && !nil_reaches;
    let nil = nil_reaches;
    if typed.is_empty() && !nil {
        // Everything reaching this read is still being answered, or ruled out: by a check, or as a
        // value rooted at a read no value reaches.
        if !waiting && gone {
            reads.unreachable.borrow_mut().insert((
                uri_id,
                at,
                sources.object,
                sources.bound.unwrap_or(0),
                sources.made.unwrap_or(0),
            ));
        }
        reads.pending.set(waiting);
        return None;
    }
    fold_reached(graph, typed, nil)
}

/// Whether `value` starts at a local read every value reaching which a check rules out:
/// its receiver chain is evaluated first, so where the read is never reached neither is the
/// value, and it hands back nothing. Only the chain's root counts: a ruled-out read anywhere else
/// (an argument, a shortcut's side) may be in a part of the value that never runs.
fn dead(sources: &Sources<'_>, uri_id: UriId, value: &Receiver) -> bool {
    let mut value = value.unguarded();
    loop {
        match value {
            Receiver::Returned { on, .. } => value = on,
            Receiver::Spelled { was, .. } => value = was,
            Receiver::Variable(at) => {
                return sources.memo.reads.unreachable.borrow().contains(&(
                    uri_id,
                    *at,
                    sources.object,
                    sources.bound.unwrap_or(0),
                    sources.made.unwrap_or(0),
                ));
            }
            _ => return false,
        }
    }
}

/// What one value reaching a read is where the checks around the read hold.
enum Narrowed {
    /// This, narrowed or not. Boxed: a [`Typed`] is large, and the other two are nothing.
    Kept(Box<Typed>),
    /// No such value can reach the read: the checks rule it out.
    Gone,
    /// Nothing can be said: the value is untyped, and no check names a class for it.
    Refused,
}

/// One value reaching a read, narrowed by the facts [`cursor::Reaching::narrowed`] holds about it
/// (`of`: the write, or `None` for `nil` or the parameter's own value).
///
/// - **Truthiness, `nil?` and `== nil`** split the value as Ruby's truthiness does ([`Sides`]).
/// - **`is_a?(K)`** keeps each class that is `K` or below it, and makes one above `K` (or a module)
///   into `K`, since a subclass has every member. A class beside `K` is ruled out: Ruby has one
///   superclass. An untyped or guessed value becomes `K`: the code names it. Its negation drops
///   each class that is `K` or below.
/// - **`K` must resolve to a class.** A fact naming anything else narrows nothing, which is always
///   safe.
/// - **Facets survive where the classes do not move**, so a value `new` built keeps what it was
///   handed.
fn narrowed_by(
    sources: &Sources<'_>,
    uri_id: UriId,
    document: &Document,
    mut value: Option<Typed>,
    reaching: &cursor::Reaching,
    of: Option<usize>,
) -> Narrowed {
    let graph = sources.graph;
    let folds = Folds::of(graph);
    let classes = |offsets: &[u32]| -> Option<Vec<DeclarationId>> {
        offsets
            .iter()
            .map(|offset| {
                let offset = document.rebase.to_graph(*offset)?;
                let found = constant_at(graph, uri_id, offset, sources.layout)?;
                matches!(
                    graph.declarations().get(&found),
                    Some(Declaration::Namespace(Namespace::Class(_)))
                )
                .then_some(found)
            })
            .collect()
    };
    let below = |class: DeclarationId, named: &[DeclarationId]| {
        named
            .iter()
            .any(|above| class == *above || descends(graph, class, *above))
    };
    let below_one = |fold: Option<DeclarationId>, named: &[DeclarationId]| {
        fold.is_some_and(|fold| below(fold, named))
    };
    for narrowing in reaching
        .narrowed
        .iter()
        .filter(|narrowing| narrowing.of == of)
    {
        let fact = &narrowing.fact;
        if let cursor::Fact::Is {
            classes: offsets,
            nil,
        } = fact
            && value
                .as_ref()
                .is_none_or(|typed| typed.derivation.tier() == Tier::Guessed)
        {
            if let Some(named) = classes(offsets) {
                let sides = Sides {
                    classes: named,
                    nil: *nil,
                    ..Sides::default()
                };
                value = rebuilt(sides, &folds, Derivation::default(), Vec::new());
            }
            continue;
        }
        let Some(typed) = value.take() else {
            continue;
        };
        let held = Sides::of(&typed, &folds);
        let mut sides = Sides::of(&typed, &folds);
        match fact {
            cursor::Fact::Truthy => {
                sides.nil = false;
                sides.falsehood = false;
            }
            cursor::Fact::Falsy => {
                sides.classes.clear();
                sides.truth = false;
            }
            cursor::Fact::Nil => {
                sides.classes.clear();
                sides.truth = false;
                sides.falsehood = false;
            }
            cursor::Fact::NotNil => sides.nil = false,
            cursor::Fact::Is {
                classes: offsets,
                nil,
            } => {
                let Some(named) = classes(offsets) else {
                    value = Some(typed);
                    continue;
                };
                let mut kept: Vec<DeclarationId> = Vec::new();
                for class in std::mem::take(&mut sides.classes) {
                    // Only a module's instance may be any class. A class object is its
                    // singleton's one instance: `Article.is_a?(Array)` is `false`.
                    let module = matches!(
                        graph.declarations().get(&class),
                        Some(Declaration::Namespace(Namespace::Module(_)))
                    );
                    let now: Vec<DeclarationId> = if below(class, &named) {
                        vec![class]
                    } else {
                        named
                            .iter()
                            .copied()
                            .filter(|above| module || descends(graph, *above, class))
                            .collect()
                    };
                    for class in now {
                        if !kept.contains(&class) {
                            kept.push(class);
                        }
                    }
                }
                sides.classes = kept;
                sides.truth &= below_one(folds.truth, &named);
                sides.falsehood &= below_one(folds.falsehood, &named);
                sides.nil &= *nil || below_one(folds.nil, &named);
            }
            cursor::Fact::IsNot {
                classes: offsets,
                nil,
            } => {
                let Some(named) = classes(offsets) else {
                    value = Some(typed);
                    continue;
                };
                sides.classes.retain(|class| !below(*class, &named));
                sides.truth &= !below_one(folds.truth, &named);
                sides.falsehood &= !below_one(folds.falsehood, &named);
                sides.nil &= !*nil && !below_one(folds.nil, &named);
            }
        }
        if sides.classes.is_empty() && !sides.truth && !sides.falsehood && !sides.nil {
            return Narrowed::Gone;
        }
        let same = sides.classes == held.classes
            && sides.truth == held.truth
            && sides.falsehood == held.falsehood
            && sides.nil == held.nil;
        value = if same {
            Some(typed)
        } else {
            let arguments = typed.arguments.clone();
            rebuilt(sides, &folds, typed.derivation, arguments)
        };
        if value.is_none() {
            return Narrowed::Refused;
        }
    }
    value.map_or(Narrowed::Refused, |typed| Narrowed::Kept(Box::new(typed)))
}

/// [`Sides::typed`], where `nil` alone is `NilClass` itself, as [`Join::finish`] has it.
fn rebuilt(
    sides: Sides,
    folds: &Folds,
    derivation: Derivation,
    arguments: Vec<Option<DeclarationId>>,
) -> Option<Typed> {
    if sides.classes.is_empty() && !sides.truth && !sides.falsehood {
        return sides
            .nil
            .then(|| Some(Typed::of(folds.nil?, derivation)))
            .flatten();
    }
    sides.typed(folds, derivation, arguments)
}

/// Every member of one row of its own text resolved: each write, and the parameter's own value
/// where it can reach. Also whether any was still being answered in this cycle, and whether any
/// was ruled out.
///
/// `None` refuses the read: a member nothing can type, or one in text the graph has not seen.
/// [`Reads::pending`] is left cleared either way, for the caller to set.
fn typed_members(
    sources: &Sources<'_>,
    uri_id: UriId,
    document: &Document,
    variables: &cursor::Variables,
    at: u32,
    reaching: &cursor::Reaching,
) -> Option<(Vec<Typed>, bool, bool)> {
    let reads = &sources.memo.reads;
    let members = reaching
        .writes
        .iter()
        .filter_map(|write| Some((Some(*write), variables.assignment(*write)?)))
        .map(|(write, written)| (write, written.at, &written.shape))
        .chain(reaching.bound.iter().map(|shape| (None, at, &**shape)));
    let mut typed = Vec::new();
    let mut waiting = false;
    let mut gone = false;
    for (write, place, shape) in members {
        let (Some(place), Some(shape)) = (
            document.rebase.to_graph(place),
            shape.rebased(&document.rebase),
        ) else {
            reads.pending.set(false);
            return None;
        };
        let scope = sources.scope_at(uri_id, place);
        let one = pending_aware(sources, || method_receiver(sources, uri_id, &shape, &scope));
        if one.is_none() && reads.pending.get() {
            waiting = true;
            continue;
        }
        // A value rooted at a read no value reaches never took effect.
        if one.is_none() && dead(sources, uri_id, &shape) {
            gone = true;
            continue;
        }
        // A check the read is under narrows this value, or rules it out.
        let one = if reaching.narrowed.is_empty() {
            one
        } else {
            match narrowed_by(sources, uri_id, document, one, reaching, write) {
                Narrowed::Kept(one) => Some(*one),
                Narrowed::Gone => {
                    gone = true;
                    continue;
                }
                Narrowed::Refused => None,
            }
        };
        let Some(mut one) = one else {
            reads.pending.set(false);
            return None;
        };
        // An empty container its method fills says what it holds.
        if let Some(fill) = write.and_then(|write| variables.fill(write))
            && let Some(held) = filled_with(sources, uri_id, document, fill)
        {
            one = one.holding(held);
        }
        typed.push(one);
    }
    reads.pending.set(false);
    Some((typed, waiting, gone))
}

/// What an empty container holds once its method has filled it ([`cursor::Fill`]): at
/// each position of its class's type parameter, the one class every value added there is, or
/// `None` where they are not one class. `None` altogether where a value is untyped or only guessed,
/// so the container stays as the literal said.
fn filled_with(
    sources: &Sources<'_>,
    uri_id: UriId,
    document: &Document,
    fill: &cursor::Fill,
) -> Option<Vec<Option<DeclarationId>>> {
    let mut held = Vec::new();
    for values in &fill.held {
        let mut class: Option<Option<DeclarationId>> = None;
        for (at, value) in values {
            let place = document.rebase.to_graph(*at)?;
            let value = value.rebased(&document.rebase)?;
            let scope = sources.scope_at(uri_id, place);
            let typed = method_receiver(sources, uri_id, &value, &scope)?;
            if typed.derivation.tier() == Tier::Guessed {
                return None;
            }
            let one = typed.one().filter(|_| !typed.nilable && !typed.boolean);
            match class {
                None => class = Some(one),
                Some(held) if held != one => class = Some(None),
                Some(_) => {}
            }
        }
        held.push(class.flatten());
    }
    held.iter().any(Option::is_some).then_some(held)
}

/// How many classes an object may be an instance of before a read of its variable is refused.
///
/// A bound on cost, never on the answer: the fold is over all of them or none. `ApplicationRecord`
/// in the largest corpus has a few hundred descendants; a read in `Object` would have every class.
const OBJECT_CLASSES: usize = 2048;

/// How many documents a read may have to scan for its variable's name before it is refused.
///
/// The scan is a text search, and a document is walked only when its text holds the name, so this
/// bounds reading files, not parsing them. A controller's hierarchy is a few hundred documents,
/// most of them the framework's.
const WRITER_DOCUMENTS: usize = 8192;

/// Which half of an object's variables a write or a read is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Side {
    /// An instance's: written in an instance method.
    Instance,
    /// A class object's: written in a class body, a `def self.` or a `class << self`.
    Class,
}

/// Every class an object can be an instance of, and every namespace whose methods can run on it.
///
/// A read of `@x` is written in one namespace, but the object may be an instance of any class
/// below it, and any method of any class above *that* can write `@x`. So the writes that reach the
/// read are those of every ancestor of every descendant.
pub(super) struct Hierarchy {
    /// The classes the object can be an instance of: the one the read is written in and every
    /// descendant the application loads, closed transitively.
    objects: Vec<DeclarationId>,
    /// Whether the read is a class object's variable.
    class_side: bool,
    /// Whose writes reach: a namespace, and which side of it.
    owners: HashSet<(DeclarationId, Side)>,
    /// Every Ruby document of every namespace in [`Self::owners`], sorted.
    documents: Vec<(String, UriId)>,
}

/// The namespace a class, module or singleton class is written as: a singleton class's attached
/// class, as many steps down as it takes.
fn base_of(graph: &Graph, mut id: DeclarationId) -> Option<DeclarationId> {
    // `class << self` inside `class << self` is as deep as anyone writes; a cycle is a graph bug,
    // and refusing is the answer to one.
    for _ in 0..4 {
        match graph.declarations().get(&id)? {
            Declaration::Namespace(Namespace::SingletonClass(_)) => {
                id = locator::attached_class(graph, id)?;
            }
            Declaration::Namespace(_) => return Some(id),
            _ => return None,
        }
    }
    None
}

/// `id`'s namespace, where its linearization can be trusted to be Ruby's: not cyclic ([`ancestry`]).
fn linearized(graph: &Graph, id: DeclarationId) -> Option<&Namespace> {
    let namespace = graph.declarations().get(&id)?.as_namespace()?;
    (!matches!(namespace.ancestors(), Ancestors::Cyclic(_))).then_some(namespace)
}

/// The member `member` of `owner`, found the way Ruby finds it, from the document `uri_id` asks in.
///
/// [`locator::find_loaded_member`], navigation's own walk, fenced from this cursor: **a member only
/// the test suite loads is not the application's**, unless the question is asked from
/// the suite itself. **A module's instance is some class's**, so a module that lacks the member asks
/// `Object`.
pub(crate) fn member_of(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: DeclarationId,
    member: StringId,
) -> Option<DeclarationId> {
    locator::find_loaded_member(sources.graph, fence_at(sources, uri_id), owner, member).ok()
}

/// [`member_of`] in `owner`'s own linearization, with no `Object` behind a module: for a walk of a
/// class's ancestors one at a time ([`from_super`]), where what comes after a module is the class's
/// next ancestor. `Object` there would answer before the superclass: ActiveSupport's `Object#with`
/// before a matcher's own `with`.
fn member_in(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: DeclarationId,
    member: StringId,
) -> Option<DeclarationId> {
    locator::loaded_member_in(sources.graph, fence_at(sources, uri_id), owner, member).ok()
}

/// The fence a member lookup asked from `uri_id` reads.
fn fence_at<'s>(sources: &Sources<'s>, uri_id: UriId) -> environment::Fence<'s> {
    environment::Fence::at(locator::uri_of(sources.graph, uri_id), sources.layout)
}

/// The member a **call** written on `owner` reaches: [`member_of`] with every rule that decides
/// whether Ruby would run it, so no call rung applies one and forgets another.
///
/// 1. **The test-suite fence** ([`member_of`]).
/// 2. **The root gate** ([`locator::declared_on_the_root`]): a member found on `Object` only
///    through `def`s written inside a block (`String.class_eval { def self.configure }`) is not
///    every object's, and navigation refuses it too.
/// 3. **Privacy**: a private member answers only a call written on `self`
///    ([`locator::is_private`], repair included). On any other receiver Ruby raises.
///
/// `super` is not a call on a receiver and skips the last two ([`from_super`]).
pub(crate) fn reach(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: DeclarationId,
    member: StringId,
    on_self: bool,
) -> Option<DeclarationId> {
    let graph = sources.graph;
    let found = member_of(sources, uri_id, owner, member)?;
    if !locator::declared_on_the_root(graph, Some(&sources.memo.blocks), found) {
        return None;
    }
    if !on_self && locator::is_private(graph, &sources.memo.modifiers, found) {
        return None;
    }
    Some(found)
}

/// Whether `ancestor` is in `object`'s linearization: an instance of `object` runs its methods.
fn descends(graph: &Graph, object: DeclarationId, ancestor: DeclarationId) -> bool {
    graph
        .declarations()
        .get(&object)
        .and_then(Declaration::as_namespace)
        .is_some_and(|namespace| {
            namespace
                .ancestors()
                .iter()
                .any(|found| matches!(found, Ancestor::Complete(id) if *id == ancestor))
        })
}

/// [`Hierarchy`] for the object whose `self` is written in `base`, read from the document
/// `uri_id`.
///
/// Held with the graph ([`Indexed::hierarchy`]), under the fence the document asks with, since
/// nothing else about the document changes what is kept. The request's own memo sits in front, so
/// the fence is read off the document's path once a request.
fn hierarchy(
    sources: &Sources<'_>,
    uri_id: UriId,
    base: DeclarationId,
    class_side: bool,
) -> Option<Rc<Hierarchy>> {
    let reads = &sources.memo.reads;
    let key = (base, class_side, uri_id);
    if let Some(held) = reads.hierarchies.borrow().get(&key) {
        return held.clone();
    }
    let graph = sources.graph;
    let made = graph.documents().get(&uri_id).and_then(|document| {
        let fence = environment::Fence::at(Some(document.uri()), sources.layout);
        graph.hierarchy(base, class_side, fence.key(), || {
            build_hierarchy(graph, fence, base, class_side)
        })
    });
    reads.hierarchies.borrow_mut().insert(key, made.clone());
    made
}

/// Every ancestor of `id`, as the owners whose writes reach its instances.
///
/// `None` where the linearization is not the one Ruby builds, since a missing ancestor may write
/// the variable and nothing can say what with:
/// - **an ancestor rubydex could not resolve** (`Ancestor::Partial`);
/// - **a chain rubydex found cyclic.** `class ApplicationController < ApplicationController` inside
///   `module Admin` names the top-level class in Ruby, and rubydex resolves it to the class itself
///   (an upstream defect), so the chain stops short of every real superclass.
fn ancestry(
    graph: &Graph,
    id: DeclarationId,
    owners: &mut HashSet<(DeclarationId, Side)>,
) -> Option<()> {
    let namespace = linearized(graph, id)?;
    for ancestor in namespace.ancestors() {
        let Ancestor::Complete(ancestor) = ancestor else {
            return None;
        };
        let side = match graph.declarations().get(ancestor)? {
            Declaration::Namespace(Namespace::SingletonClass(_)) => Side::Class,
            _ => Side::Instance,
        };
        owners.insert((base_of(graph, *ancestor)?, side));
    }
    Some(())
}

/// See [`hierarchy`].
///
/// 1. **The objects are rubydex's descendants, closed.** A class's descendants are its subclasses
///    and, for a module, every namespace that includes it. A descendant the application never
///    loads (a test double under `spec/`, a scratch file) is dropped unless the read is in one,
///    as [`environment`] rules for every surface.
/// 2. **A class object's variable** is read in a class body or a singleton method, and the object
///    is the class or a subclass (a singleton method is inherited; a module's is not). Its
///    writers are its singleton class's ancestors. A class with no singleton class in the graph
///    has written no singleton method, so both sides of its ancestors stand in: wider, never
///    narrower.
/// 3. **Every ancestor must have resolved**, or the answer would rest on the ones that did.
fn build_hierarchy(
    graph: &Indexed,
    fence: environment::Fence<'_>,
    base: DeclarationId,
    class_side: bool,
) -> Option<Hierarchy> {
    let module = matches!(
        graph.declarations().get(&base)?,
        Declaration::Namespace(Namespace::Module(_))
    );
    let mut objects = vec![base];
    let mut seen: HashSet<DeclarationId> = HashSet::from([base]);
    let mut next = 0;
    // A module's singleton methods are the module's own: no includer runs them.
    while next < objects.len() && !(class_side && module) {
        let namespace = graph.declarations().get(&objects[next])?.as_namespace()?;
        next += 1;
        for descendant in namespace.descendants() {
            if !seen.insert(*descendant) {
                continue;
            }
            let Some(Declaration::Namespace(found)) = graph.declarations().get(descendant) else {
                continue;
            };
            // A class object's subclasses inherit its singleton methods; nothing else does.
            if class_side && !matches!(found, Namespace::Class(_)) {
                continue;
            }
            if fence.loadable(graph, *descendant) && fence.inside(graph, *descendant) {
                objects.push(*descendant);
                if objects.len() > OBJECT_CLASSES {
                    return None;
                }
            }
        }
    }

    // A class of the same last name that rubydex linearized as a cycle may be one of these objects'
    // subclasses it failed to record (`Indexed::cyclic_named`): its writes would be missing.
    for object in &objects {
        let name = graph.declarations().get(object)?.unqualified_name();
        if graph
            .cyclic_named(&name)
            .iter()
            .any(|cyclic| cyclic != object)
        {
            return None;
        }
    }

    let mut owners = HashSet::new();
    for object in &objects {
        if !class_side {
            ancestry(graph, *object, &mut owners)?;
            continue;
        }
        match singleton_of(graph, *object) {
            Some(singleton) => ancestry(graph, singleton, &mut owners)?,
            None => {
                let namespace = linearized(graph, *object)?;
                for ancestor in namespace.ancestors() {
                    let Ancestor::Complete(ancestor) = ancestor else {
                        return None;
                    };
                    let ancestor = base_of(graph, *ancestor)?;
                    owners.insert((ancestor, Side::Class));
                    owners.insert((ancestor, Side::Instance));
                }
            }
        }
    }

    let mut namespaces: Vec<DeclarationId> = owners.iter().map(|(id, _)| *id).collect();
    namespaces.sort_unstable();
    namespaces.dedup();
    let mut documents: Vec<(String, UriId)> = Vec::new();
    let mut held: HashSet<UriId> = HashSet::new();
    for namespace in namespaces {
        for definition in locator::definitions_of(graph, namespace) {
            let document = *definition.uri_id();
            let Some(uri) = graph.documents().get(&document).map(|found| found.uri()) else {
                continue;
            };
            if !writes_ruby(uri)
                || (fence.on_trees() && fence.unloadable(uri))
                || fence.outside(uri)
                || !held.insert(document)
            {
                continue;
            }
            documents.push((uri.to_owned(), document));
            if documents.len() > WRITER_DOCUMENTS {
                return None;
            }
        }
    }
    documents.sort();
    Some(Hierarchy {
        objects,
        class_side,
        owners,
        documents,
    })
}

/// One write [`instance_read`] counts, and what it needs of it.
struct Counted<'d> {
    assignment: &'d cursor::Assignment,
    /// What it holds, in the graph's coordinates.
    shape: Receiver,
    /// The scope its value is read in.
    scope: Scope,
    /// The namespace that wrote it, one of the read's object's.
    owner: DeclarationId,
    /// An `attr_writer`'s: it holds what the writer's calls pass ([`setter_values`]).
    setter: bool,
}

/// The writes of `name` in one document that count toward a read of an object in `hierarchies`:
/// [`instance_read`]'s third rule, a write counts where its own namespace is an owner.
///
/// `None` refuses the read: a write whose place, value or namespace the graph cannot say may be
/// the one that reaches. **Shared with [`instance_writes`]**, so the places a jump lists are
/// exactly the writes the type was folded from.
fn counted_writes<'d>(
    sources: &Sources<'_>,
    hierarchies: &[Rc<Hierarchy>],
    document: &'d Document,
    uri: &str,
    written_in: UriId,
    name: &str,
) -> Option<Vec<Counted<'d>>> {
    let graph = sources.graph;
    let written = &document.shapes(uri, sources.held_exits).variables;
    let mut counted = Vec::new();
    for write in written.instance_writes(name) {
        let Some(assignment) = written.assignment(write.write) else {
            continue;
        };
        let (Some(place), Some(shape)) = (
            document.rebase.to_graph(assignment.at),
            assignment.shape.rebased(&document.rebase),
        ) else {
            return None;
        };
        let scope = sources.scope_at(written_in, place);
        let owner = scope.nesting_id(graph).and_then(|id| base_of(graph, id))?;
        let sides: &[Side] = match write.level {
            Some(0) => &[Side::Instance],
            Some(_) => &[Side::Class],
            None => &[Side::Instance, Side::Class],
        };
        if !hierarchies.iter().any(|hierarchy| {
            sides
                .iter()
                .any(|side| hierarchy.owners.contains(&(owner, *side)))
        }) {
            continue;
        }
        counted.push(Counted {
            assignment,
            shape,
            scope,
            owner,
            setter: write.setter,
        });
    }
    Some(counted)
}

/// Where the instance variable read starting at `at` (the reading document's own coordinates) can
/// have been assigned: every write [`instance_read`] folds, across the files of every class its object
/// can be.
#[derive(Debug, Default)]
pub struct VariableWrites {
    /// The variable's declaration for a card: on the reading class or an ancestor where one writes
    /// it, else on the first class that does (a subclass).
    pub declaration: Option<DeclarationId>,
    /// By document, the spans of the variable's name at each write, in that document's own text.
    pub places: Vec<(String, Vec<(u32, u32)>)>,
}

/// [`VariableWrites`] for a read in a class or module body, a block a signature rebinds, or a
/// template: what `definition` lists and `hover` names where the reading file writes nothing and
/// no type came out.
///
/// - **The same object, hierarchy and write rule** as [`instance_read`] ([`read_on`],
///   [`counted_writes`]), so the places are the writes the type is folded from, subclasses'
///   included: a read in a parent can run on a subclass's object, whose own methods write it. A
///   template's are every renderer's, after the path's own class has answered
///   (`renderer_writes`).
/// - **Only a write that spells the name** is a place; a reflective one counts toward the type but
///   has no name to land on.
/// - **The declaration is one class's.** A template's renderers are not one another's, so where
///   neither the path's class nor its ancestors write it and two classes do, the places stand with
///   no card.
/// - **Empty where it refuses**, as [`instance_read`] does: a read at the top level, a write the
///   graph cannot place, or a hierarchy too wide to read.
#[must_use]
pub fn instance_writes(sources: &Sources<'_>, uri_id: UriId, at: u32) -> VariableWrites {
    variable_writes(sources, uri_id, at).unwrap_or_default()
}

fn variable_writes(sources: &Sources<'_>, uri_id: UriId, at: u32) -> Option<VariableWrites> {
    let reads = &sources.memo.reads;
    let graph = sources.graph;
    let uri = graph.documents().get(&uri_id)?.uri().to_owned();
    let document = reads.documents.of(&uri, sources.read)?;
    let variables = &document.shapes(&uri, sources.held_exits).variables;
    let instance = variables.reaching(at)?.instance.as_ref()?;
    let on = read_on(sources, uri_id, &document, at, instance)?;
    object_writes(sources, uri_id, &instance.name, on)
}

/// [`VariableWrites`] of the variable `name` on what [`read_on`] said the read is on.
///
/// - **A renderer or a helper's view whose ancestry cannot be read is left out**, where others
///   remain (the user's call): its writes may be missing from the list, and the list
///   is still where the variable is set. The card refuses there, never a partial fold.
/// - **Every write is a place**: its name where it spells one, else the reflective
///   call or the accessor that makes it (`instance_variable_set("@#{name}", v)`,
///   `attr_writer :story`), which is where a reader looking for the write should land.
fn object_writes(
    sources: &Sources<'_>,
    uri_id: UriId,
    name: &str,
    on: ReadOn,
) -> Option<VariableWrites> {
    let reads = &sources.memo.reads;
    let graph = sources.graph;
    // Where the declaration may sit first, and whether the bases are one another's.
    let (bases, class_side, preferred, unrelated) = match on {
        ReadOn::Object(base, class_side) => (vec![base], class_side, Some(base), false),
        ReadOn::Rendered(Renderers { bases, rendered }) => {
            let preferred = rendered.map(|rendered| rendered.declaration);
            (bases, false, preferred, true)
        }
        ReadOn::Viewed(bases) => {
            let preferred = bases.first().copied();
            (bases, false, preferred, true)
        }
        ReadOn::Unnamed => return None,
    };
    let hierarchies: Vec<Rc<Hierarchy>> = if unrelated {
        bases
            .iter()
            .filter_map(|base| hierarchy(sources, uri_id, *base, class_side))
            .collect()
    } else {
        bases
            .iter()
            .map(|base| hierarchy(sources, uri_id, *base, class_side))
            .collect::<Option<_>>()?
    };
    if hierarchies.is_empty() {
        return None;
    }
    let bare = name.trim_start_matches('@');
    let mut found = VariableWrites::default();
    let mut owners: Vec<DeclarationId> = Vec::new();
    for (written, written_in) in documents_of(&hierarchies) {
        let other = reads.documents.of(written, sources.read)?;
        if !may_write(sources, *written_in, written, &other, bare) {
            continue;
        }
        let counted = counted_writes(sources, &hierarchies, &other, written, *written_in, name)?;
        let mut spans = Vec::new();
        for write in &counted {
            let at = write.assignment.at;
            let Some(rest) = other.source.get(at as usize..) else {
                continue;
            };
            if rest.starts_with(name) {
                spans.push((at, at + name.len() as u32));
                owners.push(write.owner);
                continue;
            }
            let length = rest
                .bytes()
                .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                .count();
            if length > 0 {
                spans.push((at, at + length as u32));
            }
        }
        if !spans.is_empty() {
            found.places.push((written.clone(), spans));
        }
    }
    // By file, as the in-file walk and the renderer's jump list them: several hierarchies are
    // walked one after another.
    found.places.sort();
    // Where it or an ancestor writes it: the reading class's, a full template's path's, or a
    // helper's own.
    let own = preferred.and_then(|base| {
        owners
            .iter()
            .find(|owner| **owner == base || descends(graph, base, **owner))
    });
    let owner = if unrelated {
        // Else the one class that does. A template's renderers are not one another's, so two
        // writers say nothing about which the reader means, and the places stand without one.
        own.or_else(|| {
            let first = owners.first()?;
            owners.iter().all(|owner| owner == first).then_some(first)
        })
    } else {
        // Else the first subclass that writes it.
        own.or_else(|| owners.first())
    };
    found.declaration = owner.and_then(|owner| {
        let named = graph.declarations().get(owner)?.name().to_owned();
        let declared = DeclarationId::from(format!("{named}#{name}").as_str());
        graph.declarations().get(&declared).map(|_| declared)
    });
    Some(found)
}

/// Whether the document at `uri` can write the variable spelled `bare` (`@` left off): its text
/// spells the name, or it is the application's and makes a reflective write, whose name it may
/// build (`instance_variable_set("@#{x}", v)`) and so never spell.
///
/// - **Without the second half the walk skipped such a document**, and a read folded the spelled
///   writes alone where the rule says the reflective one refuses it (an admin
///   engine's `ResourceController`).
/// - **Only the application's**, for [`written_by_another`]'s reason: one fully dynamic helper in
///   a gem would otherwise answer every variable in the project. actionpack's test helper removes
///   every variable a controller has, and made a never-written `@homeabout` a `NilClass`.
/// - **rubydex's call index answers the second half** ([`Indexed::reflective_documents`]) while
///   the text is the graph's, before the name is searched for. Scanning the text for both writers
///   as well cost three passes over every application file of every hierarchy a read folds. A
///   buffer edited since the last settle is scanned: the index does not know it yet.
fn may_write(
    sources: &Sources<'_>,
    uri_id: UriId,
    uri: &str,
    document: &Document,
    bare: &str,
) -> bool {
    let reflective = || {
        if document.rebase.is_identity() {
            sources
                .graph
                .reflective_documents()
                .binary_search(&uri_id)
                .is_ok()
        } else {
            scopes::REFLECTIVE_WRITERS
                .iter()
                .any(|writer| spells(&document.source, writer))
        }
    };
    (sources.layout.is_own(uri) && reflective()) || spells(&document.source, bare)
}

/// Whether `text` holds `name` anywhere: [`may_write`]'s search, run over every writer document of
/// an object for every read.
///
/// **`memchr`'s `memmem`, not `str::contains`**, with the same answer. std's search has no vector
/// path on aarch64, and there it ran 5-11 times slower over actionpack's sources: a seventh of an
/// average hover (2026-09-29).
fn spells(text: &str, name: &str) -> bool {
    memchr::memmem::find(text.as_bytes(), name.as_bytes()).is_some()
}

/// Whose variable a read of an instance variable is ([`read_on`]).
enum ReadOn {
    /// An object a namespace names: its instances, or (`true`) its class object.
    Object(DeclarationId, bool),
    /// A template's, handed it by every class that renders it.
    Rendered(Renderers),
    /// A helper module's instance method's: the view's, so the helper's own and every renderer's
    ///.
    Viewed(Vec<DeclarationId>),
    /// Nothing names it: the top level (`main`), an island, or a template the convention does not
    /// read.
    Unnamed,
}

/// The classes that render a template, each handing it its own variables.
struct Renderers {
    /// Every class whose object can be the template's `self`.
    bases: Vec<DeclarationId>,
    /// The class a full template's path names, whose lines the derivation records apart. `None` for
    /// a partial.
    rendered: Option<views::RenderedBy>,
}

/// Whose variable the read at `at` (the reading document's own coordinates) is, for the card
/// ([`instance_read`]) and the jump ([`variable_writes`]) alike. `None` refuses it.
///
/// - **A body read is on the namespace it is written in**, at its level: an instance method's
///   reads are its instances', a class body's and a `def self.`'s the class object's. `class <<
///   self`'s own body is the singleton class object's, which nobody writes.
/// - **A read in a block written straight in a namespace body** ([`cursor::InstanceRead::loose`])
///   is on what the block's call runs it against ([`loose_read_on`]).
/// - **A template's is its renderers'** ([`renderers`]), and **a helper's instance method's**
///   the view's ([`helps`]).
fn read_on(
    sources: &Sources<'_>,
    uri_id: UriId,
    document: &Document,
    at: u32,
    instance: &cursor::InstanceRead,
) -> Option<ReadOn> {
    let graph = sources.graph;
    if instance.loose {
        let (object, class_side) = loose_read_on(sources, uri_id, document.rebase.to_graph(at)?)?;
        return Some(ReadOn::Object(object, class_side));
    }
    match instance.level {
        Some(level @ (0 | 1)) => {
            let place = document.rebase.to_graph(at)?;
            let nesting = sources.scope_at(uri_id, place).nesting_id(graph)?;
            let base = base_of(graph, nesting)?;
            if level == 0 && helps(sources, uri_id, base) {
                return Some(ReadOn::Viewed(viewed_by(sources, base)));
            }
            Some(ReadOn::Object(base, level == 1))
        }
        Some(_) => None,
        None => match renderers(sources, uri_id) {
            Some(renderers) => renderers.map(ReadOn::Rendered),
            None => Some(ReadOn::Unnamed),
        },
    }
}

/// The object a read in a block written straight in a namespace body is on: what the block's call
/// runs it against ([`rebound_self`]), as [`object_of`] says. A callback's `if:` lambda runs on the controller, a
/// mailer's `default to:` lambda on the mailer.
///
/// - **Only one class's instances or its class object.** A union (a concern's `included do`
///   with several includers) has no one hierarchy to fold, and a class object's own class object
///   has variables nobody writes.
/// - **A block nothing says rebinds refuses**, as before: a DSL may run it on either side.
fn loose_read_on(
    sources: &Sources<'_>,
    uri_id: UriId,
    place: u32,
) -> Option<(DeclarationId, bool)> {
    let one = rebound_self(sources, uri_id, place)??.one()?;
    object_of(sources.graph, one)
}

/// The object a class stands for as a receiver or a `self`: an instance of a class or module, or
/// (`true`) the class object a singleton class is. `None` for a class object's own class object,
/// whose variables nobody writes, and for anything not a namespace.
#[must_use]
pub fn object_of(graph: &Graph, class: DeclarationId) -> Option<(DeclarationId, bool)> {
    match graph.declarations().get(&class)? {
        Declaration::Namespace(Namespace::SingletonClass(_)) => {
            let attached = locator::attached_class(graph, class)?;
            match graph.declarations().get(&attached)? {
                Declaration::Namespace(Namespace::Class(_) | Namespace::Module(_)) => {
                    Some((attached, true))
                }
                _ => None,
            }
        }
        Declaration::Namespace(_) => Some((class, false)),
        _ => None,
    }
}

/// The namespace the code at `at` (graph coordinates) is written in, as a class or module: a
/// singleton class's attached class. What a macro written there makes objects of.
#[must_use]
pub fn namespace_at(sources: &Sources<'_>, uri_id: UriId, at: u32) -> Option<DeclarationId> {
    let graph = sources.graph;
    base_of(graph, sources.scope_at(uri_id, at).nesting_id(graph)?)
}

/// An instance variable a symbol names on an object, where no read spells it:
/// `client.instance_variable_get(:@base_uri)`, `delegate :render, to: :@template`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedVariable {
    /// As Ruby spells it, `@` included.
    pub name: String,
    /// The class or module whose objects hold it.
    pub object: DeclarationId,
    /// Whether it is the class object's.
    pub class_side: bool,
}

/// Whether `member` reads the variable its first argument names: Ruby's own
/// `instance_variable_get` and `instance_variable_defined?`. A class's own method of that name
/// says nothing about what it reads.
#[must_use]
pub fn reads_a_variable(graph: &Graph, member: DeclarationId) -> bool {
    graph.declarations().get(&member).is_some_and(|declared| {
        matches!(
            declared.name(),
            "Kernel#instance_variable_get()"
                | "Kernel#instance_variable_defined?()"
                | "Object#instance_variable_get()"
                | "Object#instance_variable_defined?()"
        )
    })
}

/// What a variable a symbol names holds: [`instance_read`]'s fold over its object, as a read in
/// one of the object's methods that has not written it would be.
#[must_use]
pub fn named_read(sources: &Sources<'_>, uri_id: UriId, named: &NamedVariable) -> Option<Typed> {
    if written_by_another(sources, uri_id, &named.name) {
        return None;
    }
    let instance = cursor::InstanceRead {
        name: named.name.clone(),
        level: Some(i32::from(named.class_side)),
        set_here: false,
        loose: false,
        method: None,
        decided: None,
    };
    fold_objects(
        sources,
        uri_id,
        &instance,
        Objects {
            bases: vec![named.object],
            class_side: named.class_side,
            rendered: None,
        },
        (Vec::new(), false),
    )
}

/// Where a variable a symbol names is written ([`VariableWrites`]), as for a read of it
/// ([`instance_writes`]).
#[must_use]
pub fn named_writes(sources: &Sources<'_>, uri_id: UriId, named: &NamedVariable) -> VariableWrites {
    object_writes(
        sources,
        uri_id,
        &named.name,
        ReadOn::Object(named.object, named.class_side),
    )
    .unwrap_or_default()
}

/// Where the block straight in a namespace body around `at` (graph coordinates) runs, as a level
/// of the namespace it is written in, [`scopes`]' count: 0 where its call runs it on an instance
/// of that namespace, 1 on its class object. `None` where it runs on another object or nothing
/// says.
///
/// The in-file walk's half of [`loose_read_on`], so a highlight and a jump group a loose
/// occurrence with the variable the card reads it as.
#[must_use]
pub fn rebound_level(sources: &Sources<'_>, uri_id: UriId, at: u32) -> Option<i32> {
    let (object, class_side) = loose_read_on(sources, uri_id, at)?;
    let graph = sources.graph;
    let written = base_of(graph, sources.scope_at(uri_id, at).nesting_id(graph)?)?;
    (object == written).then_some(i32::from(class_side))
}

/// [`Renderers`] of the template filed under `uri_id`; `None` where it is not a template the
/// convention reads, `Some(None)` where its renderers refuse it.
///
/// - **A full template's path names a class** ([`rendered_by`]), and where it names none the
///   template is not read. Every class whose render call names it joins
///   ([`template_renderers`]).
/// - **A partial's path names nothing**: it is read wherever `[rails] views` is on,
///   from every class the convention renders a view from ([`every_renderer`]), and every class
///   whose render call names it. `user_notifications/digest/_stats.html.erb` names no class, and
///   the mailer whose view renders it hands it its variables.
/// - **A layout's path names nothing either**: it is read from every class whose views it wraps
///   ([`layout_renderers`]), and every class whose render call names it.
/// - **No renderer at all refuses**: no object to read is not one that initializes everything.
fn renderers(sources: &Sources<'_>, uri_id: UriId) -> Option<Option<Renderers>> {
    let graph = sources.graph;
    let path = DocUri::from_graph_uri(graph.documents().get(&uri_id)?.uri())?.to_file_path()?;
    let partial =
        sources.views.reads(&path) && rails::template_of(&path).is_some_and(|(_, partial)| partial);
    let (rendered, layout) = match (partial, sources.views.rendered_by(graph, &path)) {
        (true, _) => (None, None),
        (false, Some(rendered)) => (Some(rendered), None),
        (false, None) => (None, Some(sources.views.lays_out(&path)?)),
    };
    Some((|| {
        let (others, _) = template_renderers(sources, uri_id, layout.is_some())?;
        let mut bases: Vec<DeclarationId> = match (&rendered, &layout) {
            (Some(rendered), _) => vec![rendered.declaration],
            (None, Some(logical)) => layout_renderers(sources, logical),
            (None, None) => every_renderer(sources).to_vec(),
        };
        for other in others.iter() {
            if !bases.contains(other) {
                bases.push(*other);
            }
        }
        (!bases.is_empty()).then_some(Renderers { bases, rendered })
    })())
}

/// Whether `base` is an application helper module: a module written under `app/helpers`, where
/// `[rails] views` is on (`views::Views::helps`). Rails includes it into every view context, so
/// its instance methods run on the view, whose variables are its renderer's: one application's
/// `I18nHelper` reading `@home_page`, which `StoriesController` sets.
fn helps(sources: &Sources<'_>, uri_id: UriId, base: DeclarationId) -> bool {
    let graph = sources.graph;
    matches!(
        graph.declarations().get(&base),
        Some(Declaration::Namespace(Namespace::Module(_)))
    ) && graph
        .documents()
        .get(&uri_id)
        .and_then(|document| DocUri::from_graph_uri(document.uri()))
        .and_then(|uri| uri.to_file_path())
        .is_some_and(|path| sources.views.helps(&path))
}

/// The objects a helper module's instance method runs on ([`helps`]): the module's own (a class
/// may still `include` it), and every class the convention renders a view from, since any view
/// may call it ([`every_renderer`]).
fn viewed_by(sources: &Sources<'_>, helper: DeclarationId) -> Vec<DeclarationId> {
    let mut bases = vec![helper];
    for renderer in every_renderer(sources).iter() {
        if !bases.contains(renderer) {
            bases.push(*renderer);
        }
    }
    bases
}

/// Every Ruby document of every hierarchy, each once, in the hierarchies' order.
fn documents_of(hierarchies: &[Rc<Hierarchy>]) -> Vec<&(String, UriId)> {
    let mut held: HashSet<UriId> = HashSet::new();
    hierarchies
        .iter()
        .flat_map(|hierarchy| hierarchy.documents.iter())
        .filter(|(_, document)| held.insert(*document))
        .collect()
}

/// What an instance variable read holds: every write any method of its object can make, folded.
///
/// - **The object is placed by the read's own namespace** ([`Hierarchy`]). A template has none, so
///   its variables are the controller's its path names ([`views::Views::rendered_by`]), plus its
///   own writes. Anything else at the top level is `main`'s, and only its own text writes it.
/// - **A document is walked only if its text holds the name.** The walk is the held one, so a
///   framework file is walked once per session, not once per read.
/// - **A write is counted where its own namespace is an owner**, asked of the graph, not of the
///   file's spelling: a class reopened as `module Admin; class Users` and as `Admin::Users` is one.
/// - **No write anywhere refuses the read**, not `NilClass`: a variable nothing visibly writes is
///   more often written by code this cannot see than never written.
/// - **A guess in another file refuses the read**: the read's own spelling guesses the same thing
///   for free, without a file and line that look like evidence.
/// - **`nil` joins** unless the reading method has already written it, or every class the object can
///   be runs an `initialize` that does ([`initialized`]).
fn instance_read(
    sources: &Sources<'_>,
    uri_id: UriId,
    document: &Document,
    variables: &cursor::Variables,
    at: u32,
    reaching: &cursor::Reaching,
    instance: &cursor::InstanceRead,
) -> Option<Typed> {
    let reads = &sources.memo.reads;
    let graph = sources.graph;
    if written_by_another(sources, uri_id, &instance.name) {
        return None;
    }
    // The reading method's last write, with nothing between that can run code, is the
    // value. No other method can run to write it, and the write ran on every path.
    if let Some(write) = instance.decided {
        let only = cursor::Reaching {
            writes: Rc::from([write]),
            nil: false,
            bound: None,
            instance: None,
            narrowed: Box::default(),
        };
        let (typed, pending, _) = typed_members(sources, uri_id, document, variables, at, &only)?;
        if typed.is_empty() {
            reads.pending.set(pending);
            return None;
        }
        return fold_reached(graph, typed, false);
    }
    let mut typed = Vec::new();
    let mut waiting = false;
    let (bases, class_side, rendered) = match read_on(sources, uri_id, document, at, instance)? {
        ReadOn::Object(written, class_side) => {
            // A body read for a known receiver ([`Sources::object`]) runs on that object, which is
            // narrower than every object the body's own namespace can be part of.
            let base = match sources.object {
                Some(object)
                    if !class_side && object != written && descends(graph, object, written) =>
                {
                    object
                }
                _ => written,
            };
            (vec![base], class_side, None)
        }
        // A reflective write on the top level's `self` in this text may be the write that reaches.
        ReadOn::Unnamed | ReadOn::Rendered(_) if variables.reaches_main(&instance.name) => {
            return None;
        }
        ReadOn::Unnamed => {
            // `main`'s, or an island's: only this text writes it.
            if reaching.writes.is_empty() {
                return None;
            }
            let (typed, waiting, _) =
                typed_members(sources, uri_id, document, variables, at, reaching)?;
            if typed.is_empty() && !reaching.nil {
                reads.pending.set(waiting);
                return None;
            }
            return fold_reached(graph, typed, reaching.nil);
        }
        ReadOn::Rendered(Renderers { bases, rendered }) => {
            // The template's own writes reach too. They cannot remove `nil`: a partial may be
            // rendered before them.
            let (own, pending, _) =
                typed_members(sources, uri_id, document, variables, at, reaching)?;
            typed = own;
            waiting = pending;
            (bases, false, rendered)
        }
        ReadOn::Viewed(bases) => (bases, false, None),
    };
    fold_objects(
        sources,
        uri_id,
        instance,
        Objects {
            bases,
            class_side,
            rendered,
        },
        (typed, waiting),
    )
}

/// The objects a read folds the writes of: [`read_on`]'s answer, resolved.
struct Objects {
    bases: Vec<DeclarationId>,
    /// Whether the read is a class object's variable.
    class_side: bool,
    /// The class a full template's path names, whose lines the derivation records apart.
    rendered: Option<views::RenderedBy>,
}

/// [`instance_read`]'s fold: every write of the variable any hierarchy of `objects` makes, joined
/// with `own` (what the reading text's own writes gave, and whether one was still being
/// answered). Shared with a variable a symbol names ([`named_read`]).
fn fold_objects(
    sources: &Sources<'_>,
    uri_id: UriId,
    instance: &cursor::InstanceRead,
    objects: Objects,
    own: (Vec<Typed>, bool),
) -> Option<Typed> {
    let reads = &sources.memo.reads;
    let graph = sources.graph;
    let Objects {
        bases,
        class_side,
        rendered,
    } = objects;
    let (mut typed, mut waiting) = own;
    let hierarchies: Vec<Rc<Hierarchy>> = bases
        .iter()
        .map(|base| hierarchy(sources, uri_id, *base, class_side))
        .collect::<Option<_>>()?;
    let documents = documents_of(&hierarchies);

    let bare = instance.name.trim_start_matches('@');
    let mut classes: Vec<FromClass> = Vec::new();
    let mut rendered_lines = Vec::new();
    // What an `attr_writer` of the variable writes, asked once however many declare it.
    let mut set: Option<Writes> = None;
    for (uri, written_in) in documents {
        let Some(document) = reads.documents.of(uri, sources.read) else {
            // A file of the class that cannot be read may hold a write: nothing can be said.
            reads.pending.set(false);
            return None;
        };
        // `bare`, not the name: `attr_writer :story` never spells `@story`.
        if !may_write(sources, *written_in, uri, &document, bare) {
            continue;
        }
        let Some(counted) = counted_writes(
            sources,
            &hierarchies,
            &document,
            uri,
            *written_in,
            &instance.name,
        ) else {
            reads.pending.set(false);
            return None;
        };
        let mut lines: Vec<(DeclarationId, u32)> = Vec::new();
        for Counted {
            assignment,
            shape,
            scope,
            owner,
            setter,
        } in counted
        {
            if setter {
                if set.is_none() {
                    match pending_aware(sources, || {
                        setter_values(sources, uri_id, &hierarchies, bare)
                    }) {
                        Some(values) => {
                            typed.extend(values.values.iter().cloned());
                            set = Some(values);
                        }
                        None if reads.pending.get() => {
                            waiting = true;
                            continue;
                        }
                        None => {
                            reads.pending.set(false);
                            return None;
                        }
                    }
                }
                if *written_in != uri_id {
                    lines.push((owner, line_of(&document.source, assignment.at)));
                }
                continue;
            }
            match pending_aware(sources, || {
                method_receiver(sources, *written_in, &shape, &scope)
            }) {
                Some(mut one) => {
                    if *written_in != uri_id {
                        if one.derivation.tier() == Tier::Guessed {
                            reads.pending.set(false);
                            return None;
                        }
                        // **The inner assignments are dropped**: they are offsets into another
                        // file, which every consumer would draw against the *cursor's* text. The
                        // derivation records that file and its lines instead.
                        one.derivation.assignments.clear();
                        lines.push((owner, line_of(&document.source, assignment.at)));
                    }
                    typed.push(one);
                }
                None if reads.pending.get() => waiting = true,
                None => {
                    reads.pending.set(false);
                    return None;
                }
            }
        }
        lines.sort_unstable();
        lines.dedup();
        let mut owners: Vec<DeclarationId> = lines.iter().map(|(owner, _)| *owner).collect();
        owners.dedup();
        for owner in owners {
            let here = lines
                .iter()
                .filter(|(of, _)| *of == owner)
                .map(|(_, line)| *line);
            if rendered
                .as_ref()
                .is_some_and(|rendered| rendered.declaration == owner)
            {
                rendered_lines.extend(here);
                continue;
            }
            let Some(class) = graph.declarations().get(&owner) else {
                continue;
            };
            classes.push(FromClass {
                class: class.name().to_owned(),
                file: where_written(sources, uri),
                lines: here.collect(),
            });
        }
    }
    reads.pending.set(false);
    if typed.is_empty() {
        // Nothing writes it that this can see, or everything that does is still being answered.
        reads.pending.set(waiting);
        return None;
    }
    let nil = set.is_some_and(|set| set.nil)
        || !instance.set_here
            && !hierarchies
                .iter()
                .all(|hierarchy| initialized(sources, hierarchy, &instance.name))
            && !written_before(sources, uri_id, instance, &hierarchies);
    let mut answer = fold_reached(graph, typed, nil)?;
    answer.derivation.classes = classes;
    if let Some(rendered) = rendered {
        rendered_lines.sort_unstable();
        answer.derivation.renderer = Some(FromRenderer {
            renderer: rendered.name,
            controller: rendered.controller,
            lines: rendered_lines,
        });
    }
    Some(answer)
}

/// Whether an application document writes a variable spelled `name` on an object it does not own:
/// `obj.instance_variable_set(:@story, v)`, or a pattern `name` fits.
///
/// - **Any object's variable of that spelling**, since the receiver is rarely typed. So a read of a
///   matching name is refused wherever it is.
/// - **Only the application's documents**, fenced as the reader's surface fences
///   ([`environment`]). A library sets its own objects' state, and one fully dynamic helper in a
///   gem would otherwise refuse every variable in the project.
/// - **rubydex's call index names the documents** ([`Indexed::reflective_documents`]), and each
///   one's names are held by the graph's content hash, so no file is read twice to learn nothing
///   changed.
fn written_by_another(sources: &Sources<'_>, uri_id: UriId, name: &str) -> bool {
    let graph = sources.graph;
    let documents = graph.reflective_documents();
    if documents.is_empty() {
        return false;
    }
    let Some(cursor) = graph
        .documents()
        .get(&uri_id)
        .map(|found| found.uri().to_owned())
    else {
        return true;
    };
    let fence = environment::Fence::at(Some(&cursor), sources.layout);
    documents.iter().any(|document| {
        let Some(found) = graph.documents().get(document) else {
            return false;
        };
        let uri = found.uri();
        if !writes_ruby(uri) || !application(sources, &fence, uri) {
            return false;
        }
        let read = || -> Option<ForeignNames> {
            let (source, _) = (sources.read)(uri)?;
            Some(
                scopes::reflections(&source)
                    .into_iter()
                    .filter(|reflection| reflection.on == scopes::Reflected::Other)
                    .flat_map(|reflection| reflection.names)
                    .collect(),
            )
        };
        let names = sources
            .held_exits
            .foreign(*document, found.content_hash(), read);
        // A document that cannot be read may hold such a write: nothing can be said.
        names.is_none_or(|names| names.iter().any(|spelled| can_be(sources, spelled, name)))
    })
}

/// Whether a document is the application's own and the asking document's fence admits it: what a
/// reader of the application's writes reads ([`written_by_another`], [`read_renderers`],
/// [`written_to`]).
fn application(sources: &Sources<'_>, fence: &environment::Fence<'_>, uri: &str) -> bool {
    sources.layout.is_own(uri)
        && !(fence.on_trees() && fence.unloadable(uri))
        && !fence.outside(uri)
}

/// How many calls of one method a reflective write's name is read from before any name is assumed.
///
/// A method's own parameter names the variable, so the callers say which ([`passed`]). A name
/// called from this many places is a common one, and reading them all is a cost with no answer
/// the fallback does not already give.
pub(crate) const CALL_SITES: usize = 64;

/// Whether a reflective write's `spelled` name can be the variable `name`, asking the callers where
/// it is a parameter's.
fn can_be(sources: &Sources<'_>, spelled: &scopes::Spelled, name: &str) -> bool {
    match spelled {
        scopes::Spelled::Argument { method, index } => passed(sources, method, *index)
            .iter()
            .any(|argument| argument.matches(name)),
        _ => spelled.matches(name),
    }
}

/// What every call of `method` passes as its positional argument `index`, from rubydex's call
/// index; the pattern every name fits where a call cannot be read or there are none to read.
///
/// **By name, not by receiver**: a call of another method spelled the same is read too, which can
/// only add names. Memoized for the request.
fn passed(sources: &Sources<'_>, method: &str, index: usize) -> ForeignNames {
    let everything = || -> ForeignNames {
        Rc::from(vec![scopes::Spelled::Like {
            head: String::new(),
            tail: String::new(),
        }])
    };
    let reads = &sources.memo.reads;
    let key = (method.to_owned(), index);
    if let Some(held) = reads.passed.borrow().get(&key) {
        return Rc::clone(held);
    }
    let graph = sources.graph;
    let sites = graph.calls_named(method);
    // Which version of each calling document the answer would be read from: the same versions
    // give the same answer, so a later request need not read them again.
    let versions: Versions = sites
        .iter()
        .map(|(document, _, _)| {
            let hash = graph
                .documents()
                .get(document)
                .map_or(0, |found| found.content_hash());
            (*document, hash)
        })
        .collect();
    if let Some((stamp, names)) = sources.held_exits.passed.borrow().get(&key)
        && *stamp == versions
    {
        let names = Rc::clone(names);
        reads.passed.borrow_mut().insert(key, Rc::clone(&names));
        return names;
    }
    let answer = if sites.is_empty() || sites.len() > CALL_SITES {
        everything()
    } else {
        let mut names: Vec<scopes::Spelled> = Vec::new();
        for &(document, start, end) in sites.iter() {
            let argument = graph
                .documents()
                .get(&document)
                .and_then(|found| reads.documents.of(found.uri(), sources.read))
                .and_then(|read| {
                    let span = read.rebase.span_to_buffer(ByteSpan { start, end })?;
                    scopes::argument_at(&read.source, span.start, index)
                });
            match argument {
                Some(spelled) => names.push(spelled),
                None => {
                    names = everything().to_vec();
                    break;
                }
            }
        }
        Rc::from(names)
    };
    sources
        .held_exits
        .passed
        .borrow_mut()
        .insert(key.clone(), (versions, Rc::clone(&answer)));
    reads.passed.borrow_mut().insert(key, Rc::clone(&answer));
    answer
}

/// Every class other than the convention's that renders the template filed under `uri_id`, and
/// whether it is a partial; `None` where its variables are not any class's.
///
/// - **A render call naming the template makes a renderer** of the object it runs on:
///   - in a class, the graph's class at the call (a module's renderers are its includers, which
///     its hierarchy already holds);
///   - in a view, the class that view is rendered by;
///   - in a helper, a partial or a layout, whichever view called it: **every class the convention
///     renders a view from** ([`every_renderer`]), a wider answer and never a narrower one.
/// - **A partial is named by views**, so its renderers come from there, not from its own path.
/// - **A layout is named only in so many words** (`layout`): the layout lookup chooses it, and a
///   render whose target only running Ruby knows (`render options`) would otherwise put every
///   class with one in every layout.
/// - **It refuses** where the call has a receiver written (`ApplicationController.render`, with
///   `assigns:` nobody writes) or cannot be placed.
/// - **Only the application's Ruby and templates are read**, fenced like the template; the
///   documents come from rubydex's call index, and `HeldExits` holds each one's calls by content
///   hash.
fn template_renderers(
    sources: &Sources<'_>,
    uri_id: UriId,
    layout: bool,
) -> Option<(Rc<[DeclarationId]>, bool)> {
    let graph = sources.graph;
    let reads = &sources.memo.reads;
    let cursor = graph.documents().get(&uri_id)?.uri().to_owned();
    let path = DocUri::from_graph_uri(&cursor)?.to_file_path()?;
    let (logical, partial) = rails::template_of(&path)?;
    let key = format!(
        "{logical}{}{}",
        if partial { "#partial" } else { "" },
        if layout { "#layout" } else { "" }
    );
    if let Some(held) = reads.renderers.borrow().get(&key) {
        return held.clone().map(|renderers| (renderers, partial));
    }
    let answer = read_renderers(sources, &cursor, &logical, partial, layout);
    reads.renderers.borrow_mut().insert(key, answer.clone());
    answer.map(|renderers| (renderers, partial))
}

/// What a bare name in a partial is as a local ([`partial_local`]).
#[derive(Debug, Clone)]
enum Local {
    /// No render call passes it and no strict-locals comment declares it: the name is a call.
    NotOne,
    /// A render call that can render the partial passes locals nothing can read, or a value
    /// nothing types, or a helper of the name answers where a call does not pass it.
    Refused,
    /// Every value the render calls pass it, joined, and where each is written: the document and
    /// the value's span in its text. Boxed: a [`Typed`] is large.
    Typed(Box<Typed>, Rc<[(String, (u32, u32))]>),
}

/// Spans by document, each in that document's own text: where a jump lands.
pub type Places = Vec<(String, Vec<(u32, u32)>)>;

/// A bare name in the partial filed under `uri_id` that its render calls pass as a local
///: what it holds, and where each call passes it, by document, as spans of that
/// document's text. For the card and the jump, which read the rung the margin does.
#[must_use]
pub fn partial_local_of(
    sources: &Sources<'_>,
    uri_id: UriId,
    name: &str,
) -> Option<(Typed, Places)> {
    let Local::Typed(typed, sites) = partial_local(sources, uri_id, name) else {
        return None;
    };
    let mut places: Places = Vec::new();
    for (uri, span) in sites.iter() {
        match places.iter_mut().find(|(held, _)| held == uri) {
            Some((_, spans)) => spans.push(*span),
            None => places.push((uri.clone(), vec![*span])),
        }
    }
    Some((*typed, places))
}

/// Every application jbuilder view, in id order: each may render a jbuilder partial
/// through a key written with `partial:`, a call no index lists by name. Once per request.
fn jbuilder_documents(sources: &Sources<'_>) -> Rc<[UriId]> {
    let reads = &sources.memo.reads;
    if let Some(held) = reads.jbuilders.borrow().as_ref() {
        return Rc::clone(held);
    }
    let mut documents: Vec<UriId> = sources
        .graph
        .documents()
        .iter()
        .filter(|(_, document)| {
            DocUri::from_graph_uri(document.uri())
                .and_then(|uri| uri.to_file_path())
                .is_some_and(|path| rails::is_jbuilder(&path))
        })
        .map(|(id, _)| *id)
        .collect();
    documents.sort_unstable();
    let documents: Rc<[UriId]> = Rc::from(documents);
    *reads.jbuilders.borrow_mut() = Some(Rc::clone(&documents));
    documents
}

/// The documents that write a render call, from rubydex's call index, in id order.
fn render_documents(sources: &Sources<'_>) -> Vec<UriId> {
    let mut documents: Vec<UriId> = rails::RENDER_CALLS
        .iter()
        .flat_map(|name| {
            sources
                .graph
                .calls_named(name)
                .iter()
                .map(|(document, _, _)| *document)
                .collect::<Vec<_>>()
        })
        .collect();
    documents.sort_unstable();
    documents.dedup();
    documents
}

/// What the bare name `name` reads in the partial filed under `uri_id`, where the partial's render
/// calls pass it as a local: **the union of every value they pass it**.
///
/// - **The calls are every render call that can name the partial** ([`rails::Render::names`]), in
///   the application's templates and Ruby, fenced like the partial. A call that does not pass the
///   name adds nothing: the partial raises `NameError` there, unless a helper of that name answers,
///   which refuses.
/// - **An object rendered by its own partial** (`render @posts`, `collection:` alone) renders this
///   one only where its class's `to_partial_path` is this partial (`rails::partial_of`); an
///   object nothing types may render any, and refuses the partial's own name.
/// - **A strict-locals comment** (`<%# locals: (post:, compact: false) -%>`) says which names are
///   locals, and a literal default joins.
/// - **Refused, never a partial fold**, where a call or the comment says the name is a local: then
///   locals a call does not list, a call in a class that renders something of its own (a
///   component) or a document that cannot be read may pass it anything. Without that word the name
///   stays a call, as it was. A value nothing types or only a guess does refuses too, and so does a
///   partial rendering itself.
fn partial_local(sources: &Sources<'_>, uri_id: UriId, name: &str) -> Local {
    let reads = &sources.memo.reads;
    let key = (uri_id, name.to_owned());
    if let Some(held) = reads.locals.borrow().get(&key) {
        return held.clone();
    }
    if reads.localizing.borrow().contains(&key) {
        return Local::Refused;
    }
    reads.localizing.borrow_mut().push(key.clone());
    let answer = read_partial_local(sources, uri_id, name);
    reads.localizing.borrow_mut().pop();
    reads.locals.borrow_mut().insert(key, answer.clone());
    answer
}

/// [`partial_local`], unmemoized.
fn read_partial_local(sources: &Sources<'_>, uri_id: UriId, name: &str) -> Local {
    let graph = sources.graph;
    let reads = &sources.memo.reads;
    let Some(cursor) = graph
        .documents()
        .get(&uri_id)
        .map(|document| document.uri().to_owned())
    else {
        return Local::NotOne;
    };
    let Some(path) = DocUri::from_graph_uri(&cursor).and_then(|uri| uri.to_file_path()) else {
        return Local::NotOne;
    };
    if !sources.views.reads(&path) {
        return Local::NotOne;
    }
    // jbuilder's handler defines `json` in every jbuilder template, a partial or not.
    let jbuilder = rails::is_jbuilder(&path);
    if jbuilder && name == rails::JBUILDER.0 {
        return declared(graph, rails::JBUILDER.1).map_or(Local::NotOne, |class| {
            Local::Typed(
                Box::new(Typed::of(class, Derivation::default())),
                Rc::from([]),
            )
        });
    }
    let Some((logical, true)) = rails::template_of(&path) else {
        return Local::NotOne;
    };
    let strict = sources
        .markup
        .markup(&cursor)
        .and_then(|markup| rails::strict_locals(&markup));
    let default = match &strict {
        Some(strict) => match strict
            .declared
            .iter()
            .find(|(declared, _)| declared == name)
        {
            Some((_, default)) => Some(*default),
            None if strict.open => None,
            None => return Local::NotOne,
        },
        None => None,
    };
    let own = logical.rsplit('/').next().unwrap_or(&logical).to_owned();
    let names_own = [
        own.clone(),
        format!("{own}_counter"),
        format!("{own}_iteration"),
    ]
    .contains(&name.to_owned());
    let fence = environment::Fence::at(Some(&cursor), sources.layout);
    let folds = Folds::of(graph);
    let mut join = Join::default();
    let (mut passed, mut unpassed) = (false, false);
    // A call whose locals cannot be read, which may pass the name: that refuses only a name
    // something else says is a local.
    let mut unread = false;
    let mut sites: Vec<(String, (u32, u32))> = Vec::new();
    // A jbuilder partial is rendered by a key written with `partial:` too, whose name no call
    // index can list, so every jbuilder view is a site to read.
    let mut documents = render_documents(sources);
    if jbuilder {
        documents.extend(jbuilder_documents(sources).iter());
        documents.sort_unstable();
        documents.dedup();
    }
    for document in documents {
        let Some(found) = graph.documents().get(&document) else {
            continue;
        };
        let uri = found.uri();
        let Some(site) = DocUri::from_graph_uri(uri).and_then(|uri| uri.to_file_path()) else {
            continue;
        };
        let template = views::is_view(&site);
        if !(template || writes_ruby(uri)) || !application(sources, &fence, uri) {
            continue;
        }
        let view = template || rails::is_helper(&site);
        // A JSON lookup finds jbuilder partials alone, an HTML view's ERB ones; a controller's may
        // be either.
        let format = |call: &rails::Render| {
            if call.json || rails::is_jbuilder(&site) {
                Some(true)
            } else {
                erb::is_template(&site).then_some(false)
            }
        };
        let finds = |call: &rails::Render| {
            call.names(&logical, true) && format(call).is_none_or(|json| json == jbuilder)
        };
        // The calls held across requests say which documents can render the partial at all, so
        // only those are parsed for their values. A document that cannot be read may pass it
        // anything.
        let held = || -> Option<HeldRenders> {
            let (source, rebase) = (sources.read)(uri)?;
            Some(
                rails::read_renders(&source, view)
                    .into_iter()
                    .map(|render| (rebase.to_graph(render.at), render))
                    .collect(),
            )
        };
        let Some(calls) = sources
            .held_exits
            .renders(document, found.content_hash(), held)
        else {
            unread = true;
            continue;
        };
        if !calls.iter().any(|(_, call)| finds(call)) {
            continue;
        }
        // Parsed for its values only where a call passes the name, or hands an object the
        // partial's own name. Every other call that can render the partial is decided by what the
        // held calls say: locals nobody can list, a call not passing the name, or a component's or
        // a service's `render`, which renders something of its own this cannot place.
        let passes = |call: &rails::Render| match &call.locals {
            rails::Locals::Named(locals) => locals.iter().any(|local| {
                local.name.in_partial(&logical) == name
                    || (names_own && matches!(local.value, rails::Value::Object(_)))
            }),
            rails::Locals::Unread => false,
        };
        let placeable = rails::renders_its_own_variables(&site);
        if !placeable || !calls.iter().any(|(_, call)| finds(call) && passes(call)) {
            for (_, call) in calls.iter().filter(|(_, call)| finds(call)) {
                match &call.locals {
                    rails::Locals::Named(_) if placeable => unpassed = true,
                    _ => unread = true,
                }
            }
            continue;
        }
        let Some(read) = reads.documents.of(uri, sources.read) else {
            unread = true;
            continue;
        };
        let passing = read.renders(view);
        for call in &passing.calls {
            if !finds(call) {
                continue;
            }
            let rails::Locals::Named(locals) = &call.locals else {
                unread = true;
                continue;
            };
            let value_of = |span: (u32, u32)| -> Option<Typed> {
                let shape = passing.values.get(&span)?.rebased(&read.rebase)?;
                let place = read.rebase.to_graph(span.0)?;
                let scope = sources.scope_at(document, place);
                let mut typed = method_receiver(sources, document, &shape, &scope)?;
                if typed.derivation.tier() == Tier::Guessed {
                    return None;
                }
                // Its lines are that document's, not the partial's.
                if document != uri_id {
                    typed.derivation.assignments.clear();
                }
                Some(typed)
            };
            // An object rendered by its own partial renders this one only where its class says so.
            let mut rendered_object: Option<Typed> = None;
            let mut collected = false;
            if let Some(span) = locals.iter().find_map(|local| match local.value {
                rails::Value::Object(span) => Some(span),
                _ => None,
            }) {
                let Some(object) = value_of(span) else {
                    if names_own {
                        return Local::Refused;
                    }
                    unpassed |= !locals
                        .iter()
                        .any(|local| local.name.in_partial(&logical) == name);
                    continue;
                };
                match rendered_as(
                    sources,
                    document,
                    &passing.values[&span],
                    &read,
                    &object,
                    &logical,
                ) {
                    Rendered::Elsewhere => continue,
                    Rendered::Unknown => {
                        if names_own {
                            return Local::Refused;
                        }
                        unpassed |= !locals
                            .iter()
                            .any(|local| local.name.in_partial(&logical) == name);
                        continue;
                    }
                    Rendered::Here(object, collection) => {
                        rendered_object = Some(*object);
                        collected = collection;
                    }
                }
            }
            // A collection rendered by its elements' partial hands each one's count and iteration.
            let counted = collected
                .then(|| {
                    [
                        (format!("{own}_counter"), rails::Value::Counter),
                        (format!("{own}_iteration"), rails::Value::Iteration),
                    ]
                    .into_iter()
                    .find(|(counted, _)| counted == name)
                    .map(|(_, value)| rails::Local {
                        name: rails::Name::Written(name.to_owned()),
                        value,
                    })
                })
                .flatten();
            let mut passes = locals
                .iter()
                .filter(|local| local.name.in_partial(&logical) == name)
                .chain(counted.as_ref())
                .peekable();
            if passes.peek().is_none() {
                unpassed = true;
                continue;
            }
            for local in passes {
                sites.push((
                    uri.to_owned(),
                    match local.value {
                        rails::Value::Written(span)
                        | rails::Value::Element(span)
                        | rails::Value::Object(span)
                        | rails::Value::Either(span) => span,
                        // The call, for what `collection:` hands beside each element.
                        rails::Value::Counter | rails::Value::Iteration => (call.at, call.at),
                    },
                ));
                let value = match local.value {
                    rails::Value::Written(span) => value_of(span),
                    rails::Value::Element(span) => {
                        element_of(sources, document, &passing.values, &read, span)
                    }
                    rails::Value::Object(_) => rendered_object.clone(),
                    // jbuilder's `as:`: each element of a collection, else the object itself.
                    rails::Value::Either(span) => value_of(span).and_then(|object| {
                        let collection = object.classes().iter().any(|class| {
                            member_of(sources, document, *class, StringId::from("to_ary()"))
                                .is_some()
                        });
                        if collection {
                            element_of(sources, document, &passing.values, &read, span)
                        } else {
                            Some(object)
                        }
                    }),
                    rails::Value::Counter => declared(graph, "Integer")
                        .map(|class| Typed::of(class, Derivation::default())),
                    rails::Value::Iteration => declared(graph, "ActionView::PartialIteration")
                        .map(|class| Typed::of(class, Derivation::default())),
                };
                let Some(value) = value else {
                    return Local::Refused;
                };
                passed = true;
                join.add(value, &folds);
            }
        }
    }
    match default {
        Some(rails::Default::Literal(class)) => {
            let Some(class) = declared(graph, class) else {
                return Local::Refused;
            };
            passed = true;
            join.add(Typed::of(class, Derivation::default()), &folds);
        }
        Some(rails::Default::Unread) => return Local::Refused,
        Some(rails::Default::Required) | None => {}
    }
    // A local by a call's or the comment's word, which a call nobody can read may pass anything.
    if unread && (passed || default.is_some()) {
        return Local::Refused;
    }
    if !passed {
        return Local::NotOne;
    }
    // Where a call renders it without the local, the name is a call there: a helper answers it.
    if unpassed
        && strict.is_none()
        && view_context(sources, uri_id)
            .and_then(|reachable| reachable.member(graph, &format!("{name}()")))
            .is_some()
    {
        return Local::Refused;
    }
    sites.sort_unstable();
    sites.dedup();
    match join.finish(&folds) {
        Some(typed) => Local::Typed(Box::new(typed), Rc::from(sites)),
        None => Local::Refused,
    }
}

/// Which partial an object a render call renders by itself renders as.
enum Rendered {
    /// This partial, as the classes of the object (or of each element) that render it, and
    /// whether it was a collection, which hands `_counter` and `_iteration` too.
    Here(Box<Typed>, bool),
    /// Another partial.
    Elsewhere,
    /// Any: a class that says its own `to_partial_path`, or an element nothing types.
    Unknown,
}

/// What `object`, handed to a render call at `span` of `read`, renders `logical` as: itself, or
/// each element where it is a collection (it answers `to_ary`, as `render` asks), with each class
/// rendering `rails::partial_of` its name.
fn rendered_as(
    sources: &Sources<'_>,
    document: UriId,
    shape: &Receiver,
    read: &Document,
    object: &Typed,
    logical: &str,
) -> Rendered {
    let graph = sources.graph;
    let collection = object
        .classes()
        .iter()
        .any(|class| member_of(sources, document, *class, StringId::from("to_ary()")).is_some());
    let rendered = if collection {
        let Some(element) = yielded_element(sources, document, shape, read) else {
            return Rendered::Unknown;
        };
        element
    } else {
        object.clone()
    };
    let mut here = Vec::new();
    for class in rendered.classes() {
        // A record's own `to_partial_path`, written in the application, may say anything.
        if let Some(own) = member_of(
            sources,
            document,
            *class,
            StringId::from("to_partial_path()"),
        ) && locator::definitions_of(graph, own)
            .iter()
            .any(|definition| {
                sources
                    .layout
                    .is_own(locator::uri_of(graph, *definition.uri_id()).unwrap_or_default())
            })
        {
            return Rendered::Unknown;
        }
        let Some(name) = graph
            .declarations()
            .get(class)
            .map(|declared| declared.name().to_owned())
        else {
            return Rendered::Unknown;
        };
        if rails::partial_of(&name).as_deref() == Some(logical) {
            here.push(*class);
        }
    }
    match Typed::over(here, rendered.derivation.clone()) {
        Some(typed) => Rendered::Here(Box::new(typed), collection),
        None => Rendered::Elsewhere,
    }
}

/// What `each` hands its block on the collection written at `span` of `read`: one element.
fn element_of(
    sources: &Sources<'_>,
    document: UriId,
    values: &HashMap<(u32, u32), Receiver>,
    read: &Document,
    span: (u32, u32),
) -> Option<Typed> {
    let typed = yielded_element(sources, document, values.get(&span)?, read)?;
    (typed.derivation.tier() != Tier::Guessed).then_some(typed)
}

/// What `each` hands its block on `shape`, a value written in `read`.
fn yielded_element(
    sources: &Sources<'_>,
    document: UriId,
    shape: &Receiver,
    read: &Document,
) -> Option<Typed> {
    let each = Receiver::Yielded {
        on: Box::new(shape.rebased(&read.rebase)?),
        method: "each".to_owned(),
        index: 0,
        safe: false,
        arity: Arity::Exactly(0),
        arguments: Vec::new(),
        keywords: None,
        spreads: false,
        default: None,
    };
    let place = match shape {
        Receiver::Variable(at) => *at,
        _ => 0,
    };
    let scope = sources.scope_at(document, read.rebase.to_graph(place)?);
    method_receiver(sources, document, &each, &scope)
}

/// [`template_renderers`] for one template, unmemoized. A `layout` counts only a call that writes
/// its name ([`rails::Render::writes_the_name`]).
fn read_renderers(
    sources: &Sources<'_>,
    cursor: &str,
    logical: &str,
    partial: bool,
    layout: bool,
) -> Option<Rc<[DeclarationId]>> {
    let graph = sources.graph;
    let fence = environment::Fence::at(Some(cursor), sources.layout);
    let mut documents: Vec<UriId> = rails::RENDER_CALLS
        .iter()
        .flat_map(|name| {
            graph
                .calls_named(name)
                .iter()
                .map(|(document, _, _)| *document)
                .collect::<Vec<_>>()
        })
        .collect();
    documents.sort_unstable();
    documents.dedup();
    let mut renderers: Vec<DeclarationId> = Vec::new();
    for document in documents {
        let Some(found) = graph.documents().get(&document) else {
            continue;
        };
        let uri = found.uri();
        let Some(path) = DocUri::from_graph_uri(uri).and_then(|uri| uri.to_file_path()) else {
            continue;
        };
        let template = views::is_view(&path);
        // Any Ruby: a rake task renders with `ApplicationController.render` as a class does.
        if !(template || writes_ruby(uri)) || !application(sources, &fence, uri) {
            continue;
        }
        // A view and a helper run in a view context, where a string names a partial.
        let view = template || rails::is_helper(&path);
        let read = || -> Option<HeldRenders> {
            let (source, rebase) = (sources.read)(uri)?;
            Some(
                rails::read_renders(&source, view)
                    .into_iter()
                    .map(|render| (rebase.to_graph(render.at), render))
                    .collect(),
            )
        };
        let calls = sources
            .held_exits
            .renders(document, found.content_hash(), read)?;
        for (at, render) in calls.iter() {
            if !render.names(logical, partial) || (layout && !render.writes_the_name()) {
                continue;
            }
            if render.elsewhere {
                return None;
            }
            // A component's or a service's own `render` renders something of its own.
            if !rails::renders_its_own_variables(&path) {
                continue;
            }
            if view {
                // The object a view runs on is its renderer's; a helper's, a partial's and a
                // layout's is whichever view called it.
                let rendered = template
                    .then(|| sources.views.rendered_by(graph, &path))
                    .flatten()
                    .filter(|_| !rails::template_of(&path).is_some_and(|(_, part)| part));
                match rendered {
                    Some(rendered) => renderers.push(rendered.declaration),
                    None => renderers.extend(every_renderer(sources).iter()),
                }
                continue;
            }
            let owner = sources
                .scope_at(document, (*at)?)
                .nesting_id(graph)
                .and_then(|id| base_of(graph, id))?;
            renderers.push(owner);
        }
    }
    renderers.sort_unstable();
    renderers.dedup();
    Some(Rc::from(renderers))
}

/// What the document filed under `uri_id` can call without a receiver ([`views::Views::reachable`]),
/// with a layout's renderers found here ([`layout_renderers`]).
///
/// The one door to the view context, so a card, a margin and a list ask one question.
#[must_use]
pub fn view_context(sources: &Sources<'_>, uri_id: UriId) -> Option<views::Reachable> {
    sources.views.reachable(sources.graph, uri_id, &|logical| {
        layout_renderers(sources, logical)
    })
}

/// Every class whose views the layout template `logical` wraps, of those a view can run on
/// ([`every_renderer`]), in its order.
///
/// Each renderer's layouts are Rails' own lookup ([`views::Views::layouts_of`]), found once per graph
/// with the renderers ([`Indexed::layouts`]): the nearest `layout` its ancestors wrote, else the
/// first class whose own name finds a layout template. One whose `layout` is a method only running
/// Ruby answers may be rendered in any.
fn layout_renderers(sources: &Sources<'_>, logical: &str) -> Vec<DeclarationId> {
    sources
        .graph
        .layouts(|| find_layouts(sources))
        .iter()
        .filter(|(_, layouts)| layouts.includes(logical))
        .map(|(renderer, _)| *renderer)
        .collect()
}

/// [`layout_renderers`]' table, found: each renderer and the layouts its views are rendered in.
///
/// A layout template exists where any document the graph holds is one by that name, a gem's
/// included: Rails looks a layout up in every view path.
fn find_layouts(sources: &Sources<'_>) -> Rc<[(DeclarationId, rails::Layouts)]> {
    let graph = sources.graph;
    let templates: HashSet<String> = graph
        .documents()
        .values()
        .filter_map(|document| {
            let path = DocUri::from_graph_uri(document.uri())?.to_file_path()?;
            let (logical, partial) = rails::template_of(&path)?;
            (erb::is_template(&path) && !partial).then_some(logical)
        })
        .collect();
    let exists = |logical: &str| templates.contains(logical);
    every_renderer(sources)
        .iter()
        .map(|renderer| {
            (
                *renderer,
                sources.views.layouts_of(graph, *renderer, &exists),
            )
        })
        .collect()
}

/// Every class a view can run on: each view's controller or mailer by the convention, and each
/// class that renders by a call of its own. The renderers a view could have been
/// rendered by when nothing narrower can be said. Held with the graph ([`Indexed::renderers`]).
///
/// **The calls count too**, or the answer is narrower than the truth: a `StoriesController`
/// has no `stories/` view of its own and renders `articles/index` by name, so a helper or a
/// partial reading `@home_page`, which only it sets, found no write.
fn every_renderer(sources: &Sources<'_>) -> Rc<[DeclarationId]> {
    sources.graph.renderers(|| find_every_renderer(sources))
}

/// [`every_renderer`], found.
fn find_every_renderer(sources: &Sources<'_>) -> Rc<[DeclarationId]> {
    let graph = sources.graph;
    let mut renderers: Vec<DeclarationId> = graph
        .documents()
        .values()
        .filter_map(|document| {
            let path = DocUri::from_graph_uri(document.uri())?.to_file_path()?;
            // A partial's own path renders nothing: views name it. Only an ERB view's class: this
            // is what an ERB partial, a helper or a layout may run on, and a JSON view renders none
            // of them; a jbuilder partial's renderers come from its calls.
            let partial = rails::template_of(&path).is_none_or(|(_, partial)| partial);
            if partial || !erb::is_template(&path) || !sources.layout.is_own(document.uri()) {
                return None;
            }
            sources
                .views
                .rendered_by(graph, &path)
                .map(|rendered| rendered.declaration)
        })
        .collect();
    // Only where the convention is on: with `[rails] views` off no view has a renderer.
    if sources.views.on() {
        renderers.extend(calling_renderers(sources));
    }
    renderers.sort_unstable();
    renderers.dedup();
    Rc::from(renderers)
}

/// Every application class that renders with a call of its own, on its own object ([`every_renderer`]):
/// the class at each render call in a controller or a mailer, as [`read_renderers`] places one. A
/// call with a receiver written renders with another object, and does not count.
fn calling_renderers(sources: &Sources<'_>) -> Vec<DeclarationId> {
    let graph = sources.graph;
    let mut documents: Vec<UriId> = rails::RENDER_CALLS
        .iter()
        .flat_map(|name| {
            graph
                .calls_named(name)
                .iter()
                .map(|(document, _, _)| *document)
                .collect::<Vec<_>>()
        })
        .collect();
    documents.sort_unstable();
    documents.dedup();
    let mut renderers = Vec::new();
    for document in documents {
        let Some(found) = graph.documents().get(&document) else {
            continue;
        };
        let uri = found.uri();
        let Some(path) = DocUri::from_graph_uri(uri).and_then(|uri| uri.to_file_path()) else {
            continue;
        };
        if views::is_view(&path)
            || rails::is_helper(&path)
            || !writes_ruby(uri)
            || !sources.layout.is_own(uri)
            || !rails::renders_its_own_variables(&path)
        {
            continue;
        }
        let read = || -> Option<HeldRenders> {
            let (source, rebase) = (sources.read)(uri)?;
            Some(
                rails::read_renders(&source, false)
                    .into_iter()
                    .map(|render| (rebase.to_graph(render.at), render))
                    .collect(),
            )
        };
        let Some(calls) = sources
            .held_exits
            .renders(document, found.content_hash(), read)
        else {
            continue;
        };
        for (at, render) in calls.iter() {
            if render.elsewhere {
                continue;
            }
            if let Some(owner) =
                at.and_then(|at| base_of(graph, sources.scope_at(document, at).nesting_id(graph)?))
            {
                renderers.push(owner);
            }
        }
    }
    renderers
}

/// What one `def initialize` does with one instance variable before any other method can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Initializes {
    /// Writes it as a statement of its own body.
    Writes,
    /// Does not, but runs the `initialize` above it (`super` as a statement).
    Supers,
    /// Neither, or it cannot be read: `nil` can be there.
    Neither,
}

/// Whether no object the read can be on holds `nil` before some method has run: every class it
/// can be an instance of runs an `initialize` that writes the variable.
///
/// A class object has no `initialize` of its own, and neither does an object a module was
/// `extend`ed onto; both keep `nil`. So does a module no class includes, where there is no
/// instance to argue about.
fn initialized(sources: &Sources<'_>, hierarchy: &Hierarchy, name: &str) -> bool {
    if hierarchy.class_side {
        return false;
    }
    let graph = sources.graph;
    let mut classes = 0;
    for object in &hierarchy.objects {
        match graph.declarations().get(object) {
            Some(Declaration::Namespace(Namespace::Class(_))) => {}
            Some(Declaration::Namespace(Namespace::Module(_))) => continue,
            _ => return false,
        }
        classes += 1;
        if built_by_a_library(sources, *object) || !initializes(sources, *object, name) {
            return false;
        }
    }
    classes > 0
}

/// Whether the read runs in an action every one of whose objects' callbacks writes the variable
/// first: rule 5's other half, where a controller's `before_action` is `initialize`.
///
/// - **The read is an action's**: an instance's variable, read in a public receiverless `def`
///   (blocks included) of every class the object can be, that nothing in the application's Ruby of
///   those classes calls by name ([`action_called_by_name`]): an action runs once, dispatched.
/// - **On every class the object can be**, one of the methods [`views::Views::runs_before`] says
///   surely run before that action, looked up from the class as a self-call is, writes the
///   variable as a statement of its own body before any `return` ([`opens_with`]).
/// - A module among the objects (a concern) is no object; a class object never runs an action.
fn written_before(
    sources: &Sources<'_>,
    uri_id: UriId,
    instance: &cursor::InstanceRead,
    hierarchies: &[Rc<Hierarchy>],
) -> bool {
    let graph = sources.graph;
    let (Some(0), Some(action)) = (instance.level, instance.method.as_deref()) else {
        return false;
    };
    let member = StringId::from(format!("{action}()").as_str());
    let mut classes = 0;
    // Level 0 is an instance's variable, so no hierarchy here is a class object's.
    for hierarchy in hierarchies {
        for object in &hierarchy.objects {
            match graph.declarations().get(object) {
                Some(Declaration::Namespace(Namespace::Class(_))) => {}
                Some(Declaration::Namespace(Namespace::Module(_))) => continue,
                _ => return false,
            }
            classes += 1;
            let public = member_of(sources, uri_id, *object, member)
                .is_some_and(|found| graph.visibility(&found) == Some(Visibility::Public));
            let written = public
                && sources
                    .views
                    .runs_before(graph, *object, action)
                    .iter()
                    .any(|method| {
                        member_of(
                            sources,
                            uri_id,
                            *object,
                            StringId::from(format!("{method}()").as_str()),
                        )
                        .is_some_and(|found| opens_with(sources, found, &instance.name))
                    });
            if !written {
                return false;
            }
        }
    }
    classes > 0 && !action_called_by_name(sources, uri_id, hierarchies, action)
}

/// Whether the method `method` writes `name` as a statement of its own body before any `return`
/// ([`cursor::Variables::opening`]). One definition, in Ruby, read: a signature or two `def`s say
/// nothing about which body runs.
fn opens_with(sources: &Sources<'_>, method: DeclarationId, name: &str) -> bool {
    read_opening(sources, method).is_some_and(|writes| writes.iter().any(|written| written == name))
}

/// [`opens_with`]'s writes, or `None` where the method is not one Ruby `def` that can be read.
fn read_opening(sources: &Sources<'_>, method: DeclarationId) -> Option<Vec<String>> {
    let reads = &sources.memo.reads;
    let graph = sources.graph;
    let definitions = locator::definitions_of(graph, method);
    let [definition] = definitions.as_slice() else {
        return None;
    };
    let uri = graph.documents().get(definition.uri_id())?.uri().to_owned();
    writes_ruby(&uri).then_some(())?;
    let document = reads.documents.of(&uri, sources.read)?;
    let span = document.rebase.span_to_buffer(ByteSpan {
        start: definition.offset().start(),
        end: definition.offset().end(),
    })?;
    Some(
        document
            .shapes(&uri, sources.held_exits)
            .variables
            .opening(span.start)?
            .to_vec(),
    )
}

/// Whether the application's Ruby of the objects' classes calls `action` by name on `self`: a
/// receiverless call, `self.action`, or a `send` that may name it. An action called that way also
/// runs inside another, whose callbacks may not be its own.
fn action_called_by_name(
    sources: &Sources<'_>,
    uri_id: UriId,
    hierarchies: &[Rc<Hierarchy>],
    action: &str,
) -> bool {
    read_action_calls(sources, uri_id, hierarchies, action).is_none_or(|called| called)
}

/// [`action_called_by_name`], or `None` where a document that may call it cannot be read.
fn read_action_calls(
    sources: &Sources<'_>,
    uri_id: UriId,
    hierarchies: &[Rc<Hierarchy>],
    action: &str,
) -> Option<bool> {
    let graph = sources.graph;
    let reads = &sources.memo.reads;
    let cursor = graph.documents().get(&uri_id)?.uri().to_owned();
    let fence = environment::Fence::at(Some(&cursor), sources.layout);
    // The application's documents of the objects' classes, by id: a library calling a name like
    // `index` is not the application dispatching its action.
    let own: HashMap<UriId, &str> = hierarchies
        .iter()
        .flat_map(|hierarchy| hierarchy.documents.iter())
        .filter(|(uri, _)| application(sources, &fence, uri))
        .map(|(uri, document)| (*document, uri.as_str()))
        .collect();
    for (document, start, _) in graph.calls_named(action).iter() {
        let Some(uri) = own.get(document) else {
            continue;
        };
        let read = reads.documents.of(uri, sources.read)?;
        let at = read.rebase.span_to_buffer(ByteSpan {
            start: *start,
            end: *start,
        })?;
        if on_self(&read.source, at.start as usize) {
            return Some(true);
        }
    }
    let mut senders: Vec<&str> = cursor::SENDERS
        .iter()
        .flat_map(|sender| {
            graph
                .calls_named(sender)
                .iter()
                .copied()
                .collect::<Vec<_>>()
        })
        .filter_map(|(document, _, _)| own.get(&document).copied())
        .collect();
    senders.sort_unstable();
    senders.dedup();
    for uri in senders {
        let read = reads.documents.of(uri, sources.read)?;
        if read
            .shapes(uri, sources.held_exits)
            .sends
            .iter()
            .any(|sent| matches!(sent.on, Receiver::SelfObject(_)) && sent.name.matches(action))
        {
            return Some(true);
        }
    }
    Some(false)
}

/// Whether the call whose name starts at `at` in `source` is on `self`: nothing written before the
/// name but space, or `self.` (`&.` included).
fn on_self(source: &str, at: usize) -> bool {
    let before = source.get(..at).unwrap_or_default().trim_end();
    let Some(receiver) = before
        .strip_suffix("&.")
        .or_else(|| before.strip_suffix('.'))
    else {
        return !before.ends_with("::");
    };
    let receiver = receiver.trim_end();
    receiver.ends_with("self")
        && !receiver[..receiver.len() - 4]
            .chars()
            .next_back()
            .is_some_and(|last| last.is_alphanumeric() || last == '_' || last == '@' || last == ':')
}

/// Whether a library may build instances of `class` without running `initialize`.
///
/// Active Record builds a loaded record with `allocate`, so a model's `initialize` has not run on
/// it. The rule names no library: **a class whose superclass chain holds a class defined in Ruby
/// outside the project** may be built by that Ruby. `Object` and `BasicObject` are every class's
/// roots, reopened by libraries without building anything, and do not count. A module builds
/// nothing, and a class only signatures declare is Ruby's own.
fn built_by_a_library(sources: &Sources<'_>, class: DeclarationId) -> bool {
    let graph = sources.graph;
    let Some(namespace) = linearized(graph, class) else {
        return true;
    };
    namespace.ancestors().iter().any(|ancestor| {
        let Ancestor::Complete(id) = ancestor else {
            return true;
        };
        let Some(declaration @ Declaration::Namespace(Namespace::Class(_))) =
            graph.declarations().get(id)
        else {
            return false;
        };
        if matches!(declaration.name(), "Object" | "BasicObject") {
            return false;
        }
        locator::definitions_of(graph, *id)
            .iter()
            .any(|definition| {
                graph
                    .documents()
                    .get(definition.uri_id())
                    .is_some_and(|document| {
                        writes_ruby(document.uri()) && !sources.layout.is_own(document.uri())
                    })
            })
    })
}

/// Whether a new instance of `class` has written `name` once its `initialize` returns: the first
/// `initialize` up its ancestry writes it, or `super`s to one that does.
fn initializes(sources: &Sources<'_>, class: DeclarationId, name: &str) -> bool {
    let graph = sources.graph;
    let Some(namespace) = graph
        .declarations()
        .get(&class)
        .and_then(Declaration::as_namespace)
    else {
        return false;
    };
    let member = StringId::from("initialize()");
    for ancestor in namespace.ancestors() {
        let Ancestor::Complete(ancestor) = ancestor else {
            return false;
        };
        let Some(method) = graph
            .declarations()
            .get(ancestor)
            .and_then(Declaration::as_namespace)
            .and_then(|found| found.members().get(&member))
        else {
            continue;
        };
        match initializer(sources, *method, name) {
            Initializes::Writes => return true,
            Initializes::Supers => {}
            Initializes::Neither => return false,
        }
    }
    false
}

/// [`Initializes`] for one method, memoized for the request.
///
/// **One definition, in Ruby, read.** A signature, a C method or an `initialize` defined twice says
/// nothing about which body runs, so each is `Neither`.
fn initializer(sources: &Sources<'_>, method: DeclarationId, name: &str) -> Initializes {
    let reads = &sources.memo.reads;
    let key = (method, name.to_owned());
    if let Some(held) = reads.initializers.borrow().get(&key) {
        return *held;
    }
    let answer = read_initializer(sources, method, name).unwrap_or(Initializes::Neither);
    reads.initializers.borrow_mut().insert(key, answer);
    answer
}

fn read_initializer(
    sources: &Sources<'_>,
    method: DeclarationId,
    name: &str,
) -> Option<Initializes> {
    let reads = &sources.memo.reads;
    let graph = sources.graph;
    let definitions = locator::definitions_of(graph, method);
    let [definition] = definitions.as_slice() else {
        return None;
    };
    let uri = graph.documents().get(definition.uri_id())?.uri().to_owned();
    if !writes_ruby(&uri) {
        return None;
    }
    let document = reads.documents.of(&uri, sources.read)?;
    let span = document.rebase.span_to_buffer(ByteSpan {
        start: definition.offset().start(),
        end: definition.offset().end(),
    })?;
    let found = document
        .shapes(&uri, sources.held_exits)
        .variables
        .initializer(span.start)?;
    Some(if found.writes.iter().any(|written| written == name) {
        Initializes::Writes
    } else if found.supers {
        Initializes::Supers
    } else {
        Initializes::Neither
    })
}

/// Every value that can reach one place, as one type, by [`Join`]'s rule. Every assignment any
/// value names is kept, so the card can name every line, and `nil` alone is `NilClass`, an answer:
/// `x = nil` really holds it.
fn fold_reached(graph: &Graph, mut typed: Vec<Typed>, nil: bool) -> Option<Typed> {
    // One value and no `nil`: the answer is that value, untouched.
    if typed.len() == 1 && !nil {
        return typed.pop();
    }
    let folds = Folds::of(graph);
    let mut join = Join::default();
    if nil {
        join.nil();
    }
    for one in typed {
        join.add(one, &folds);
    }
    join.finish(&folds)
}

/// Every document a scope has been asked of, walked once.
///
/// - **Why:** [`body_return`] reads a method's body from whichever document declares it and needs
///   the scope of that `def`. Asking [`Scope::at`] per `def` is the quadratic [`Scope::bodies`]
///   avoids, one rung further down.
/// - **A map, not one walk**, because the rung crosses documents. The `UriId` is the key, so a walk
///   of the wrong document cannot be returned.
#[derive(Default)]
pub struct Walked<'g> {
    walked: RefCell<HashMap<UriId, Rc<Bodies<'g>>>>,
}

impl<'g> Walked<'g> {
    /// One document's bodies, walked here or reused from earlier in this request.
    fn of(&self, graph: &'g Graph, uri_id: UriId) -> Rc<Bodies<'g>> {
        if let Some(found) = self.walked.borrow().get(&uri_id) {
            return Rc::clone(found);
        }
        let walked = Rc::new(Scope::bodies(graph, uri_id));
        self.walked.borrow_mut().insert(uri_id, Rc::clone(&walked));
        walked
    }
}

/// How many `CONST = Klass.new` hops one answer may follow.
///
/// - **A cycle guard, not a limit real code meets.** Real chains are longer than they look:
///   addressable builds `QUERY` from `PCHAR`, `PCHAR` from `UNRESERVED` and `UNRESERVED` from
///   `ALPHA`, four hops to a `String`. At 3 that chain answered nothing; 10 is headroom.
/// - **Bounded because a cycle would crash the process** (`A = B.new` beside `B = A.new`).
const CONSTANT_HOPS: u8 = 10;

/// How many method bodies deep one answer may read: the chain's *depth*.
///
/// 1. **A guard, not a limit real code meets.** A recursion no longer reaches it: [`body_return`]
///    refuses a body asked for inside itself ([`Reads::bodies`]). What is left is a chain of
///    distinct methods, and the deepest the six corpora read is 13; at the old 10 a few
///    were cut. 32 is headroom.
/// 2. **Still needed** for a caller with no memo, whose first body is not on any stack, and for a
///    recursion that passes something new at every level. An unbounded walk would abort the
///    process.
/// 3. **Held to the stack by a test** (`a_chain_of_distinct_bodies_is_read_to_the_bound`), which
///    reads a chain this deep on a test thread's stack.
/// 4. **A constant, not a setting.** A depth is not something a project has an opinion about.
const BODY_HOPS: u8 = 32;

/// A receiver's declaration, and how ya-lsp arrived at it.
///
/// The second half is the point. Three of the five rungs are *derived*: correct only if the RBS is
/// correct and the followed assignment is the one that ran. A user who cannot tell those from what
/// the code states has lost what makes this server different. So the derivation travels with the
/// declaration.
///
/// # Why the class is a list, reached through a method
///
/// - **Almost every answer is one class.** The two exceptions Ruby writes constantly are folded: a
///   class *or* `nil` ([`Self::nilable`]) and `true` or `false` ([`Self::boolean`]). What is left
///   after both folds, such as `String | Integer`, is a real union.
/// - **A union is never taken for one class.** Two readers take it whole, on purpose: a call on it
///   runs on each class that has the member ([`narrowed`]), and a completion lists each class's
///   members, each row marked with its class.
/// - **Enforced by construction.** The classes are private and [`Self::one`] is the only way to a
///   single `DeclarationId`; it answers `None` for a union. Every member lookup already uses `?`,
///   so a union stops every step that needs one class without any site checking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Typed {
    /// Every class the value can be, with `nil` and the boolean pair folded out. **Never empty.**
    /// In the order the answers were found; for a body, the order the exits were read, which is
    /// stable across runs.
    classes: Vec<DeclarationId>,
    pub derivation: Derivation,
    /// `nil` was one of the answers, and was folded out of [`Self::classes`].
    ///
    /// A facet, not a class, so **a label draws the mark and a completion reads the classes without
    /// it**. A completion on a `String?` offers `String`'s members, which is what someone typing a
    /// `.` wants. `types.md` calls this the one inexact entry; the mark makes it visible.
    ///
    /// **A call's answer reads it** ([`returned_by`]): a `.` also asks `NilClass`, and a `&.` adds
    /// `nil` back.
    pub nilable: bool,
    /// The answer is `true` or `false`, and [`Self::classes`] holds `TrueClass` alone.
    ///
    /// Ruby has no `bool` class, so a real class must carry the members. Either half works:
    /// `TrueClass` and `FalseClass` declare the same six names (`!`, `&`, `===`, `^`, `to_s`, `|`),
    /// so a `.` reaches the same list. This is the argument made for a generic's head.
    pub boolean: bool,
    /// What the class was written holding, by position: the `Integer` of `Array[Integer]`.
    ///
    /// - **Invisible to every step.** `Array[Integer]` and `Array[String]` reach the same
    ///   declarations, so steps read [`Self::classes`]. This answers what the head cannot: what a
    ///   block on this value is handed, and what a method declared `-> E` returns
    ///   ([`Return::Parameter`], resolved at [`Self::argument`]).
    /// - **Private, read only through [`Self::argument`].** A position means nothing without the
    ///   list it was counted along (`Enumerable[E]` is an array's element but a hash's whole pair),
    ///   so the reader compares the declaring class first. A bare accessor would allow getting that
    ///   wrong.
    /// - **Empty means "nothing said"**, as for most receivers. It never claims the class has no
    ///   parameters.
    arguments: Vec<Option<DeclarationId>>,
    /// **The body said `self`, so the answer is the receiver's type, not this class.**
    ///
    /// - **The Ruby version of RBS's `self`** ([`Return::Same`]). `def probe; self; end` in
    ///   `class Numeric` would otherwise resolve in the `def`'s scope, so `1.probe` would answer
    ///   `Numeric` where Ruby answers `Integer`. ActiveSupport's `presence` (`self if present?`) is
    ///   the common real case.
    /// - **A facet, with the declaring class kept in [`Self::classes`] as the carrier**, like
    ///   [`Self::boolean`]. Surfaces that ignore the facet keep their answer.
    /// - **The one reader is [`from_body`]**, the only caller with a receiver to substitute. A
    ///   margin drawn on the `def` itself has none, and for it the declaring class is the honest
    ///   answer.
    same: bool,
    /// **What `new` passed to build this one object**: the binding for the `initialize`
    /// it ran.
    ///
    /// - **Read by [`from_body`]**, which reads a body for this object with it
    ///   ([`Sources::made`]), so `Service.new(story).call` sees `@story` hold a `Story`.
    /// - **It travels with the object, not the class.** A local holding it, a `T?` of it, and
    ///   `self` in a body read for it keep it; a fold of two values drops it, and so does every
    ///   new answer.
    /// - **Invisible to every step and every label**, like [`Self::arguments`]: members come from
    ///   [`Self::classes`].
    made: Option<Made>,
    /// **Which proc or lambda literals this `Proc` can be**: each is a document and where
    /// the literal starts ([`cursor::Receiver::Proc`]).
    ///
    /// - **Every value must be one**: a fold keeps the union of its values' literals only while
    ///   each value adds one (`nil` aside), and drops them at the first `Proc` from anywhere else.
    /// - **Read by a call of the proc and a block it is passed as** ([`proc_value`]).
    procs: Option<Rc<[(UriId, u32)]>>,
    /// **Which method this `Method` is bound to** ([`Bound`]): `widget.method(:shout)` is
    /// `Widget#shout` on that widget.
    ///
    /// - **Every value must be the same one**: a fold keeps it only where each value adding a class
    ///   is bound to the same method on the same class (`nil` aside).
    /// - **Read by a call of it and a block it is passed as** ([`called_method`], [`bound_return`]),
    ///   and drawn as `Method[Widget#shout]` (`render::typed`).
    bound: Option<Rc<Bound>>,
}

/// The method a `Method` is bound to ([`Typed::bound`]): what `method(:shout)` looked up, on the
/// object it was sent to.
#[derive(Debug, PartialEq, Eq)]
pub struct Bound {
    /// The object the method runs on.
    on: Typed,
    /// The method, as the lookup found it.
    method: DeclarationId,
    /// Its name as written, which a call of the `Method` makes.
    name: String,
    /// Whether the lookup reached a private method, as `method` does and `public_method` does
    /// not: a call of the `Method` runs it either way.
    private: bool,
}

/// A [`Binding`] of `initialize`, for [`Typed::made`]: compared by its key, like the memo.
#[derive(Debug, Clone)]
pub struct Made(Rc<Binding>);

impl PartialEq for Made {
    fn eq(&self, other: &Self) -> bool {
        self.0.key == other.0.key
    }
}

impl Eq for Made {}

impl Typed {
    /// One class and no facets: what most rungs produce.
    pub fn of(declaration: DeclarationId, derivation: Derivation) -> Self {
        Self {
            classes: vec![declaration],
            derivation,
            nilable: false,
            boolean: false,
            arguments: Vec::new(),
            same: false,
            made: None,
            procs: None,
            bound: None,
        }
    }

    /// The same class, carrying what it was written holding.
    ///
    /// A builder, not a third constructor: the facets already work this way, and the arguments
    /// resolve one step after the class. The class comes from the signature; the arguments from the
    /// signature *and* the receiver it was read against (see [`held_by_return`]).
    #[must_use]
    fn holding(mut self, arguments: Vec<Option<DeclarationId>>) -> Self {
        self.arguments = arguments;
        self
    }

    /// The receiver's own type argument at one position, where the position was counted along this
    /// receiver's own class.
    ///
    /// - **`of` is the whole safety argument.** `Array` includes `Enumerable[E]` and `Hash`
    ///   includes `Enumerable[[K, V]]`, so one `Parameter { at: 0, of: "Enumerable" }` is an
    ///   array's element but a hash's whole pair.
    /// - **So a member reached through an ancestor answers nothing.** That loses the names Ruby
    ///   reaches through a module, but `Array` and `Hash` alias most of them onto the class itself.
    /// - **The comparison is a hash and touches no graph.** [`DeclarationId`] is built from the
    ///   name, as [`declared`] does before it checks the graph.
    #[must_use]
    fn argument(&self, of: &str, at: usize) -> Option<DeclarationId> {
        (self.one()? == DeclarationId::from(of))
            .then(|| *self.arguments.get(at)?)
            .flatten()
    }

    /// Several classes, after `nil` and the boolean pair are folded out.
    ///
    /// One class goes through here too, so a caller that folded down to one answer need not notice.
    /// [`Self::one`] reads the length, not a flag, so there is only one spelling of "one class".
    fn over(classes: Vec<DeclarationId>, derivation: Derivation) -> Option<Self> {
        (!classes.is_empty()).then_some(Self {
            classes,
            derivation,
            nilable: false,
            boolean: false,
            arguments: Vec::new(),
            same: false,
            made: None,
            procs: None,
            bound: None,
        })
    }

    /// The one class, or `None` where the answer is a union.
    ///
    /// **Every rung that needs a class goes through here**, which makes a union terminal without
    /// any site testing for one. See the type's docs.
    #[must_use]
    pub fn one(&self) -> Option<DeclarationId> {
        match self.classes.as_slice() {
            [only] => Some(*only),
            _ => None,
        }
    }

    /// What the class was written holding, by position. Empty except for a generic.
    ///
    /// **Public for one reader, [`render::typed`](super::render::typed)**, the one place a type is
    /// spelled for a person. Everyone else asks the safe question, [`Self::argument`], which names
    /// a position against the class that declared it. This is the unsafe one, so read it beside the
    /// head or not at all.
    #[must_use]
    pub fn arguments(&self) -> &[Option<DeclarationId>] {
        &self.arguments
    }

    /// Every class, for the readers that take a union whole: a label, and a completion's list.
    #[must_use]
    pub fn classes(&self) -> &[DeclarationId] {
        &self.classes
    }

    /// The method this `Method` is bound to, where it is one ([`Typed::bound`]): what a label
    /// draws inside the brackets.
    #[must_use]
    pub fn bound_method(&self) -> Option<DeclarationId> {
        self.bound.as_ref().map(|bound| bound.method)
    }

    /// The same value with `nil` folded in, for a rung that read one.
    #[must_use]
    fn or_nil(mut self) -> Self {
        self.nilable = true;
        self
    }

    /// A **signature's** declared return, as a type a margin can draw.
    ///
    /// About the method, not a receiver. Three refusals: [`Return::Same`] is the receiver (at a
    /// `def`, the class the label sits in), and the two query-interface sentinels are names no file
    /// declares (see [`ELEMENT`]).
    #[must_use]
    pub fn declared_as(graph: &Graph, returns: &Returned) -> Option<Self> {
        let declaration = match &returns.of {
            // The head alone. What the class holds is a question about a *value*, and a `def`'s
            // label has no value yet. Every other reader of this table reads the arguments at a
            // call.
            Return::Class { name, scope, .. } => declared_in(graph, name, scope)?,
            Return::Bool => declared(graph, "TrueClass")?,
            // A `def`'s label has no receiver and no call, so it can no more name a receiver's type
            // argument than `self`, or what an unwritten block would return.
            Return::Same
            | Return::Element
            | Return::Collection
            | Return::Parameter { .. }
            | Return::Block
            | Return::Argument { .. }
            | Return::Written
            | Return::Shared
            | Return::Held
            | Return::Scoped
            | Return::Forwarded { .. } => {
                return None;
            }
            // Each member as its own label would be, joined.
            Return::Union(members) => {
                let folds = Folds::of(graph);
                let mut join = Join::default();
                for member in members {
                    let mut typed = Self::declared_as(graph, &Returned::plain(member.clone()))?;
                    typed.arguments.clear();
                    join.add(typed, &folds);
                }
                let mut joined = join.finish(&folds)?;
                joined.nilable |= returns.nilable;
                return Some(joined);
            }
        };
        Some(Self::of(declaration, Derivation::default()).faceted(returns))
    }

    /// The same class, carrying the facets a signature declared.
    ///
    /// `|=` on the mark, not `=`: a chain through a `String?` stays nilable even when the method it
    /// reached declares a plain return.
    #[must_use]
    fn faceted(mut self, returns: &Returned) -> Self {
        self.nilable |= returns.nilable;
        self.boolean = matches!(returns.of, Return::Bool);
        self
    }
}

/// What was followed to get a type, in the order it was followed.
///
/// Empty means nothing was: the receiver named its own type (a constant, a literal, `self`,
/// `Foo.new`, or a local assigned one of those).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Derivation {
    /// The signatures a chain was read through, in walk order:
    /// `["String#upcase()", "String#strip()"]`.
    pub signatures: Vec<String>,
    /// The offsets of the instance-variable assignments the type came from: every write that can
    /// reach the read, in the request's document. Empty where none did.
    pub assignments: Vec<u32>,
    /// The constant whose *declared* type answered, when the receiver was a constant holding an
    /// object.
    ///
    /// - **Not a row in [`Self::signatures`]**: that list renders as "from what those **methods**
    ///   declare", and this is not a method. Same strength of evidence and same tier.
    /// - **The constant's name, not its class.** The class is already the card's answer. The reader
    ///   needs to know that `ENV` being an `ENVClass` comes from a signature, not from this
    ///   expression.
    pub constant: Option<String>,
    /// The constant whose **Ruby assignment** answered, and where that line is.
    ///
    /// The sibling of [`Self::constant`], kept separate because the evidence differs. A signature
    /// is a type somebody wrote down; `CONFIG = Settings.new` is Ruby that will have run. Same
    /// tier, different sentence, so the reader knows which one to check.
    pub assigned_constant: Option<FromAssignment>,
    /// The `def` whose **body** answered, and where it is written.
    ///
    /// The sibling of [`Self::signatures`] where nothing declared a return, as
    /// [`Self::assigned_constant`] is to [`Self::constant`]. A signature is a claim; a body is a
    /// fact about one branch of one method. Same tier, different sentence.
    pub body: Option<FromBody>,
    /// The class a *template's* instance variable was typed from, when the view↔renderer convention
    /// answered: the controller its path names, or the mailer when there is no controller.
    pub renderer: Option<FromRenderer>,
    /// Every other class whose file assigns an instance variable this one reads: an ancestor, a
    /// descendant, or another file of the same class ([`instance_read`]).
    ///
    /// A sibling of [`Self::renderer`], kept separate because the evidence differs. A renderer is a
    /// **path convention**: nothing in either file says `app/views/stories/` means
    /// `StoriesController`. These are the code's own `<` and `include`, linearized by rubydex.
    /// Same tier, different sentence.
    ///
    /// **One per class and document**: every write reaches the read, so a fold across two files
    /// names both.
    pub classes: Vec<FromClass>,
    /// The declaration `super` reached, when the answer came from the method this one overrides.
    ///
    /// **A name, with no file or line**, the only evidence named this way. A `super` answer always
    /// comes through a body, and [`Self::body`] already carries that place. What the reader cannot
    /// see is which ancestor the keyword climbed to. A name is what they would search for next, and
    /// turning it into a line would cost a parse.
    pub superclass: Option<String>,
    /// The receiver's own spelling, when only it was left and the last rung answered.
    ///
    /// The name, not the class: the class is the card's answer. The reader needs to know that
    /// `@user` being a `User` was inferred from six letters, not stated by the code.
    pub guess: Option<String>,
    /// The macro that made a `:symbol` a name, when the cursor was on one.
    ///
    /// A convention, stated like the ones above: nothing in `:normalise` says it is a method; only
    /// the word to its left does. The card names that word, so a reader who thinks `before_save`
    /// does something else can see what the answer rests on.
    pub named_by: Option<String>,
    /// The class whose **instance** side answered a bare name inside a block in its body, when that
    /// rung answered.
    ///
    /// - **The one place `self` may not be what the file says.** A block is a value, and its
    ///   receiver may run it against anything: `rule(:colon) { str(':') }`,
    ///   `scope :recent, -> { where(...) }`, `validates :x, if: -> { active? }`.
    /// - **So the answer is derived, and the card says from what:** the member is missing from the
    ///   class object and present on an instance. Evidence, not proof.
    /// - **The class, not the member**, for [`Self::guess`]'s reason: the member is the card's
    ///   answer.
    pub closure: Option<String>,
    /// How a **bare** name in a template was reached, when the view-context rung answered it.
    ///
    /// The one field not about a receiver's type. It is here because it is the same kind of fact:
    /// nothing in the template says `app/helpers` is in scope or which class renders it, so the
    /// answer is *derived* and the card names the convention.
    pub view: Option<views::InView>,
    /// The conversion whose class Ruby checks, when that rule answered a call ([`converted`]).
    ///
    /// A rule of the language, not of this code: nothing here says what `params[:id]` is, but its
    /// `to_s` is a `String` or Ruby raises where it converts with it. The method, not the class,
    /// for [`Self::guess`]'s reason.
    pub conversion: Option<&'static str>,
    /// The `def` whose Ruby `yield`s answered what a block is handed, where no signature says
    /// ([`yielded_from_body`]). [`Self::body`]'s sibling, from the block's side.
    pub yielded: Option<FromBody>,
    /// The call whose block this `self` was read in, where a generator names the class that call
    /// makes for it ([`Synthesized::ran`](super::synthesized::Synthesized::ran)).
    ///
    /// A convention of whoever makes the class, stated like [`Self::closure`]: nothing in the text
    /// names it, so the card says which call's block the answer rests on. The method, not the
    /// class, for [`Self::guess`]'s reason.
    pub ran: Option<String>,
    /// The module whose including classes this `self` is, every one of them
    /// ([`Runs::Each`](crate::generated::Runs::Each)): a block the module hands each
    /// class that includes it. A convention stated like [`Self::ran`].
    pub each: Option<String>,
    /// The method a call made by name answered for ([`sent`], [`called_method`]):
    /// `widget.send(:shout)` and `formatter.call(2)` are `shout`'s answer, and the card says so,
    /// since every other line on it is about `shout`.
    ///
    /// Ruby's own rule, like `new`: it names which call answered, and adds no doubt to the tier.
    pub sent: Option<Sent>,
}

/// How a call reached the method that answered it by name ([`Derivation::sent`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// Named by the call's first argument: `send(:shout)`, `try(:shout)`.
    Named(String),
    /// A call of a `Method` bound to it: `method(:shout).call`.
    Bound(String),
}

/// Which of the three kinds of answer a derivation is.
///
/// - **The tier describes how a type was reached**, so it lives beside the derivation, not in
///   whichever module draws it.
/// - **Every consumer that can shows the bottom tier**: a hover card's *Guessed from name alone.*
///   line, a completion row's detail. The two tiers above it read alike to a reader
///   (decided 2026-09-29): both are checked by going where the jump goes.
/// - **An inlay hint refuses the bottom tier.** It is drawn unasked and has no room to say so.
///   The refusal must test the tier, not a list of shapes, or the next rung added below the graph
///   is drawn in every margin by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The code names the type. Nothing was followed and there is nothing to doubt.
    Resolved,
    /// A signature, an assignment or a convention was followed. Correct if that is.
    Derived,
    /// Matched on a name alone: the one answer ya-lsp gives that is allowed to be wrong.
    Guessed,
}

impl Derivation {
    /// Takes in what an answer read in **another document** rests on: a method's body
    /// ([`read_body`]), or the bodies disputing a signature ([`disputed`]).
    ///
    /// - **Destructured, not read field by field**, so a new field cannot be silently dropped on
    ///   the way out of the other document.
    /// - **`body` is set, not `or`ed**: the outermost body is the one a reader opens, and the note
    ///   is why the reader sees what came out of it.
    /// - **Its assignments are not taken.** They are offsets in *that* document, while every
    ///   reader draws them as lines of the request's (`types.md`).
    fn absorb(&mut self, other: Derivation) {
        let Derivation {
            signatures,
            assignments: _,
            constant,
            assigned_constant,
            body,
            renderer,
            classes,
            superclass,
            guess,
            named_by,
            closure,
            view,
            conversion,
            yielded,
            ran,
            each,
            sent,
        } = other;
        self.signatures.extend(signatures);
        self.constant = self.constant.take().or(constant);
        self.assigned_constant = self.assigned_constant.take().or(assigned_constant);
        self.body = body;
        self.renderer = self.renderer.take().or(renderer);
        if self.classes.is_empty() {
            self.classes = classes;
        }
        self.superclass = self.superclass.take().or(superclass);
        self.guess = self.guess.take().or(guess);
        self.named_by = self.named_by.take().or(named_by);
        self.closure = self.closure.take().or(closure);
        self.view = self.view.take().or(view);
        self.conversion = self.conversion.take().or(conversion);
        self.yielded = self.yielded.take().or(yielded);
        self.ran = self.ran.take().or(ran);
        self.each = self.each.take().or(each);
        self.sent = self.sent.take().or(sent);
    }

    /// Which tier this answer is, read from what was followed to reach it.
    ///
    /// Destructured, not matched field by field, so a new kind of provenance cannot be added
    /// without choosing its tier. That matters: one consumer refuses to draw `Tier::Guessed` at
    /// all.
    #[must_use]
    pub fn tier(&self) -> Tier {
        let Self {
            signatures,
            assignments,
            constant,
            assigned_constant,
            body,
            renderer,
            classes,
            superclass,
            guess,
            named_by,
            closure,
            view,
            conversion,
            yielded,
            ran,
            each,
            // Which call a sender made, not what the answer rests on.
            sent: _,
        } = self;
        // The name rung beats everything. A chain through three signatures that *ended* at a guess
        // is a guess: the weakest rung is what the answer rests on.
        if guess.is_some() {
            return Tier::Guessed;
        }
        if signatures.is_empty()
            && assignments.is_empty()
            && constant.is_none()
            && assigned_constant.is_none()
            && body.is_none()
            && renderer.is_none()
            && classes.is_empty()
            && superclass.is_none()
            && named_by.is_none()
            && closure.is_none()
            && view.is_none()
            && conversion.is_none()
            && yielded.is_none()
            && ran.is_none()
            && each.is_none()
        {
            return Tier::Resolved;
        }
        Tier::Derived
    }
}

/// Where a template's instance variable came from: the class Rails' path convention names, and the
/// line of its assignment **in that file**.
///
/// A line, not an offset (the other provenance fields do the reverse). An offset needs the text it
/// indexes, and the caller drawing the card has only the *template's* text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromRenderer {
    pub renderer: String,
    /// Whether a controller answered, rather than a mailer ([`views::RenderedBy`]).
    ///
    /// The derivation records the convention, not only the class: `UserMailer` is not a
    /// controller. Same rung and same tier either way.
    pub controller: bool,
    /// Every assignment's line: each one reaches the template.
    pub lines: Vec<u32>,
}

/// Where an instance variable was assigned in another file: the class whose method writes it, the
/// file, and the lines.
///
/// - **A line, not an offset**, for [`FromRenderer`]'s reason: the caller has only the text the
///   *cursor* is in.
/// - **The file is carried here, unlike `FromRenderer`.** A renderer's name is enough to find
///   `StoriesController`. The class is usually a concern the reader has never opened (one
///   application sets `@user` in `Authenticatable`), so the file is what makes "go and look" possible.
///   [`FromAssignment`] makes the same split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromClass {
    pub class: String,
    pub file: String,
    /// Every assignment's line in that file.
    pub lines: Vec<u32>,
}

/// Where a constant is given the object it holds: the constant, the file, and the line.
///
/// - **A line, not an offset**, for [`FromRenderer`]'s reason: the caller has only the text the
///   *cursor* is in.
/// - **The file is carried too.** The reader just hovered the constant, so its name is not news;
///   the file is. A constant assigned in an initializer is exactly where "go and look" is the
///   point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromAssignment {
    pub constant: String,
    pub file: String,
    pub line: u32,
}

/// Which `def` was read, and where it is written.
///
/// - **The method's name as rubydex spells it** (`Order#customer()`), because that is what a reader
///   opens.
/// - **The `def`'s own line, not the exit's.** A method has several exits, and one of them is no
///   place to send anyone.
/// - **The file**, for [`FromAssignment`]'s reason: the body is never the code the cursor is in,
///   and for a gem it is not even in the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromBody {
    pub method: String,
    pub file: String,
    pub line: u32,
}

/// The declaration whose members can be written after a `.` on `receiver`.
///
/// The one place a [`Receiver`] becomes a graph declaration, shared by completion and navigation so
/// the two cannot disagree about `person.`. `scope` is where the cursor is (the nesting a guessed
/// constant is resolved in), the only input from the enclosing code.
///
/// - **A `self` is placed by its own offset, not by `scope`.** The receiver is not always written
///   where it is read. See [`Receiver::SelfObject`] and the arm below.
/// - **`receiver` must be in `uri_id`'s graph coordinates.** A `Receiver` is parsed from a
///   *buffer*, and its offsets are graph keys; the two agree only until the next keystroke.
///   [`Receiver::rebased`] translates, or refuses. Skipping it makes a card lose the type on any
///   keystroke and fall back to a guess.
/// - **`None` means nothing exact can be said.** Every caller degrades on it.
#[must_use]
pub fn method_receiver(
    sources: &Sources<'_>,
    uri_id: UriId,
    receiver: &Receiver,
    scope: &Scope,
) -> Option<Typed> {
    let graph = sources.graph;
    let plain = |declaration| Some(Typed::of(declaration, Derivation::default()));
    match receiver {
        // `Foo.bar` calls a *singleton* method, so the receiver is `Foo`'s singleton class. A
        // constant that is not a namespace (`MAX.times`) has a type we cannot name here.
        //
        // 1. **A signature is asked first.** `ENV: RBS::Unnamed::ENVClass` states the type as
        //    plainly as `-> String`. It is asked before the singleton too: a typed constant holds
        //    an object, and the singleton of a namespace rubydex invented for it is wrong. No
        //    constant in `vendor/rbs` shares a name with a class or module.
        // 2. **A `Todo` is refused.** rubydex promotes a constant used as a receiver to
        //    `Namespace::Todo`, so `ENV` and `URI::RFC2396_PARSER` get a singleton class whose
        //    ancestors are `Class`, `Module` and `Object`. `ENV.` would then offer `alias_method`
        //    and `attr_accessor`: precise, wrong, and enough to displace the name-based list. A
        //    real class object has a definition somewhere, and a `Todo` does not. `hierarchy` and
        //    `search` refuse them too.
        // 3. **The Ruby assignment is asked last.** `CONFIG = Settings.new` is the same fact in the
        //    other language. A class object is the type outright, a signature is a stated type, and
        //    an assignment is a line that will have run: the module's usual rung order. Both the
        //    `Todo` arm and a plain `Declaration::Constant` with no singleton reach it.
        //
        // **A constant alias is the namespace it names** (`YAML = Psych`), after the signature and
        // before the singleton, as `Foo.new` on one builds that class ([`locator::alias_target`],
        // chains followed). An alias of something that is no namespace is itself, as before.
        Receiver::Constant(offset) => {
            let constant = constant_at(graph, uri_id, *offset, sources.layout)?;
            if let Some(held) = held_by(sources, constant) {
                return Some(held);
            }
            let constant = locator::alias_target(graph, constant).unwrap_or(constant);
            if !is_todo(graph, constant)
                && let Some(singleton) = singleton_of(graph, constant)
            {
                return plain(singleton);
            }
            assigned_to(sources, constant)
        }
        // **Placed where the `self` was written, not where the cursor is.** They differ in one
        // case: a `self` captured into a variable above a block that rebinds it. rubydex records a
        // `Class.new(base) do … end` body as an anonymous class, and reading at the cursor would
        // answer *that* class, with none of the captured instance's members and no printable name
        // for a reader. See `Receiver::SelfObject`.
        //
        // **In a body read for a known receiver, `self` is that receiver**. Ruby looks
        // a self-call up from the object's own class, so `CsvImporter.new.run` reaches
        // `CsvImporter#parse` from inside `Importer#run`, not the `parse` beside it. Only a `self`
        // whose class the object descends from: one written in another class's body (a constant's
        // assignment, a `Class.new` block) is that class's. Subclasses of the receiver's class are
        // not read; an override there is the code's own inconsistency (`types.md`).
        //
        // **It is the object `new` built, too**, so a self-call in the body carries what
        // built it ([`Sources::made`]).
        //
        // **Asked first: a block a signature says runs against something else** ([`rebound_self`]).
        // `before_save do … end` is written in a class body and runs against a record.
        Receiver::SelfObject(offset) => {
            if let Some(rebound) = rebound_self(sources, uri_id, *offset) {
                return rebound;
            }
            let written = sources.scope_at(uri_id, *offset).caller(graph)?;
            match sources.object {
                Some(object)
                    if object == written
                        || descends(graph, object, written)
                        || sources.extended == Some(written) =>
                {
                    let mut typed = Typed::of(object, Derivation::default());
                    typed.made = sources
                        .made
                        .and_then(|key| {
                            Some(Rc::clone(sources.memo.reads.bindings.borrow().get(&key)?))
                        })
                        .map(Made);
                    Some(typed)
                }
                _ => plain(written),
            }
        }
        // An instance, so the receiver is the class itself rather than its singleton, carrying
        // what `new` passed to build it ([`made_by`]).
        Receiver::Instance {
            at,
            arity,
            arguments,
            keywords,
        } => {
            // `Attribute.new` where `Attribute = Attributes::Attribute` builds the class the alias
            // names ([`locator::alias_target`]).
            let class = constant_at(graph, uri_id, *at, sources.layout)?;
            let class = locator::alias_target(graph, class).unwrap_or(class);
            let singleton = singleton_of(graph, class);
            let new = singleton.and_then(|singleton| {
                member_of(sources, uri_id, singleton, StringId::from("new()"))
            });
            let written = Called {
                positional: arguments,
                keywords: keywords.as_deref(),
            };
            // **`new` has one rule for both spellings** ([`new_overridden`]): an instance of the
            // class, unless the class's own `self.new` declares something else.
            if let Some(singleton) = singleton
                && let Some(new) = new
                && new_overridden(sources, singleton, new, *arity)
            {
                return returned_on(
                    sources,
                    uri_id,
                    Typed::of(singleton, Derivation::default()),
                    Call {
                        method: "new",
                        arity: *arity,
                        block: &Block::None,
                        written,
                        safe: false,
                        on_self: false,
                    },
                    scope,
                );
            }
            let mut typed = Typed::of(class, Derivation::default());
            typed.made = made_by(sources, uri_id, class, new, *arity, written, scope);
            Some(typed)
        }
        // A literal's class is named, not resolved: `String` means `String` in every file.
        //
        // - **`declared` makes `[rbs]` off degrade, not break.** With no core signatures there is
        //   no such declaration, and `None` falls through to the name-based list.
        // - **What it holds rides beside the head.** `[1, 2]` is an `Array` whose element is an
        //   `Integer`, because the parser said so one level down. Read from the source, never
        //   inferred (see [`cursor::Receiver::Literal`](super::cursor::Receiver::Literal)), and
        //   resolved here because a class is only a name until the graph has one.
        Receiver::Literal {
            class, arguments, ..
        } => Some(
            Typed::of(declared(graph, class)?, Derivation::default()).holding(
                arguments
                    .iter()
                    .map(|held| declared(graph, (*held)?))
                    .collect(),
            ),
        ),
        // `to_s` answers where the call's own lookup gives nothing checkable. See [`converted`].
        Receiver::Returned {
            on,
            method,
            block,
            arity,
            arguments,
            keywords,
            safe,
        } => {
            let call = Call {
                method,
                arity: *arity,
                block,
                written: Called {
                    positional: arguments,
                    keywords: keywords.as_deref(),
                },
                safe: *safe,
                on_self: matches!(on.as_ref(), Receiver::SelfObject(_)),
            };
            // The receiver is typed once, for both questions below.
            let owner = method_receiver(sources, uri_id, on, scope);
            // A proc literal called directly reads its own body. See [`called_proc`].
            if let Some(answer) = owner
                .as_ref()
                .and_then(|owner| called_proc(sources, uri_id, owner, call, scope))
            {
                return answer;
            }
            // So does a bound `Method` its method. See [`called_method`].
            if let Some(answer) = owner
                .as_ref()
                .and_then(|owner| called_method(sources, uri_id, owner, call, scope))
            {
                return answer;
            }
            let answer = match owner
                .and_then(|owner| returned_by(sources, uri_id, on, owner, call, scope))
            {
                Some(typed) if typed.derivation.tier() != Tier::Guessed => Some(typed),
                answer => converted(sources, uri_id, on, call, scope).or(answer),
            };
            // A `break` in the block ends the call with its own value instead. See [`broken`].
            if block.breaks().is_empty() {
                answer
            } else {
                broken(sources, uri_id, answer?, block.breaks(), scope)
            }
        }
        // One target of a multiple assignment: the same lookup, for one position of the tuple. Only
        // a **call** can be destructured by a signature; a constant, a literal or `self` has no
        // positions, so every other shape answers nothing rather than position zero.
        Receiver::Destructured { of, index } => match of.as_ref() {
            Receiver::Returned {
                on,
                method,
                block,
                arity,
                // A tuple is one entry per method (see [`returned_element`]), so the arguments
                // cannot change which arm is spread.
                arguments: _,
                keywords: _,
                safe,
            } => returned_element(
                sources,
                uri_id,
                on,
                Call {
                    method,
                    arity: *arity,
                    block,
                    written: Called {
                        positional: &[],
                        keywords: None,
                    },
                    safe: *safe,
                    on_self: matches!(on.as_ref(), Receiver::SelfObject(_)),
                },
                scope,
                *index,
            ),
            _ => None,
        },
        // The same declaration from the other end: what the block was handed, not what the call
        // returned.
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
        } => yielded_by(
            sources,
            uri_id,
            on,
            Call {
                method,
                arity: *arity,
                block: &Block::None,
                written: Called {
                    positional: arguments,
                    keywords: keywords.as_deref(),
                },
                safe: *safe,
                on_self: matches!(on.as_ref(), Receiver::SelfObject(_)),
            },
            Handing {
                index: *index,
                spreads: *spreads,
                default: default.as_deref(),
            },
            scope,
        ),
        // An instance variable is what its assignment was, plus a note saying where. The note is
        // why the variant exists; see `Receiver::Assigned`.
        Receiver::Assigned { at, was } => {
            let mut typed = method_receiver(sources, uri_id, was, scope)?;
            typed.derivation.assignments = vec![*at];
            Some(typed)
        }
        // A read: every write that can reach it, folded. See [`variable`].
        Receiver::Variable(at) => variable(sources, uri_id, *at),
        // `super`: the same lookup as a call, with a different way of finding the member. The
        // receiver is `self` and the name is the enclosing `def`'s.
        Receiver::Super {
            at,
            method,
            block,
            arity,
        } => from_super(sources, uri_id, *at, method, *arity, *block),
        // A method parameter: the one Ruby binding no write introduces, and what a declaration
        // says of it. See [`from_parameter`].
        Receiver::Parameter { at, method, slot } => {
            from_parameter(sources, uri_id, *at, method, slot)
        }
        // One side of `block_given?`: its value, wherever it is reached. A read for one call
        // leaves out the side that call cannot reach before asking ([`unreached`]).
        Receiver::BlockGiven { value, .. } => method_receiver(sources, uri_id, value, scope),
        // A proc literal: a `Proc` that remembers which one, for a call of it. See [`proc_value`].
        // `->` is syntax. `lambda { }` and `proc { }` are calls, typed as any other, with their
        // tier, and are the literal only where Ruby's own method answered.
        Receiver::Proc { at, call } => {
            let proc = declared(graph, "Proc")?;
            let mut typed = match call {
                None => Typed::of(proc, Derivation::default()),
                Some(call) => {
                    let typed = method_receiver(sources, uri_id, call, scope)?;
                    if typed.classes() != [proc] || !made_by_ruby(call, &typed) {
                        return Some(typed);
                    }
                    typed
                }
            };
            typed.procs = Some(Rc::from([(uri_id, *at)]));
            Some(typed)
        }
        // What the call of the literal being read passed there. See [`proc_parameter`].
        Receiver::ProcParameter { at, index } => proc_parameter(sources, uri_id, *at, *index),
        // What the call's own block hands back, inside a body read for that call. See
        // [`yielded_to_block`].
        Receiver::Yield {
            at,
            method,
            arguments,
        } => yielded_to_block(sources, uri_id, *at, method, arguments, scope),
        // `a && b` is `a` or `b`, never a third thing. Which one depends on whether `a` is falsy, a
        // property of `a`'s class, so it is evaluated here, not approximated in `cursor`. See
        // [`shortcut`].
        Receiver::Shortcut { left, right, and } => {
            shortcut(sources, uri_id, left, right, *and, scope)
        }
        // `!x` is `true` or `false`. Which one depends on whether `x` is falsy, the same property
        // [`shortcut`] reads, so it is evaluated in the same place. See [`negated`].
        Receiver::Negated(on) => negated(sources, uri_id, on, scope),
        // A conditional read as a value: whichever branch ran. See [`either`].
        Receiver::Either(arms) => either(sources, uri_id, arms, scope),
        // An exception caught: an instance of a class the `rescue` named. See [`rescued`].
        Receiver::Rescued(classes) => rescued(sources, uri_id, classes),
        // The two rungs below the graph, in the only safe order: a convention that can be checked,
        // then a guess that cannot.
        Receiver::Named(name) => named(sources, name, scope),
        // The fall-through. The `or_else` is its safety: the assignment is asked first and the
        // spelling only when it answered nothing, so a guess never displaces a chain that resolves.
        // No new rung: `named` is the same pair a bare name reaches, so the label matches and
        // `[types] guess_from_names = false` turns it off.
        //
        // **A read still being answered is not a read nothing answers.** Inside a cycle, a
        // variable's own value is not known yet ([`Reads::pending`]); falling to its spelling there
        // would let a guess into the value the cycle settles on.
        Receiver::Spelled { was, name } => {
            let answer = pending_aware(sources, || method_receiver(sources, uri_id, was, scope));
            // A read still being answered, or one a check rules every value out of:
            // neither is a name to guess from.
            if answer.is_none() && (pending(sources) || dead(sources, uri_id, was)) {
                return None;
            }
            answer.or_else(|| named(sources, name, scope))
        }
        // `::Foo.bar` reaches here as an ordinary constant; a bare `::` never does.
        Receiver::TopLevel | Receiver::Unknown => None,
    }
}

/// What `self` is at `offset` where a block or lambda around it runs against something a
/// signature names ([`Types::selves`]), innermost block first.
///
/// - **`None` where no block around `offset` is rebound**, and `self` is the body's, as before.
///   `Some(None)` where one is, and its type cannot be read against this receiver (an `instance` of
///   something that is not a class object): the body's `self` would be the wrong answer, so
///   nothing is.
/// - **A block whose call cannot be looked up is passed over**, as it was before signatures could
///   say anything: its receiver is untyped or only guessed, or the member is not there. A guess
///   deciding what `self` is would carry its tier into every receiverless call in the block.
/// - **A plain block is passed over too** (`each`, `map`): its call is found and binds nothing,
///   so an `each` inside `before_save do … end` still runs against the record.
/// - **The call's receiver is resolved where the call starts**, outside the block, so a nested
///   rebinding reads the outer one's answer.
pub(crate) fn rebound_self(
    sources: &Sources<'_>,
    uri_id: UriId,
    offset: u32,
) -> Option<Option<Typed>> {
    let graph = sources.graph;
    let reads = &sources.memo.reads;
    let uri = graph.documents().get(&uri_id)?.uri().to_owned();
    let document = reads.documents.of(&uri, sources.read)?;
    let at = document
        .rebase
        .span_to_buffer(ByteSpan {
            start: offset,
            end: offset,
        })?
        .start;
    for site in document.shapes(&uri, sources.held_exits).rebinding(at) {
        // **A class a generator made for this very block** answers before any signature: two calls
        // of one method make two classes, which no signature can say.
        if let Some(bound) = document
            .rebase
            .to_graph(site.call)
            .and_then(|call| sources.generated.ran(&uri, call))
        {
            return Some(match bound {
                Runs::Made(class) => made_for(graph, class, &site.method),
                Runs::Each { of, classes } => each_of(graph, of, classes, sources.object),
                Runs::Refused => None,
            });
        }
        let key = (
            uri_id,
            site.call,
            sources.object,
            sources.bound.unwrap_or(0),
            sources.made.unwrap_or(0),
        );
        let held = reads.rebound.borrow().get(&key).cloned();
        let answer = match held {
            Some(answer) => answer,
            None => {
                let answer = pending_aware(sources, || site_self(sources, uri_id, &document, site));
                // A block whose call was being answered in a cycle is asked again next round.
                if !pending(sources) {
                    reads.rebound.borrow_mut().insert(key, answer.clone());
                }
                answer
            }
        };
        if answer.is_some() {
            return answer;
        }
    }
    None
}

/// The class objects of every class that includes `of` ([`Runs::Each`]), joined: `None` unless the
/// graph holds every one, since an answer resting on some of them would read as all.
///
/// **A body read for a known object is that object's alone** ([`Sources::object`]): a `scope` the
/// concern writes, called on `Article`, runs its lambda against `Article`'s relation, never the
/// other includers'. Where the object is one of the classes or below one, it is the only class.
fn each_of(
    graph: &Indexed,
    of: &str,
    classes: &[String],
    object: Option<DeclarationId>,
) -> Option<Typed> {
    let declared_classes: Vec<DeclarationId> = classes
        .iter()
        .map(|class| declared(graph, class))
        .collect::<Option<_>>()?;
    let alone = object.filter(|object| {
        declared_classes
            .iter()
            .any(|class| object == class || descends(graph, *object, *class))
    });
    // The object alone is the receiver the body is read for, not "every includer": nothing to
    // record.
    if let Some(object) = alone {
        return Some(Typed::of(
            singleton_of(graph, object)?,
            Derivation::default(),
        ));
    }
    let chosen = declared_classes;
    let folds = Folds::of(graph);
    let mut join = Join::default();
    for class in chosen {
        let singleton = singleton_of(graph, class)?;
        join.add(
            Typed::of(
                singleton,
                Derivation {
                    each: Some(of.to_owned()),
                    ..Derivation::default()
                },
            ),
            &folds,
        );
    }
    join.finish(&folds)
}

/// The class object a generator made for a block of `method`'s ([`rebound_self`]), where the graph
/// holds it.
fn made_for(graph: &Indexed, class: &str, method: &str) -> Option<Typed> {
    let singleton = singleton_of(graph, declared(graph, class)?)?;
    Some(Typed::of(
        singleton,
        Derivation {
            ran: Some(method.to_owned()),
            ..Derivation::default()
        },
    ))
}

/// What one block runs against ([`rebound_self`]): `None` where it is passed over, `Some(None)`
/// where a binding cannot be read against this receiver or its call cannot be asked.
fn site_self(
    sources: &Sources<'_>,
    uri_id: UriId,
    document: &Document,
    site: &cursor::BlockSite,
) -> Option<Option<Typed>> {
    let graph = sources.graph;
    let (Some(on), Some(call)) = (
        site.on.rebased(&document.rebase),
        document.rebase.to_graph(site.call),
    ) else {
        // A call in text the graph has not seen: whether it rebinds cannot be asked, and the body's
        // `self` may be the wrong answer.
        return Some(None);
    };
    let scope = sources.scope_at(uri_id, call);
    let owner = method_receiver(sources, uri_id, &on, &scope)
        .filter(|owner| owner.derivation.tier() != Tier::Guessed)?;
    let on_self = matches!(on, Receiver::SelfObject(_));
    let member = StringId::from(&format!("{}()", site.method));
    let Some(one) = owner.one() else {
        return site_self_each(sources, uri_id, &owner, member, on_self, site);
    };
    let found = reach(sources, uri_id, one, member, on_self)?;
    let rebound = sources.types.rebound(found, &site.slot)?;
    let mut derivation = owner.derivation.clone();
    derivation
        .signatures
        .push(graph.declarations().get(&found)?.name().to_owned());
    Some(match rebound {
        Rebound::Instance => {
            instance_of(graph, one).map(|instance| Typed::of(instance, derivation))
        }
        Rebound::Returned(returned) => {
            resolved(sources, &returned.of, &owner, None).map(|declaration| {
                Typed::of(declaration, derivation)
                    .faceted(returned)
                    .holding(held_by_return(sources, &returned.of, &owner, None))
            })
        }
        Rebound::Unread => None,
    })
}

/// [`site_self`] for a receiver that is one of several classes: each class's own answer, joined
///. A callback block in a concern's included block runs against a record of
/// whichever including class runs it.
///
/// - **Every class must agree about rebinding.** Where none has the member or none rebinds, the
///   block is passed over, as for one class. Where only some do, the block's `self` is refused:
///   an answer resting on some of the classes would read as all of them.
/// - **Any class whose rebinding cannot be read refuses it**, as one class's would.
fn site_self_each(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: &Typed,
    member: StringId,
    on_self: bool,
    site: &cursor::BlockSite,
) -> Option<Option<Typed>> {
    let graph = sources.graph;
    let folds = Folds::of(graph);
    let mut join = Join::default();
    let mut rebinding: Option<bool> = None;
    for &class in owner.classes() {
        let rebound = reach(sources, uri_id, class, member, on_self)
            .map(|found| (found, sources.types.rebound(found, &site.slot)));
        let binds = matches!(rebound, Some((_, Some(_))));
        if rebinding.is_some_and(|before| before != binds) {
            return Some(None);
        }
        rebinding = Some(binds);
        let Some((found, Some(rebound))) = rebound else {
            continue;
        };
        let mut derivation = owner.derivation.clone();
        derivation
            .signatures
            .push(graph.declarations().get(&found)?.name().to_owned());
        let alone = Typed::of(class, owner.derivation.clone());
        let typed = match rebound {
            Rebound::Instance => {
                instance_of(graph, class).map(|instance| Typed::of(instance, derivation))
            }
            Rebound::Returned(returned) => {
                resolved(sources, &returned.of, &alone, None).map(|declaration| {
                    Typed::of(declaration, derivation)
                        .faceted(returned)
                        .holding(held_by_return(sources, &returned.of, &alone, None))
                })
            }
            Rebound::Unread => None,
        };
        let Some(typed) = typed else {
            return Some(None);
        };
        join.add(typed, &folds);
    }
    if rebinding == Some(true) {
        return Some(join.finish(&folds));
    }
    None
}

/// A conditional's value ([`Receiver::Either`]): every branch it can hand back, joined
/// ([`Join`]). One branch nothing types refuses the whole, as one write refuses a read.
fn either(sources: &Sources<'_>, uri_id: UriId, arms: &[Receiver], scope: &Scope) -> Option<Typed> {
    let folds = Folds::of(sources.graph);
    let mut join = Join::default();
    for arm in arms {
        if unreached(sources, uri_id, arm) {
            continue;
        }
        let Some(typed) = method_receiver(sources, uri_id, arm, scope) else {
            // A branch a check leaves no value for never runs.
            if dead(sources, uri_id, arm) {
                continue;
            }
            return None;
        };
        join.add(typed, &folds);
    }
    join.finish(&folds)
}

/// The exception a `rescue` bound ([`Receiver::Rescued`]): an instance of one of the classes it
/// names, joined, or of `StandardError` where it names none.
///
/// - **A named class is a supertype of what was caught**, as a signature's class is of what a
///   method returns: `rescue Net::ReadTimeout => e` may hold a subclass, which has every member the
///   class has.
/// - **Only a class.** A module can be rescued (its includers are caught), but it has no instances
///   to name, and a constant nothing defines (a `Todo`) is no class at all. Either refuses.
fn rescued(sources: &Sources<'_>, uri_id: UriId, classes: &[u32]) -> Option<Typed> {
    let graph = sources.graph;
    let class = |id: DeclarationId| {
        matches!(
            graph.declarations().get(&id),
            Some(Declaration::Namespace(Namespace::Class(_)))
        )
        .then(|| Typed::of(id, Derivation::default()))
    };
    if classes.is_empty() {
        return class(declared(graph, "StandardError")?);
    }
    let folds = Folds::of(graph);
    let mut join = Join::default();
    for at in classes {
        join.add(
            class(constant_at(graph, uri_id, *at, sources.layout)?)?,
            &folds,
        );
    }
    join.finish(&folds)
}

/// `to_s`, the one conversion every object answers, and the class it returns.
///
/// - **`Kernel#to_s`, so every object has it**, and Ruby raises a `TypeError` where it converts
///   with one that returns another class (`String(x)`).
/// - **An override may still return another class**, since a direct call is not checked: one application's
///   admin fields answer an `Integer` or a `Hash`, which `<%= %>` converts again. Of the 4,584
///   overrides in the six corpora and their gems, 2,969 read as a `String` and eight were found
///   returning something else. Kept anyway, by decision (2026-09-26).
/// - **No other conversion.** None of `to_i`, `to_f`, `to_a`, `to_hash` and the rest is on
///   `Object`, so a call reaches only the classes that define one, and overrides returning `nil`
///   or another class were found for `to_f`, `to_r`, `to_a`, `to_ary` and `to_hash`. `to_sym`,
///   `to_h`, `inspect` and `to_json` are a habit Ruby never checks.
const CONVERSION: (&str, &str) = ("to_s", "String");

/// What `to_s` returns ([`CONVERSION`]), whatever it was called on.
///
/// - **Asked only where the call's own lookup answered nothing, or only a guess.** A body or a
///   signature is a fact about that method and comes first: `def to_s = 42` stays `Integer`. A
///   guessed receiver gives way, since this answer does not depend on it.
/// - **A receiver whose type is known must have the member**, on every class it may be. Otherwise
///   the member is one this server cannot see, and a label would hide that.
/// - **`nil.to_s` is `""`**, so `x.to_s` on an unknown `x` is a `String`. `x&.to_s` skips the call
///   on `nil`, so it is `String?` unless the receiver is known not to be `nil`.
/// - **Derived, recorded as such** ([`Derivation::conversion`]): the answer rests on Ruby's
///   rule, not on anything this file or a signature says.
fn converted(
    sources: &Sources<'_>,
    uri_id: UriId,
    on: &Receiver,
    call: Call<'_>,
    scope: &Scope,
) -> Option<Typed> {
    let (method, class) = CONVERSION;
    if call.method != method {
        return None;
    }
    let class = declared(sources.graph, class)?;
    let owner = method_receiver(sources, uri_id, on, scope)
        .filter(|owner| owner.derivation.tier() != Tier::Guessed);
    let member = StringId::from(&call.member());
    if let Some(owner) = &owner
        && !owner
            .classes
            .iter()
            .all(|one| reach(sources, uri_id, *one, member, call.on_self).is_some())
    {
        return None;
    }
    let typed = Typed::of(
        class,
        Derivation {
            conversion: Some(method),
            ..Derivation::default()
        },
    );
    Some(if call.safe && owner.is_none_or(|owner| owner.nilable) {
        typed.or_nil()
    } else {
        typed
    })
}

/// One link of a chain: what the call was written on, and what RBS says it returns.
///
/// The lookup happens *after* rubydex has found the member. `[].tap` is owned by `Kernel`, not
/// `Array`, so asking the table for `Array#tap()` would miss. The ancestor walk is rubydex's; this
/// only reads the declaration it found.
///
/// # A receiver that may be `nil`
///
/// A `T?` is two receivers, and the call runs on whichever one the value is. [`returned_on`] asks
/// `T`. What happens on `nil` depends on how the call was written:
///
/// - **`a&.m` skips the call on `nil` and answers `nil`**, so the answer is `T`'s plus the mark,
///   and `NilClass` is never asked. `x&.nil?` is `false?`, not `bool`.
/// - **`a.m` makes the call on `nil` too**, so `NilClass` is asked the same name ([`from_nil`]) and
///   the two answers are folded. `x.nil?` is `bool`, not `Kernel#nil?`'s `false`, and `x.dup` is
///   `T?`.
/// - **Where `NilClass` has no answer, `T`'s stands**, the one inexact entry the module docs name.
///   Ruby raises there, so there is no second value to fold.
///
/// A receiver without the mark is taken at its word: `Post.new&.user` is `Post.new.user`.
fn returned_by(
    sources: &Sources<'_>,
    uri_id: UriId,
    on: &Receiver,
    owner: Typed,
    call: Call<'_>,
    scope: &Scope,
) -> Option<Typed> {
    // **A bare name in a partial is a local where its render calls pass one**, which
    // Ruby reads before any method of the name.
    if matches!(on, Receiver::SelfObject(_))
        && call.arity == Arity::Exactly(0)
        && matches!(call.block, Block::None)
        && !call.safe
    {
        match partial_local(sources, uri_id, call.method) {
            Local::Typed(typed, _) => return Some(*typed),
            Local::Refused => return None,
            Local::NotOne => {}
        }
    }
    // **A receiverless call in a template or a helper is the view's**: `self` there is
    // the view, whose helpers and ActionView come before `Object`. The same rung `locator` answers
    // a card from ([`views::Reachable::member`]), so a margin and a card read one rule.
    if matches!(on, Receiver::SelfObject(_))
        && let Some(reached) = view_context(sources, uri_id)
            .and_then(|reachable| reachable.member(sources.graph, &call.member()))
    {
        let mut typed = returned_for(sources, uri_id, owner, reached.declaration, call, scope)?;
        typed.derivation.view = Some(reached.how);
        return Some(typed);
    }
    either_receiver(sources, uri_id, owner, call, scope)
}

/// [`returned_by`] past the view's rung: `T`'s answer, and `nil`'s where the receiver may be `nil`.
fn either_receiver(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: Typed,
    call: Call<'_>,
    scope: &Scope,
) -> Option<Typed> {
    if !owner.nilable {
        return returned_on(sources, uri_id, owner, call, scope);
    }
    // Kept for the `nil` half, which is asked on the same evidence the receiver rests on.
    let derivation = owner.derivation.clone();
    let carried = Typed {
        nilable: false,
        ..owner
    };
    let here = returned_on(sources, uri_id, carried, call, scope)?;
    if call.safe {
        return Some(here.or_nil());
    }
    match from_nil(sources, uri_id, derivation, call, scope) {
        Some(there) => either_half(sources.graph, here, there),
        None => Some(here),
    }
}

/// What `nil` answers to a call written on a `T?`, or `None` where Ruby would raise.
///
/// [`split_bool`]'s sibling: the carrier holds one half of the value, and this asks the other.
/// `None`, which keeps `T`'s answer, in four cases:
///
/// 1. **`NilClass` lacks the member.** `nil.name` raises, so there is no second value.
/// 2. **The member is private.** A top-level `def` is a private `Object` method, and a written
///    receiver cannot reach it. one application's scripts define `id`, `upload` and `match` that way, so
///    reading them would cost `x.id` its `Integer`. [`locator::is_private`] is the same test
///    navigation applies, repair included.
/// 3. **`NilClass`'s answer is unreadable.** `NilClass#to_a: () -> []` is a tuple, which the table
///    drops.
/// 4. **`NilClass` is not declared**, because `[rbs]` is off.
///
/// The receiver really is `NilClass`, so a `self` in the signature answers `nil`: `Kernel#dup` on
/// a `String?` is `String` from one half and `nil` from the other.
fn from_nil(
    sources: &Sources<'_>,
    uri_id: UriId,
    derivation: Derivation,
    call: Call<'_>,
    scope: &Scope,
) -> Option<Typed> {
    let graph = sources.graph;
    let nil = declared(graph, "NilClass")?;
    // Written on a receiver that may be `nil`, never on `self`.
    let found = reach(sources, uri_id, nil, StringId::from(&call.member()), false)?;
    returned_for(
        sources,
        uri_id,
        Typed::of(nil, derivation),
        found,
        call,
        scope,
    )
}

/// The value a call answers when it can run on either half of a `T?` or a `bool`: either one, by
/// [`Join`]'s rule. So `false` from one half and `true` from the other are `bool`, and `T` beside
/// `nil` is `T?`.
fn either_half(graph: &Graph, here: Typed, there: Typed) -> Option<Typed> {
    let folds = Folds::of(graph);
    let mut join = Join::default();
    join.add(here, &folds);
    join.add(there, &folds);
    join.finish(&folds)
}

/// [`returned_by`] for one receiver, whatever its mark says.
///
/// The mark is never read here: a `T?` reaches this with it cleared, and [`returned_by`] decides
/// what `nil` adds.
fn returned_on(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: Typed,
    call: Call<'_>,
    scope: &Scope,
) -> Option<Typed> {
    if owner.classes.len() > 1 && !owner.boolean {
        return narrowed(sources, uri_id, &owner, call, scope);
    }
    let graph = sources.graph;
    let member = call.member();
    // **Not `?`**: the rung below does not need the member to exist. `Class#new` is declared only
    // by Ruby's own signatures, so with `[rbs]` off there is no `new` to find. And `new` meaning
    // *an instance of this class* is Ruby's rule, not a signature's.
    let found = reach(
        sources,
        uri_id,
        owner.one()?,
        StringId::from(&member),
        call.on_self,
    );
    // **A name a collection lacks is its element's class's** ([`delegated`]).
    let (owner, found) = match found {
        None => match owner
            .one()
            .and_then(|one| delegated(sources, uri_id, one, StringId::from(&member)))
        {
            Some((class_object, found)) => (
                Typed::of(class_object, owner.derivation.clone()),
                Some(found),
            ),
            None => (owner, None),
        },
        found => (owner, found),
    };
    // **`new` sent to a class object is an instance of that class.** Ruby's rule, not an inference.
    //
    // - **The half [`cursor::instantiated`] cannot see.** It reads the text, so it resolves
    //   `Foo.new` but not `new` inside `def self.call`, `self.new`, or `new` on a local holding the
    //   class. `def self.call; new(...).call; end` is the commonest service-object entry point in
    //   Rails.
    // - **Unless the class's own `self.new` declares something else** ([`new_overridden`]), the
    //   one rule `Foo.new(…)` written literally ([`Receiver::Instance`]) asks too, so the two
    //   spellings agree.
    // - **[`instance_of`] refuses the instance side.** A bare `new` inside `def perform` is some
    //   private method, never `Class#new`; the receiver's name has no `::<` to strip, so nothing is
    //   answered.
    if call.method == "new"
        && found.is_none_or(|found| {
            owner.one().is_none_or(|class_object| {
                !new_overridden(sources, class_object, found, call.arity)
            })
        })
        && let Some(instance) = owner.one().and_then(|one| instance_of(graph, one))
    {
        let mut typed = Typed::of(instance, owner.derivation);
        typed.made = made_by(
            sources,
            uri_id,
            instance,
            found,
            call.arity,
            call.written,
            scope,
        );
        return Some(typed);
    }
    // **A Ruby `alias` of a method nothing declares is that method** ([`renamed`]).
    let found = found.map(|found| renamed(sources, found).unwrap_or(found));
    let found = found?;
    // **`send(:name, …)` is the call `name(…)`**, on the same receiver ([`sent`]).
    if let Some(answer) = sent(sources, uri_id, &owner, found, call, scope) {
        return answer;
    }
    // **A key a body of knowledge keeps is what it holds there** ([`keyed`]).
    if sources.types.keyed.contains(&found)
        && let Some(answer) = keyed(sources, &owner, found, call)
    {
        return Some(answer);
    }
    // **`method(:name)` is a `Method` bound to `name`** on the same receiver ([`bound_to`]).
    if let Some(namer) = namer(sources, found).filter(|namer| namer.object) {
        let answer = returned_for(sources, uri_id, owner.clone(), found, call, scope)?;
        return Some(bound_to(sources, uri_id, answer, owner, call, namer));
    }
    // **`class` sent to an object is that object's class object.** Ruby's rule, like `new` above.
    //
    // - **Why:** core RBS declares `Kernel#class: () -> Class`. In a module RBS cannot say "the
    //   class of this object", so the answer is every class at once, and `self.class.new(…)` or
    //   `self.class.some_class_method` stops there.
    // - **Only `Kernel#class` itself.** A class that defines its own `class` (a proxy) keeps it.
    // - **Only a receiver that is one class.** A module is no object's class, a class object's
    //   class is `Class` (which the signature already says), and `bool` is two classes.
    if call.method == "class"
        && !owner.boolean
        && graph
            .declarations()
            .get(&found)
            .is_some_and(|declaration| declaration.name() == "Kernel#class()")
        && let Some(one) = owner.one()
        && matches!(
            graph.declarations().get(&one),
            Some(Declaration::Namespace(Namespace::Class(_)))
        )
        && let Some(singleton) = singleton_of(graph, one)
    {
        return Some(Typed::of(singleton, owner.derivation));
    }
    returned_for(sources, uri_id, owner, found, call, scope)
}

/// A member a **collection** lacks, answered by its element's class: `(the class object, the
/// member)`.
///
/// - **ActiveRecord's rule.** A relation hands a name it does not have to its model's class
///   (`Relation#method_missing`, with `public_send`, inside the relation's scope) and returns what
///   that answers. So a model's own `def self.digest`, called on `Story.where(…)` or inside a
///   `scope` lambda, whose `self` is the relation, answers as it does on the class.
/// - **Only a collection this crate named** (`generated::element_of`), whose element is declared,
///   and only a public member: `public_send` raises on a private one.
/// - **Gated on models**, as [`Return::Collection`] is.
fn delegated(
    sources: &Sources<'_>,
    uri_id: UriId,
    collection: DeclarationId,
    member: StringId,
) -> Option<(DeclarationId, DeclarationId)> {
    if !sources.features.models {
        return None;
    }
    let graph = sources.graph;
    let element = generated::element_of(graph.declarations().get(&collection)?.name())?;
    let class_object = singleton_of(graph, declared(graph, element)?)?;
    let found = reach(sources, uri_id, class_object, member, false)?;
    Some((class_object, found))
}

/// A call on a **union**, made on whichever class the value turns out to be.
///
/// - **A class without the member drops out.** Ruby raises `NoMethodError` there, so the call
///   never returns on that class and adds nothing to its value. `Story.find(params[:id]).title` is
///   `Story#title`'s answer: an `Array[Story]` has no `title`. [`returned_by`] applies the same rule
///   to a `T?`'s `nil`.
/// - **Every class that has it answers, joined** ([`Join`]): `to_s` on `Story | Array[Story]` is a
///   `String` either way.
/// - **A member [`reach`] refuses is not there either**: private on a written receiver raises, as
///   for `nil` ([`from_nil`]), and a member the root gate withdrew is no member on any other road.
///   A script's top-level `def tags`, or one inside a block that rubydex files on `Object`, must
///   not refuse `Story#tags` on `Story | Array[Story]`.
/// - **Refused where a class might answer after all**: it writes its own `method_missing`, which
///   may answer any name. `BasicObject`'s raises, so it does not count.
/// - **Refused where no class has it**, and where one class that has it answers nothing: the union
///   of what is left would rest on the classes that happened to answer.
fn narrowed(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: &Typed,
    call: Call<'_>,
    scope: &Scope,
) -> Option<Typed> {
    let graph = sources.graph;
    let folds = Folds::of(graph);
    let classes = answering(
        sources,
        uri_id,
        &owner.classes,
        StringId::from(&call.member()),
        call.on_self,
    )?;
    let mut join = Join::default();
    for class in classes {
        let one = returned_on(
            sources,
            uri_id,
            Typed::of(class, owner.derivation.clone()),
            call,
            scope,
        )?;
        join.add(one, &folds);
    }
    join.finish(&folds)
}

/// The classes of a union a call on it can run on: [`narrowed`]'s rule, for the call and for the
/// jump ([`narrowed_classes`]).
///
/// `None` where a class might answer after all (its own `method_missing`) and where no class has
/// the member.
fn answering(
    sources: &Sources<'_>,
    uri_id: UriId,
    classes: &[DeclarationId],
    member: StringId,
    on_self: bool,
) -> Option<Vec<DeclarationId>> {
    let graph = sources.graph;
    let missing = StringId::from("method_missing()");
    let mut answering = Vec::new();
    for &class in classes {
        if reach(sources, uri_id, class, member, on_self).is_some() {
            answering.push(class);
            continue;
        }
        // Found and not reached is not reached: private on a written receiver raises, as a
        // missing member does ([`from_nil`]'s rule), and a member the root gate withdrew is no
        // member anywhere else either ([`reach`]). Only a class's own `method_missing` may answer.
        let answers_anyway = member_of(sources, uri_id, class, member).is_none()
            && member_of(sources, uri_id, class, missing)
                .and_then(|found| graph.declarations().get(&found))
                .is_some_and(|found| !found.name().starts_with("BasicObject#"));
        if answers_anyway {
            return None;
        }
    }
    (!answering.is_empty()).then_some(answering)
}

/// The classes of a union a call to `member` (keyed with its parentheses) runs on, for navigation:
/// [`narrowed`]'s rule. Each is looked up for its own declaration (`locator::on_a_typed_receiver`),
/// so the answer is one place where they all reach one `def` (`URI.parse`'s ten classes and
/// `URI::Generic#host`), and each class's own where they differ. `None` for one class, `bool`,
/// and wherever [`answering`] refuses.
#[must_use]
pub fn narrowed_classes(
    sources: &Sources<'_>,
    uri_id: UriId,
    typed: &Typed,
    member: &str,
) -> Option<Vec<DeclarationId>> {
    if typed.classes.len() < 2 || typed.boolean {
        return None;
    }
    answering(
        sources,
        uri_id,
        &typed.classes,
        StringId::from(member),
        false,
    )
}

/// What `new(…)` passed, bound to the `initialize` the instance runs, for
/// [`Typed::made`].
///
/// - **Only where `new` is `Class#new`**, or where nothing declares one (`[rbs]` off). A class's own
///   `self.new` may hand `super` something else (`super(amount.to_d)`), and binding what the call
///   wrote would then type `initialize`'s parameters wrong. The receiver rungs still answer an
///   instance there, as before.
/// - **[`Binding::of`]'s rules**: a Ruby `initialize` whose definitions agree, positionals only in
///   a `def` of required and optional positionals, keywords by name, a left-out argument's
///   default. `Object`'s own `initialize` has no Ruby, so a class without one binds nothing.
/// - **The arguments are typed where `new` is written**, by [`passed_to`], as for any call.
fn made_by(
    sources: &Sources<'_>,
    uri_id: UriId,
    class: DeclarationId,
    new: Option<DeclarationId>,
    arity: Arity,
    written: Called<'_>,
    scope: &Scope,
) -> Option<Made> {
    let graph = sources.graph;
    if let Some(new) = new
        && graph.declarations().get(&new)?.name() != "Class#new()"
    {
        return None;
    }
    let initialize = member_of(sources, uri_id, class, StringId::from("initialize()"))?;
    // **Nothing to bind, so nothing to type**: `Object`'s own `initialize` has no Ruby, and typing
    // every argument of every such `new` is most of this rung's cost.
    let ruby = locator::definitions_of(graph, initialize)
        .iter()
        .any(|definition| {
            matches!(definition, Definition::Method(_))
                && graph
                    .documents()
                    .get(definition.uri_id())
                    .is_some_and(|document| !document.uri().ends_with(".rbs"))
        });
    if !ruby {
        return None;
    }
    let passed = passed_to(sources, uri_id, arity, written, &Block::None, scope)?;
    Binding::of(sources, initialize, &passed).map(|binding| Made(Rc::new(binding)))
}

/// [`returned_on`] once the member is found: what the call on `owner` returns, read from `found`.
// Nine arguments, as [`returned_by`] takes: one call as the text wrote it, plus what it resolved to.
fn returned_for(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: Typed,
    found: DeclarationId,
    call: Call<'_>,
    scope: &Scope,
) -> Option<Typed> {
    let Call {
        arity,
        block,
        written,
        ..
    } = call;
    let declared = sources
        .types
        .returns_to(found, arity, block.written(), written.keywords);
    // **Read only where the signature has a slot for it**, because reading it resolves a second
    // expression. `each` is the commonest block call in Ruby and its return says nothing about the
    // block. The declared return is the gate, so a block is walked for `map` and `then`, and
    // nothing else.
    let hand = || match block {
        Block::Symbol(name) => symbol_return(sources, uri_id, &owner, found, name, scope),
        Block::Passed(value) => passed_return(sources, uri_id, &owner, found, value, scope),
        _ => block_return(sources, uri_id, block, scope),
    };
    let handed = declared
        .is_some_and(|returns| substitutes_a_block(&returns.of))
        .then(hand)
        .flatten();
    // **What an accessor's writer is given** is read off the writer's calls, where the call is.
    if declared.is_some_and(|returns| matches!(returns.of, Return::Written)) {
        return written_to(sources, uri_id, found, &owner);
    }
    if declared.is_some_and(|returns| matches!(returns.of, Return::Shared)) {
        return shared_by(sources, uri_id, found, &owner);
    }
    if declared.is_some_and(|returns| matches!(returns.of, Return::Held)) {
        return current_held(sources, uri_id, found, &owner, None);
    }
    // **A current attribute's literal default beside what its writer is given**.
    if let Some(Returned {
        of: Return::Union(members),
        nilable,
    }) = declared
        && members.contains(&Return::Held)
    {
        let mut rest: Vec<Return> = members
            .iter()
            .filter(|member| **member != Return::Held)
            .cloned()
            .collect();
        let rest = Returned {
            of: match rest.len() {
                1 => rest.pop()?,
                _ => Return::Union(rest.into_boxed_slice()),
            },
            nilable: *nilable,
        };
        return current_held(sources, uri_id, found, &owner, Some(&rest));
    }
    // **And beside what something else set first**, where the writer is one member of a union.
    if let Some(Returned {
        of: Return::Union(members),
        nilable,
    }) = declared
        && members.contains(&Return::Written)
    {
        let mut rest: Vec<Return> = members
            .iter()
            .filter(|member| **member != Return::Written)
            .cloned()
            .collect();
        let rest = Returned {
            of: match rest.len() {
                1 => rest.pop()?,
                _ => Return::Union(rest.into_boxed_slice()),
            },
            nilable: *nilable,
        };
        return written_beside(sources, uri_id, found, &owner, &rest);
    }
    // **A lambda's value where truthy, beside the rest**: a `scope`.
    if let Some(Returned {
        of: Return::Union(members),
        nilable,
    }) = declared
        && members.contains(&Return::Scoped)
    {
        let mut rest: Vec<Return> = members
            .iter()
            .filter(|member| **member != Return::Scoped)
            .cloned()
            .collect();
        let rest = Returned {
            of: match rest.len() {
                1 => rest.pop()?,
                _ => Return::Union(rest.into_boxed_slice()),
            },
            nilable: *nilable,
        };
        return scoped_beside(sources, found, &owner, &rest);
    }
    // **What a member hands its call on to** is those calls, made where this one is.
    if let Some(Returned {
        of: Return::Forwarded { through, method },
        nilable,
    }) = declared
    {
        let answer = forwarded(
            sources,
            uri_id,
            found,
            &owner,
            (through, method),
            call,
            scope,
        )?;
        return Some(if *nilable { answer.or_nil() } else { answer });
    }
    // **A `bool` receiver is the pair, and the halves do not always agree.** Asked before the
    // ordinary resolve, and `None` wherever both halves name the same declaration, so it costs one
    // hash lookup on calls it does not change. See [`split_bool`].
    if let Some(split) = split_bool(sources, uri_id, &owner, found, call, handed.as_ref()) {
        return split;
    }
    // **A method's own `[X]` is what the call passed where the signature takes an `X`**, put in its
    // place before anything resolves. Only a return that names one pays for typing an argument.
    // See [`Return::Argument`].
    let bound = declared
        .filter(|returns| names_an_argument(&returns.of))
        .and_then(|returns| bound_to_arguments(sources, uri_id, returns, call, scope));
    // **Only where the partition already refused.** A method the table answers pays one hash lookup
    // it was paying anyway, and no argument is typed. See [`pick_by_argument`].
    // The symbols a call wrote pick first, where they pick at all: every arm they can pick is one
    // the partition reached, so they only narrow what the partition's arms agreed on
    // (`pluck(:depth)`'s `Array[Integer]`, where the arms agree on `Array`).
    let picked = bound
        .or_else(|| pick_by_literal(sources, found, call))
        .or_else(|| {
            declared
                .is_none()
                .then(|| {
                    pick_by_argument(sources, uri_id, found, call, scope, &owner)
                        .or_else(|| only_arm_reached(sources, found, call))
                })
                .flatten()
        });
    // **The one arm a call's keywords reach may take the block's value** (`Rails.cache.fetch(k,
    // expires_in: 1.hour) { … }`), which the partition's refusal left unread.
    let handed = handed.or_else(|| {
        picked
            .as_ref()
            .filter(|picked| substitutes_a_block(&picked.of))
            .and_then(|_| hand())
    });
    // **Where no arm could be picked, one of them still runs.** See [`join_arms`].
    if picked.is_none()
        && declared.is_none()
        && let Some(joined) = join_arms(sources, found, &owner, call)
    {
        return Some(joined);
    }
    // **What this call passed, for a body read at the seam**. Typed only where no
    // signature answers and no arm was picked, so a declared method pays nothing.
    let passed = (picked.is_none() && declared.is_none())
        .then(|| {
            // The block is typed only for a body that asks for it: a binding of its own makes every
            // call re-read the body. So is whether there is one.
            let asked = block_use(sources, found);
            let handing = if asked.hands { block } else { &Block::None };
            let mut passed = passed_to(sources, uri_id, arity, written, handing, scope)?;
            if asked.asks {
                passed.given = passes_a_block(sources, uri_id, arity, block, scope);
            }
            Some(passed)
        })
        .flatten();
    returned_from(
        sources,
        found,
        owner,
        (arity, written.keywords),
        block.written(),
        handed.as_ref(),
        picked,
        passed.as_ref(),
    )
}

/// What a member that hands its call on answers ([`Return::Forwarded`]): `through` asked of the
/// receiver, then the call as written asked of that.
///
/// - **The first call is the receiver's own**, written with no receiver, so a private member
///   answers it. A constant is spelled from the receiver's class outward.
/// - **The second is written on a receiver**, so it reaches public members only, and runs on each
///   half of a `T?` as any call does: where `nil` lacks the member Ruby raises, and the other half
///   stands.
/// - **A hop resting on a name guess makes the answer one**, and says so, as any chain through it
///   does: the derivation is the hops' own.
/// - **A member asked again while it is being answered refuses** ([`Reads::forwarding`]). A target
///   that is the receiver itself, or one a guess makes a class with the same member, would
///   otherwise recurse until the stack overflows, which no bulkhead contains.
fn forwarded(
    sources: &Sources<'_>,
    uri_id: UriId,
    found: DeclarationId,
    owner: &Typed,
    hops: (&str, &str),
    call: Call<'_>,
    scope: &Scope,
) -> Option<Typed> {
    let forwarding = &sources.memo.reads.forwarding;
    if forwarding.borrow().contains(&found) {
        return None;
    }
    forwarding.borrow_mut().push(found);
    let answer = forward(sources, uri_id, found, owner, hops, call, scope);
    forwarding.borrow_mut().pop();
    answer
}

/// [`forwarded`], unguarded.
fn forward(
    sources: &Sources<'_>,
    uri_id: UriId,
    found: DeclarationId,
    owner: &Typed,
    (through, method): (&str, &str),
    call: Call<'_>,
    scope: &Scope,
) -> Option<Typed> {
    let graph = sources.graph;
    let one = owner.one()?;
    let first = if through.starts_with(|first: char| first.is_ascii_uppercase() || first == ':') {
        let (name, scope) = match through.strip_prefix("::") {
            Some(absolute) => (absolute, ""),
            None => (through, graph.declarations().get(&one)?.name()),
        };
        Typed::of(
            singleton_of(graph, declared_in(graph, name, scope)?)?,
            owner.derivation.clone(),
        )
    } else {
        let asked = Call {
            method: through,
            arity: Arity::Exactly(0),
            block: &Block::None,
            written: Called {
                positional: &[],
                keywords: None,
            },
            safe: false,
            on_self: true,
        };
        returned_on(sources, uri_id, owner.clone(), asked, scope)?
    };
    let handed = Call {
        method,
        safe: false,
        on_self: false,
        ..call
    };
    let mut answer = either_receiver(sources, uri_id, first, handed, scope)?;
    answer
        .derivation
        .signatures
        .insert(0, graph.declarations().get(&found)?.name().to_owned());
    Some(answer)
}

/// What an accessor on `owner` hands back: every value a call of its writer on the same object
/// passes, and `nil`, which it holds before any does ([`Return::Written`]).
///
/// - **The calls come from rubydex's call index** ([`Indexed::calls_named`]), and each calling
///   document's [`cursor::Shapes::setters`] say what each passes, read once per content hash.
/// - **A call counts where its receiver is the same object**, typed to it where it is written
///   (`Current.account = x`, `self.account = x`). A call on anything else is some other object's
///   writer of that name, and one on a receiver nothing types is passed over.
/// - **Only the application's own Ruby is read**, fenced as the asking document is
///   ([`environment::Fence`]): a spec's doubles do not answer for the application.
/// - **Every value must be typed.** One nothing types, or only a name guesses, could be anything,
///   and the answer would rest on the values that happened to be readable.
/// - **A value reading the accessor it is written to** is a loop, and refuses too. The general
///   depth bound would stop it anyway; [`Reads::writing`] stops it at the first turn.
///
/// Memoized for the request, by the reader and the asking document (whose fence decides what is
/// read).
fn written_to(
    sources: &Sources<'_>,
    uri_id: UriId,
    found: DeclarationId,
    owner: &Typed,
) -> Option<Typed> {
    let writes = writes_of(sources, uri_id, found, owner, Storage::ClassVariable)?;
    // No write anywhere: unseen code may write it, so nothing is said, not `nil`.
    if writes.values.is_empty() {
        return None;
    }
    let mut answer = fold_reached(sources.graph, writes.values, true)?;
    answer.derivation = written_derivation(sources, found, owner)?;
    Some(answer)
}

/// What a member returns where something else fills it first and the application may assign it
/// after: the rest of its declared union, joined with every value the writer is given
/// (`(ActiveSupport::BroadcastLogger | WrittenByItsWriter)`).
///
/// [`written_to`]'s reading, with the two differences a value that is always set makes: no write
/// at all is the rest alone, and `nil` joins only where a write passes it. One untyped or guessed
/// write still refuses.
fn written_beside(
    sources: &Sources<'_>,
    uri_id: UriId,
    found: DeclarationId,
    owner: &Typed,
    rest: &Returned,
) -> Option<Typed> {
    let derivation = written_derivation(sources, found, owner)?;
    let set = typed_return(sources, rest, owner, None, &derivation)?;
    let writes = writes_of(sources, uri_id, found, owner, Storage::InstanceVariable)?;
    let mut values = writes.values;
    values.insert(0, set);
    let mut answer = fold_reached(sources.graph, values, writes.nil)?;
    answer.derivation = derivation;
    Some(answer)
}

/// What a member a call declared from its lambda answers ([`Return::Scoped`]): the lambda's value
/// where it is truthy, and `rest` where it is `nil` or `false`, as `instance_exec(&body) || self`.
///
/// - **The lambda is found at the member's own place**, as [`made_from_block`] finds a block, and
///   read where it is written, so its own `[self: …]` decides `self` in it.
/// - **A lambda it cannot read is `rest` alone**, which is what the member said before its lambda
///   was read: no place, a lambda whose exit nothing types, or two places.
/// - **The `||` is [`shortcut`]'s** ([`reaching_end`]): a value that is never falsy stands alone,
///   and one that may be keeps its classes and `true` beside `rest`.
fn scoped_beside(
    sources: &Sources<'_>,
    found: DeclarationId,
    owner: &Typed,
    rest: &Returned,
) -> Option<Typed> {
    let graph = sources.graph;
    let derivation = written_derivation(sources, found, owner)?;
    let otherwise = typed_return(sources, rest, owner, None, &derivation)?;
    let lambda = || {
        let mut places: Vec<(String, u32)> = Vec::new();
        for definition in locator::definitions_of(graph, found) {
            if !matches!(definition, Definition::Method(_)) {
                continue;
            }
            let Origin::Declared(site) = sources
                .generated
                .origin(definition.uri_id(), definition.offset().start())
            else {
                return None;
            };
            places.push((site.uri.clone(), site.full.0));
        }
        places.sort();
        places.dedup();
        let [(uri, call)] = places.as_slice() else {
            return None;
        };
        let uri_id = UriId::from(uri.as_str());
        let document = sources.memo.reads.documents.of(uri, sources.read)?;
        let at = document
            .rebase
            .span_to_buffer(ByteSpan {
                start: *call,
                end: *call,
            })?
            .start;
        let exits = document.lambdas().get(&at)?;
        let scope = sources.scope_at(uri_id, *call);
        // **Read for the class the scope was called on** ([`Sources::object`]): written in a
        // concern's `included do`, the lambda's `self` is that class's relation, not every
        // includer's.
        // A class object is its class; a relation is its model's (`Story::Relation` holds `Story`).
        let there = Sources {
            object: owner.one().map(|on| {
                locator::attached_class(graph, on)
                    .or_else(|| {
                        let name = graph.declarations().get(&on)?.name().to_owned();
                        declared(graph, generated::element_of(&name)?)
                    })
                    .unwrap_or(on)
            }),
            ..*sources
        };
        let folds = Folds::of(graph);
        let mut join = Join::default();
        for exit in exits {
            let exit = exit.rebased(&document.rebase)?;
            join.add(method_receiver(&there, uri_id, &exit, &scope)?, &folds);
        }
        let mut value = join.finish(&folds)?;
        value.derivation.body = Some(FromBody {
            method: graph.declarations().get(&found)?.name().to_owned(),
            file: where_written(sources, uri),
            line: line_of(&document.source, at),
        });
        Some(value)
    };
    let Some(value) = lambda() else {
        return Some(otherwise);
    };
    let folds = Folds::of(graph);
    let side = Sides::of(&value, &folds);
    if !side.falsy() {
        return Some(value);
    }
    reaching_end(value, side, otherwise, false, &folds)
}

/// What a reader answers whose storage every instance of its class shares: every value its writer
/// is handed on the class or a descendant ([`Return::Shared`]).
///
/// [`written_to`]'s reading, with three differences storage shared by every instance makes:
///
/// - **Every receiver counts**, not only a constant or `self`: `config.x = y` in an initializer's
///   block and `Rails.configuration.x = y` fill the same storage. A receiver typed as the declaring
///   class or a descendant is a write; one typed as anything else is another object's writer.
/// - **A receiver nothing types, or only a guess, refuses**: it may be one of them.
/// - **`nil` joins only where a write passes it.** A read before any write raises
///   (`method_missing` calls `super`), so no call returns it.
fn shared_by(
    sources: &Sources<'_>,
    uri_id: UriId,
    found: DeclarationId,
    owner: &Typed,
) -> Option<Typed> {
    let writes = writes_of(sources, uri_id, found, owner, Storage::Shared)?;
    let mut answer = fold_reached(sources.graph, writes.values, writes.nil)?;
    answer.derivation = written_derivation(sources, found, owner)?;
    Some(answer)
}

/// The chain so far, with the accessor's own declaration named.
fn written_derivation(
    sources: &Sources<'_>,
    found: DeclarationId,
    owner: &Typed,
) -> Option<Derivation> {
    let mut derivation = owner.derivation.clone();
    derivation
        .signatures
        .push(sources.graph.declarations().get(&found)?.name().to_owned());
    Some(derivation)
}

/// Every value the application passes an accessor's writer on the same object, each typed.
#[derive(Debug, Clone)]
struct Writes {
    values: Vec<Typed>,
    /// Whether a write passes a literal `nil`, which is not among the values.
    nil: bool,
}

/// Where an accessor keeps its value, which decides what else can fill it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Storage {
    /// A plain `mattr_accessor`'s `@@name`: an `@@name` spelled anywhere, or a
    /// `class_variable_set` that can name it, fills it too ([`class_variable_written`]).
    ClassVariable,
    /// An `attr_accessor` on the object, as railties writes `Rails.logger`'s in `class << self`:
    /// no class variable reaches it.
    InstanceVariable,
    /// One store every instance of the declaring class reads, which the writer fills on any of
    /// them ([`shared_by`]): not a class variable of the accessor's name.
    Shared,
    /// A current attribute's: the receiver's class object and its one instance share it, and
    /// `set(name: value)` fills it too ([`current_held`]).
    Held,
}

/// [`Writes`] for one accessor, or `None` where one value cannot be typed or a loop comes back.
///
/// Memoized for the request, by the reader and the asking document (whose fence decides what is
/// read). One accessor has one kind of storage, so the storage is not part of the key.
fn writes_of(
    sources: &Sources<'_>,
    uri_id: UriId,
    found: DeclarationId,
    owner: &Typed,
    storage: Storage,
) -> Option<Writes> {
    let reads = &sources.memo.reads;
    // A current attribute's store is its receiver's class's own, so the reader alone is no key.
    let key = (
        found,
        uri_id,
        (storage == Storage::Held).then(|| owner.one()).flatten(),
    );
    if let Some(held) = reads.written.borrow().get(&key) {
        return held.clone();
    }
    if reads.writing.borrow().contains(&found) {
        return None;
    }
    reads.writing.borrow_mut().push(found);
    let answer = read_writes(sources, uri_id, found, owner, storage);
    reads.writing.borrow_mut().pop();
    reads.written.borrow_mut().insert(key, answer.clone());
    answer
}

/// [`writes_of`], unmemoized.
fn read_writes(
    sources: &Sources<'_>,
    uri_id: UriId,
    found: DeclarationId,
    owner: &Typed,
    storage: Storage,
) -> Option<Writes> {
    let graph = sources.graph;
    let reads = &sources.memo.reads;
    let declaration = graph.declarations().get(&found)?;
    // The same object for an accessor on one object; for a shared store, the class declaring it.
    let holder = match storage {
        Storage::Shared => DeclarationId::from(declaration.name().rsplit_once('#')?.0),
        Storage::Held => base_of(graph, owner.one()?)?,
        Storage::ClassVariable | Storage::InstanceVariable => owner.one()?,
    };
    let name = declaration
        .name()
        .rsplit_once('#')?
        .1
        .trim_end_matches("()");
    let cursor = graph.documents().get(&uri_id)?.uri().to_owned();
    let fence = environment::Fence::at(Some(&cursor), sources.layout);
    if storage == Storage::ClassVariable
        && class_variable_written(sources, &fence, declaration.name(), name)
    {
        return None;
    }
    // A template is Ruby too: `<% Current.account = … %>` is a write like any other. By path, so
    // the union reads the same on every run; an id is a hash. A current attribute's `set` calls
    // write it too.
    let setting = if storage == Storage::Held {
        graph.calls_named("set")
    } else {
        Rc::from([])
    };
    let mut documents: Vec<(&str, UriId)> = graph
        .calls_named(&format!("{name}="))
        .iter()
        .chain(setting.iter())
        .filter_map(|(document, _, _)| {
            let uri = graph.documents().get(document)?.uri();
            application(sources, &fence, uri).then_some((uri, *document))
        })
        .collect();
    documents.sort_unstable();
    documents.dedup();
    let mut typed: Vec<Typed> = Vec::new();
    let mut nil = false;
    for (uri, document) in documents {
        // A document that cannot be read may hold a write: nothing can be said.
        let read = reads.documents.of(uri, sources.read)?;
        for setter in &read.shapes(uri, sources.held_exits).setters {
            // `set(**options)` may write any name.
            let spread = setter.set && setter.name == "**";
            if (setter.set && storage != Storage::Held) || (!spread && setter.name != name) {
                continue;
            }
            let place = read.rebase.to_graph(setter.at)?;
            let on = setter.on.rebased(&read.rebase)?;
            let value = setter.value.rebased(&read.rebase)?;
            let scope = sources.scope_at(document, place);
            let on = method_receiver(sources, document, &on, &scope);
            let reaches = match storage {
                Storage::Shared => {
                    let on = on
                        .filter(|on| on.derivation.tier() != Tier::Guessed)?
                        .one()?;
                    on == holder || descends(graph, on, holder)
                }
                // The class object or its instance: `base_of` is the class for both, and a
                // subclass's is its own store.
                Storage::Held => {
                    on.and_then(|on| on.one()).and_then(|on| base_of(graph, on)) == Some(holder)
                }
                Storage::ClassVariable | Storage::InstanceVariable => {
                    on.and_then(|on| on.one()) == Some(holder)
                }
            };
            if !reaches {
                continue;
            }
            if spread {
                return None;
            }
            // A literal `nil` is said apart: an accessor holds it before any write anyway, and a
            // value that is always set holds it only after this one.
            if matches!(&value, Receiver::Literal { class, .. } if *class == "NilClass") {
                nil = true;
                continue;
            }
            let mut value = method_receiver(sources, document, &value, &scope)?;
            if value.derivation.tier() == Tier::Guessed {
                return None;
            }
            // Offsets into another file, which every consumer would draw against the asking one.
            value.derivation.assignments.clear();
            typed.push(value);
        }
    }
    if storage == Storage::Held && sent_to(sources, &fence, &format!("{name}="), holder)? {
        return None;
    }
    Some(Writes { values: typed, nil })
}

/// Whether the application's Ruby may send `writer` by name to the class object or the instance of
/// `holder`: a `send`, `public_send` or kin that can name it, on a receiver typed as
/// either. `None` where a document that may cannot be read.
fn sent_to(
    sources: &Sources<'_>,
    fence: &environment::Fence<'_>,
    writer: &str,
    holder: DeclarationId,
) -> Option<bool> {
    let graph = sources.graph;
    let reads = &sources.memo.reads;
    let mut documents: Vec<(&str, UriId)> = cursor::SENDERS
        .iter()
        .flat_map(|sender| {
            graph
                .calls_named(sender)
                .iter()
                .copied()
                .collect::<Vec<_>>()
        })
        .filter_map(|(document, _, _)| {
            let uri = graph.documents().get(&document)?.uri();
            application(sources, fence, uri).then_some((uri, document))
        })
        .collect();
    documents.sort_unstable();
    documents.dedup();
    for (uri, document) in documents {
        let read = reads.documents.of(uri, sources.read)?;
        for sent in &read.shapes(uri, sources.held_exits).sends {
            if !sent.may_write() || !sent.name.matches(writer) {
                continue;
            }
            let place = read.rebase.to_graph(sent.at)?;
            let scope = sources.scope_at(document, place);
            let on = method_receiver(sources, document, &sent.on.rebased(&read.rebase)?, &scope);
            if on.and_then(|on| on.one()).and_then(|on| base_of(graph, on)) == Some(holder) {
                return Some(true);
            }
        }
    }
    Some(false)
}

/// What a current attribute's reader hands back ([`Return::Held`]): every value its
/// writer, or `set(name: value)`, is given on the receiver's class object or its instance, with
/// `nil`, or beside `rest`, its literal default, which the store holds before any write.
///
/// [`written_to`]'s reading, with two differences: a receiver typed as the class object and one
/// typed as its instance write one store, and a writer the class `def`s itself runs its own body,
/// so the reader refuses.
fn current_held(
    sources: &Sources<'_>,
    uri_id: UriId,
    found: DeclarationId,
    owner: &Typed,
    rest: Option<&Returned>,
) -> Option<Typed> {
    let graph = sources.graph;
    let holder = base_of(graph, owner.one()?)?;
    let class = graph.declarations().get(&holder)?.name().to_owned();
    let name = graph
        .declarations()
        .get(&found)?
        .name()
        .rsplit_once('#')?
        .1
        .trim_end_matches("()")
        .to_owned();
    let hand_written = [class.clone(), singleton_name(&class)].iter().any(|owner| {
        let writer = DeclarationId::from(format!("{owner}#{name}=()").as_str());
        locator::definitions_of(graph, writer)
            .iter()
            .any(|definition| {
                matches!(definition, Definition::Method(_))
                    && graph
                        .documents()
                        .get(definition.uri_id())
                        .is_some_and(|document| writes_ruby(document.uri()))
            })
    });
    if hand_written {
        return None;
    }
    let derivation = written_derivation(sources, found, owner)?;
    let writes = writes_of(sources, uri_id, found, owner, Storage::Held)?;
    let mut values = writes.values;
    let nil = match rest {
        Some(rest) => {
            values.insert(0, typed_return(sources, rest, owner, None, &derivation)?);
            writes.nil
        }
        // No write anywhere: unseen code may write it, so nothing is said, not `nil`.
        None if values.is_empty() => return None,
        None => true,
    };
    let mut answer = fold_reached(graph, values, nil)?;
    answer.derivation = derivation;
    Some(answer)
}

/// Whether the holder's class variable of the accessor's name is filled by anything but the
/// writer: an `@@name` Ruby spells anywhere in the graph, or a `class_variable_set` in the
/// application's own Ruby that can name it.
///
/// A writer keeping a class variable is filled by those too, so its calls are not every value.
/// Where the writer keeps its value elsewhere, a class variable of the same name only makes this
/// refuse what it could have answered.
fn class_variable_written(
    sources: &Sources<'_>,
    fence: &environment::Fence<'_>,
    reader: &str,
    name: &str,
) -> bool {
    let graph = sources.graph;
    let holder = reader.rsplit_once('#').map_or(reader, |(owner, _)| owner);
    let holder = holder
        .rsplit_once("::<")
        .map_or(holder, |(namespace, _)| namespace);
    let variable = format!("@@{name}");
    if graph.declarations().contains_key(&DeclarationId::from(
        format!("{holder}#{variable}").as_str(),
    )) {
        return true;
    }
    graph
        .calls_named("class_variable_set")
        .iter()
        .filter_map(|(document, _, _)| graph.documents().get(document))
        .map(|document| document.uri())
        .filter(|uri| application(sources, fence, uri))
        .any(|uri| {
            sources
                .memo
                .reads
                .documents
                .of(uri, sources.read)
                .is_none_or(|read| {
                    scopes::class_variable_sets(&read.source)
                        .iter()
                        .any(|spelled| spelled.matches(&variable))
                })
        })
}

/// What an `attr_writer` or `attr_accessor` writes on a read's object: every value a call of the
/// writer passes where its receiver can be that object. `None` refuses the read.
///
/// - **Only a writer Ruby makes.** A `def name=` in any of the object's classes, or a signature's
///   (a column's writer), may run instead and write what its body says: the read refuses.
/// - **The calls are rubydex's** ([`Indexed::calls_named`]), in the application's own Ruby fenced
///   as the asking document is ([`application`]), templates included, as a written accessor's are
///   read. `x.name =`, `||=` and `&&=` pass their value; `+=` passes what `+` answers, which
///   nothing types.
/// - **A receiver counts where it can be the object**: one of its classes is a class or module
///   the object's classes are, descend from or include ([`Hierarchy::owners`]). A value it passes
///   joins the answer, which is wider where the call wrote another object of an ancestor, never
///   narrower. A receiver of other classes writes another object. One nothing types, or only a
///   guess, may be this one: the read refuses.
/// - **A call that can send the writer by name refuses** where its receiver can be the object
///   ([`cursor::Shapes::sends`]): any in the application's Ruby, and one on `self` in any file of
///   the object's classes that builds a writer's name (ActiveModel's `public_send(:"#{k}=", v)`,
///   which `new(attributes)` runs). A gem's forwarding (`try`'s `public_send(*args)`) sends what
///   its caller wrote, and the caller is read.
/// - **Every value must be typed**, and not by a guess; a literal `nil` is said apart.
/// - **A value reading the variable again** comes back here, and refuses ([`Reads::setting`]).
///
/// **Not read**: a gem calling the writer on an object it is handed, a multiple assignment's target
/// (`a.x, b = 1, 2`, which rubydex indexes as no call), `method(:x=)`, and a `delegate` of it.
fn setter_values(
    sources: &Sources<'_>,
    uri_id: UriId,
    hierarchies: &[Rc<Hierarchy>],
    name: &str,
) -> Option<Writes> {
    let reads = &sources.memo.reads;
    let key = (
        name.to_owned(),
        hierarchies
            .iter()
            .filter_map(|hierarchy| hierarchy.objects.first().copied())
            .collect::<Vec<_>>(),
    );
    if reads.setting.borrow().contains(&key) {
        return None;
    }
    reads.setting.borrow_mut().push(key);
    let answer = read_setter_values(sources, uri_id, hierarchies, name);
    reads.setting.borrow_mut().pop();
    answer
}

/// [`setter_values`], unguarded.
fn read_setter_values(
    sources: &Sources<'_>,
    uri_id: UriId,
    hierarchies: &[Rc<Hierarchy>],
    name: &str,
) -> Option<Writes> {
    let graph = sources.graph;
    let reads = &sources.memo.reads;
    let writer = format!("{name}=");
    for hierarchy in hierarchies {
        let side = if hierarchy.class_side {
            Side::Class
        } else {
            Side::Instance
        };
        for (owner, _) in hierarchy.owners.iter().filter(|(_, held)| *held == side) {
            let owner = graph.declarations().get(owner)?.name();
            let holder = if hierarchy.class_side {
                singleton_name(owner)
            } else {
                owner.to_owned()
            };
            let declared = DeclarationId::from(format!("{holder}#{writer}()").as_str());
            if locator::definitions_of(graph, declared)
                .iter()
                .any(|definition| {
                    !matches!(
                        definition,
                        Definition::AttrWriter(_) | Definition::AttrAccessor(_)
                    )
                })
            {
                return None;
            }
        }
    }
    // Whether a receiver can be the object: an instance of one of the objects' classes or of what
    // they descend from or include, or for a class object's variable, one of those classes.
    let reaches = |on: &Typed| {
        on.classes().iter().any(|class| {
            let key = match graph.declarations().get(class) {
                Some(Declaration::Namespace(Namespace::SingletonClass(_))) => {
                    base_of(graph, *class).map(|base| (base, Side::Class))
                }
                _ => Some((*class, Side::Instance)),
            };
            key.is_none_or(|key| {
                hierarchies
                    .iter()
                    .any(|hierarchy| hierarchy.owners.contains(&key))
            })
        })
    };
    // A receiver or value typed, and not by a guess, or `None`, with [`Reads::pending`] saying
    // whether it is only not known yet.
    let sure = |document: UriId, shape: &Receiver, scope: &Scope| -> Option<Typed> {
        let typed = pending_aware(sources, || method_receiver(sources, document, shape, scope))?;
        if typed.derivation.tier() == Tier::Guessed {
            reads.pending.set(false);
            return None;
        }
        Some(typed)
    };
    let cursor = graph.documents().get(&uri_id)?.uri().to_owned();
    let fence = environment::Fence::at(Some(&cursor), sources.layout);

    let mut documents: Vec<(&str, UriId)> = graph
        .calls_named(&writer)
        .iter()
        .filter_map(|(document, _, _)| {
            let uri = graph.documents().get(document)?.uri();
            application(sources, &fence, uri).then_some((uri, *document))
        })
        .collect();
    documents.sort_unstable();
    documents.dedup();
    let mut values: Vec<Typed> = Vec::new();
    let mut nil = false;
    for (uri, document) in documents {
        // A document that cannot be read may hold a write: nothing can be said.
        let read = reads.documents.of(uri, sources.read)?;
        for setter in &read.shapes(uri, sources.held_exits).setters {
            if setter.set || setter.name != name {
                continue;
            }
            let place = read.rebase.to_graph(setter.at)?;
            let scope = sources.scope_at(document, place);
            let on = sure(document, &setter.on.rebased(&read.rebase)?, &scope)?;
            if !reaches(&on) {
                continue;
            }
            let value = setter.value.rebased(&read.rebase)?;
            if matches!(&value, Receiver::Literal { class, .. } if *class == "NilClass") {
                nil = true;
                continue;
            }
            let mut value = sure(document, &value, &scope)?;
            // Offsets into another file, which every consumer would draw against the asking one.
            value.derivation.assignments.clear();
            values.push(value);
        }
    }

    let held: HashSet<UriId> = hierarchies
        .iter()
        .flat_map(|hierarchy| hierarchy.documents.iter().map(|(_, document)| *document))
        .collect();
    let mut senders: Vec<(&str, UriId, bool)> = cursor::SENDERS
        .iter()
        .flat_map(|sender| {
            graph
                .calls_named(sender)
                .iter()
                .copied()
                .collect::<Vec<_>>()
        })
        .filter_map(|(document, _, _)| {
            let uri = graph.documents().get(&document)?.uri();
            if application(sources, &fence, uri) {
                Some((uri, document, true))
            } else {
                held.contains(&document).then_some((uri, document, false))
            }
        })
        .collect();
    senders.sort_unstable();
    senders.dedup();
    for (uri, document, own) in senders {
        let read = reads.documents.of(uri, sources.read)?;
        for sent in &read.shapes(uri, sources.held_exits).sends {
            // Outside the application, only a writer's name built on `self` is read: forwarding
            // sends whatever its caller wrote.
            let counts =
                own || (sent.name.names_a_writer() && matches!(sent.on, Receiver::SelfObject(_)));
            if !counts || !sent.may_write() || !sent.name.matches(&writer) {
                continue;
            }
            let place = read.rebase.to_graph(sent.at)?;
            let scope = sources.scope_at(document, place);
            let on = sure(document, &sent.on.rebased(&read.rebase)?, &scope)?;
            if reaches(&on) {
                reads.pending.set(false);
                return None;
            }
        }
    }
    Some(Writes { values, nil })
}

/// The arms a call reaches ([`Arm::reached_by`]), with the names of the keywords it wrote.
fn arms_reached<'t>(sources: &Sources<'t>, found: DeclarationId, call: Call<'_>) -> Vec<&'t Arm> {
    sources
        .types
        .arms(found, call.block.written())
        .into_iter()
        .filter(|arm| arm.reached_by(call.arity, call.written.keywords))
        .collect()
}

/// The one arm a keyword call's **names** reach, where the partition by count could not tell.
///
/// `CSV.read(path, col_sep: ";")` writes no `headers:`, which the `CSV::Table` arm requires, so
/// only the `Array` arm runs. The partition is built before any call, so it cannot read names; this
/// only narrows what it refused, and adds no answer it gave.
fn only_arm_reached(
    sources: &Sources<'_>,
    found: DeclarationId,
    call: Call<'_>,
) -> Option<Returned> {
    match arms_reached(sources, found, call).as_slice() {
        [arm] => arm.returns.clone(),
        _ => None,
    }
}

/// The union of every arm a call of this many arguments reaches, where the arguments could not
/// pick one ([`pick_by_argument`]).
///
/// - **One of the arms runs**, so the call returns one of their answers. `Story.find(params[:id])`
///   is `Story | Array[Story]`: a request can send a list of ids. A call on the union then drops the
///   classes that lack the member ([`narrowed`]).
/// - **A keyword call joins too**. `CSV.read(path, headers: true)` reaches the
///   `CSV::Table` arm by its keyword and the `Array` arm by its options `Hash`, so it is either;
///   otherwise the body answered, reading `CSV#read` as `Array` whatever `headers:` said.
/// - **Only a count the call wrote exactly.** A splat could reach any arm.
/// - **Every reachable arm must answer.** An arm whose return the policy refuses (`untyped`, a
///   tuple, a union the table drops) may return anything, so the union would rest on the arms that
///   happened to be readable.
/// - **Two arms at least.** A single refused arm is the table's refusal, not a choice to join.
/// - **A block's own value is not read here** ([`Return::Block`] refuses): the arms disagree about
///   what the call is, so there is no one slot to put it in.
fn join_arms(
    sources: &Sources<'_>,
    found: DeclarationId,
    owner: &Typed,
    call: Call<'_>,
) -> Option<Typed> {
    if call.arity == Arity::Unknown {
        return None;
    }
    let reachable = arms_reached(sources, found, call);
    if reachable.len() < 2 {
        return None;
    }
    let graph = sources.graph;
    let folds = Folds::of(graph);
    let named = graph.declarations().get(&found)?.name().to_owned();
    let mut join = Join::default();
    let mut derivation = owner.derivation.clone();
    derivation.signatures.push(named);
    for arm in reachable {
        let returns = arm.returns.as_ref()?;
        join.add(
            typed_return(sources, returns, owner, None, &derivation)?,
            &folds,
        );
    }
    join.finish(&folds)
}

/// What a member call on a `bool` answers, where `TrueClass` and `FalseClass` declare it
/// differently.
///
/// `bool` carries one half and means both ([`Return::Bool`]). That is fine while both halves
/// declare the same names. It breaks when a gem reopens the pair: ActiveSupport declares `blank?`
/// on both with opposite bodies, and `"x".empty?.blank?` would answer `-> false` through
/// `TrueClass`'s override, for a receiver that is undetermined.
///
/// So the lookup runs on **both** halves and folds the answers:
///
/// - **The same declaration for both** (every member neither half overrides): `None`, so the
///   ordinary path runs and this costs one hash.
/// - **Two declarations agreeing on a class**: that class.
/// - **Two declarations naming the two halves of the pair**: `bool`, through the usual fold.
/// - **Anything else**, including one half answering alone: a union, which a call then runs on
///   class by class ([`narrowed`]), as on any other union.
///
/// Reached only where [`Typed::boolean`] is set, the unresolved `bool`. A literal `true` or `false`
/// carries its own class.
fn split_bool(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: &Typed,
    found: DeclarationId,
    call: Call<'_>,
    handed: Option<&Typed>,
) -> Option<Option<Typed>> {
    let graph = sources.graph;
    if !owner.boolean {
        return None;
    }
    let (arity, block) = (call.arity, call.block.written());
    let other = declared(graph, "FalseClass")?;
    let member = StringId::from(&call.member());
    let elsewhere = member_of(sources, uri_id, other, member)?;
    // Both halves declare it in one place, as for every name a gem does not reopen. Nothing to
    // fold; the caller's path runs untouched.
    if elsewhere == found {
        return None;
    }
    // The other half, resolved against a receiver that really is that half, so a `self` in its
    // signature answers `FalseClass`, not the carrier's class.
    let mut half = owner.clone();
    half.classes = vec![other];
    half.boolean = false;
    let mut carrier = owner.clone();
    carrier.boolean = false;
    let written = (arity, call.written.keywords);
    let here = returned_from(sources, found, carrier, written, block, handed, None, None)?;
    let there = returned_from(sources, elsewhere, half, written, block, handed, None, None)?;
    // The value depends on which half ran, so it is either answer ([`Join`]).
    either_half(graph, here, there).map(Some)
}

/// Which arm the call's **arguments** picked, where the arity partition could not tell the arms
/// apart.
///
/// **Why:** `vendor/rbs/core/integer.rbs` declares `Integer#+` four ways (`Integer`, `Float`,
/// `Rational`, `Complex`), and bigdecimal adds `(BigDecimal) -> BigDecimal`. Five arms, one arity,
/// five answers, so [`settle`]'s one-argument bucket refuses and `1 + 2` has no type. This rung
/// makes `1 + 2` an `Integer`.
///
/// # Why it is safe, in the order the checks run
///
/// 1. **Only where the partition already refused.** [`returned_by`] asks the table first and
///    reaches this only on `None`, so this rung can only *add* an answer.
/// 2. **The count must be known and match.** `Arity::Unknown` (a splat) picks nothing, and so does
///    a shape list that is not one per counted argument ([`Receiver::Returned`]'s "no claim").
/// 3. **Every reachable arm must be readable.** An arm with an optional or rest positional has no
///    parameter list, and a union or `untyped` parameter is `None`. Such an arm cannot be ruled
///    out, so the whole pick is abandoned.
/// 4. **Every argument must resolve to one class.** A union has no class to compare
///    ([`Typed::one`]'s rule).
/// 5. **Exactly one arm may match.** Two is the disagreement the partition refused; zero is a call
///    no arm describes. Both leave the refusal as it was.
///
/// A parameter is matched against the argument's **ancestry**, not its name. `Float#+` declares
/// `(Numeric) -> Float`, and `1.5 + 1` passes an `Integer`.
fn pick_by_argument(
    sources: &Sources<'_>,
    uri_id: UriId,
    found: DeclarationId,
    call: Call<'_>,
    scope: &Scope,
    owner: &Typed,
) -> Option<Returned> {
    let written = call.written.positional;
    let Arity::Exactly(counted) = call.arity else {
        return None;
    };
    let counted = counted as usize;
    // A call writing nothing has nothing to pick with, and a shape list that does not match the
    // count is the "no claim" an unreadable argument list files.
    if counted == 0 || written.len() != counted {
        return None;
    }
    let reachable = arms_reached(sources, found, call);
    // One arm cannot disagree with itself, so the refusal came from something this rung does not
    // answer for. An arm this cannot read is one it cannot **rule out**: an empty list is an arm
    // with an optional or rest positional, and a `None` is a parameter that is not a class. Either
    // abandons the whole pick, so the arms this happens to understand never decide alone.
    let unreadable =
        |arm: &&Arm| arm.takes.len() != counted || arm.takes.iter().any(Option::is_none);
    if reachable.len() < 2 || reachable.iter().any(unreadable) {
        return None;
    }
    let mut classes = Vec::with_capacity(counted);
    for argument in written {
        let typed = method_receiver(sources, uri_id, argument, scope)?;
        // **A guessed argument picks nothing.** The arm it picks answers with the *call's* tier,
        // so `1 + x` with `x` read off its spelling would be drawn as a fact about `Float#+`.
        if typed.derivation.tier() == Tier::Guessed {
            return None;
        }
        classes.push(typed.one()?);
    }
    let mut picked: Option<&Arm> = None;
    for arm in reachable {
        let fits = arm.takes.iter().zip(&classes).all(|(takes, held)| {
            takes.as_ref().is_some_and(|takes| {
                resolved(sources, &takes.of, owner, None)
                    .is_some_and(|wanted| inherits(sources.graph, *held, wanted))
            })
        });
        if !fits {
            continue;
        }
        // A second fitting arm is the disagreement the partition already refused.
        if picked.is_some() {
            return None;
        }
        picked = Some(arm);
    }
    picked?.returns.clone()
}

/// Which arm a call's **symbol arguments** picked, where a signature names the symbols it takes:
/// `def pick: (:depth) -> Integer? | (:body) -> String? | (*untyped) -> untyped`.
///
/// [`pick_by_argument`] cannot: each arm takes a `Symbol`, every symbol fits every one, and the
/// catch-all has a rest parameter it cannot rule out. RBS tries an overload set in order and the
/// first arm the call fits answers, which is what this reads.
///
/// - **Every argument at a position an arm names a symbol at must be a symbol literal** with its
///   name (`Receiver::Literal::symbol`). A variable holding one could be any symbol. The rest take
///   anything: an arm names symbols only where every later positional is `untyped` ([`Arm::symbols`]),
///   so `create(:user, :admin, name: "x")` reaches `(:user, *untyped, **untyped)`. A
///   required `untyped` between them takes anything too, so `create_list(:user, 3)` reaches
///   `(:user, untyped amount, *untyped, **untyped)`.
/// - **In written order, one document's arms only** ([`Types::arms_in_order`]): two documents'
///   arms have no order between them.
/// - **An arm written as those symbols answers.** An arm naming other symbols is skipped: the call
///   does not fit it.
/// - **Any other reachable arm before a match refuses**: a `(Symbol)` or a rest parameter takes
///   the call too, and it is tried first.
/// - **Asked before the partition**: an arm it picks is one the partition reached,
///   so it narrows what those arms agreed on and never contradicts it. `pluck(:depth)`'s arms all
///   return an `Array`, which the partition keeps without its element; the picked arm says
///   `Array[Integer]`.
fn pick_by_literal(
    sources: &Sources<'_>,
    found: DeclarationId,
    call: Call<'_>,
) -> Option<Returned> {
    let (Arity::Exactly(counted) | Arity::Keyed(counted) | Arity::Spread(counted)) = call.arity
    else {
        return None;
    };
    let written = call.written.positional;
    if counted == 0 || written.len() != counted as usize {
        return None;
    }
    let arms = sources.types.arms_in_order(found, call.block.written())?;
    for arm in arms
        .iter()
        .filter(|arm| arm.reached_by(call.arity, call.written.keywords))
    {
        // An arm naming no symbol takes the call first, and so does one with a position of
        // another type: whether the argument there fits is not known.
        if !arm
            .symbols
            .iter()
            .any(|slot| matches!(slot, LiteralSlot::Symbol(_)))
            || arm.symbols.contains(&LiteralSlot::Other)
        {
            return None;
        }
        let mut named = true;
        for (declared, argument) in arm.symbols.iter().zip(written) {
            // `untyped` takes whatever is written there.
            let LiteralSlot::Symbol(declared) = declared else {
                continue;
            };
            let Receiver::Literal {
                symbol: Some(argument),
                ..
            } = argument
            else {
                // A value that is not a symbol literal rules no arm out.
                return None;
            };
            named &= **declared == **argument;
        }
        if named {
            return arm.returns.clone();
        }
    }
    None
}

/// Whether `held` **is** `wanted` or has it among its ancestors.
///
/// rubydex already linearized the chain, so this scans its list (the one `hierarchy::supertypes`
/// prints). An `Ancestor::Partial` is an unresolved name and is skipped, not a stop: a missing link
/// can only cost a match, and this question may answer `false`, unlike [`from_super`], which must
/// refuse.
fn inherits(graph: &Graph, held: DeclarationId, wanted: DeclarationId) -> bool {
    if held == wanted {
        return true;
    }
    graph
        .declarations()
        .get(&held)
        .and_then(Declaration::as_namespace)
        .is_some_and(|namespace| {
            namespace
                .ancestors()
                .iter()
                .any(|ancestor| matches!(ancestor, Ancestor::Complete(id) if *id == wanted))
        })
}

/// Whether a declared return names an argument anywhere in it ([`Return::Argument`]): the free
/// gate on typing one.
fn names_an_argument(of: &Return) -> bool {
    match of {
        Return::Argument { .. } => true,
        Return::Class { arguments, .. } => arguments
            .iter()
            .any(|argument| argument.as_ref().is_some_and(names_an_argument)),
        Return::Union(members) => members.iter().any(names_an_argument),
        _ => false,
    }
}

/// The declared return with each [`Return::Argument`] replaced by the type the call passed there,
/// or `None` where one cannot be.
///
/// - **Only positions the call wrote.** A splat or an argument list the cursor cannot read files
///   none ([`Receiver::Returned`]'s "no claim"), and a keyword hash a method takes as one more
///   positional is not in the list, so a variable at its position binds nothing.
/// - **Only a type the argument has**, resolved or derived. One nothing types, or only a guess
///   does, is no class to put there, and the call keeps answering nothing.
/// - **The whole of the return** carries the argument's `nil` and every class of a union: `X` bound
///   to an `Integer?` makes `(String | X)` a `(String | Integer)?`. **A position in a class**
///   (`Enumerator[U]`) holds only one class, as every position does, and is left unknown otherwise.
fn bound_to_arguments(
    sources: &Sources<'_>,
    uri_id: UriId,
    returns: &Returned,
    call: Call<'_>,
    scope: &Scope,
) -> Option<Returned> {
    let written = call.written.positional;
    let mut typed: Vec<Option<Option<Typed>>> = vec![None; written.len()];
    let mut argument = |at: usize| -> Option<Typed> {
        typed
            .get_mut(at)?
            .get_or_insert_with(|| {
                method_receiver(sources, uri_id, &written[at], scope)
                    .filter(|typed| typed.derivation.tier() != Tier::Guessed)
            })
            .clone()
    };
    let mut nilable = returns.nilable;
    let of = bind(sources.graph, &returns.of, &mut argument, &mut nilable)?;
    Some(Returned { of, nilable })
}

/// [`bound_to_arguments`]' walk over one return.
fn bind(
    graph: &Graph,
    of: &Return,
    argument: &mut dyn FnMut(usize) -> Option<Typed>,
    nilable: &mut bool,
) -> Option<Return> {
    Some(match of {
        Return::Argument { at } => {
            let passed = argument(*at)?;
            *nilable |= passed.nilable;
            as_return(graph, &passed)?
        }
        Return::Union(members) => Return::Union(
            members
                .iter()
                .map(|member| bind(graph, member, argument, nilable))
                .collect::<Option<_>>()?,
        ),
        Return::Class {
            name,
            scope,
            arguments,
        } => Return::Class {
            name: name.clone(),
            scope: scope.clone(),
            arguments: arguments
                .iter()
                .map(|held| match held {
                    Some(Return::Argument { at }) => argument(*at)
                        .filter(|passed| !passed.nilable && !passed.boolean)
                        .and_then(|passed| {
                            Some(Return::class(
                                graph.declarations().get(&passed.one()?)?.name(),
                            ))
                        }),
                    held => held.clone(),
                })
                .collect(),
        },
        other => other.clone(),
    })
}

/// A value's type as a declared return, so a signature's `X` can be spelled with it.
///
/// `bool` is the pair; a union is each class; one class keeps what it holds. `nil` is the
/// caller's to carry: it rides beside a [`Return`], never in it.
fn as_return(graph: &Graph, typed: &Typed) -> Option<Return> {
    if typed.boolean {
        return Some(Return::Bool);
    }
    let name = |id: &DeclarationId| -> Option<Box<str>> {
        Some(graph.declarations().get(id)?.name().into())
    };
    let mut classes: Vec<Return> = typed
        .classes()
        .iter()
        .map(|id| Some(Return::class(&name(id)?)))
        .collect::<Option<_>>()?;
    match classes.len() {
        0 => None,
        1 => {
            let Some(Return::Class {
                name: head, scope, ..
            }) = classes.pop()
            else {
                return None;
            };
            Some(Return::Class {
                name: head,
                scope,
                arguments: typed
                    .arguments
                    .iter()
                    .map(|held| Some(Return::class(&name(held.as_ref()?)?)))
                    .collect(),
            })
        }
        _ => Some(Return::Union(classes.into_boxed_slice())),
    }
}

/// Whether a declared return has a [`Return::Block`] anywhere in it.
///
/// The free gate in [`returned_by`]: almost no return has a block substitution, and walking a block
/// to find that out would make `each` pay for `map`.
fn substitutes_a_block(of: &Return) -> bool {
    match of {
        Return::Block => true,
        Return::Class { arguments, .. } => arguments
            .iter()
            .any(|argument| argument.as_ref().is_some_and(substitutes_a_block)),
        Return::Union(members) => members.iter().any(substitutes_a_block),
        _ => false,
    }
}

/// What the block a call was written with returns, as one type.
///
/// [`body_return`]'s rule applied to a block, and much shorter. The exits were parsed from the
/// buffer the cursor is in, so there is no span to rebase, no document to read and no depth to
/// bound; `cursor::Finder`'s budget already bounded them.
///
/// - **Every exit is joined as in a body** ([`Join`]), and the value must be **one class**, since
///   it fills a type argument: `map { |r| r.blank? ? nil : r.name }` is `String?`;
///   `map { |r| r.admin? ? r : r.name }` is nothing.
/// - **One untyped exit declines the whole block.** Half a block is not evidence.
/// - **Every exit being `nil` is an answer.** `[1, 2].map { }` really is `[nil, nil]`, and a label
///   saying so beats no label.
/// - **A guessed exit declines the block.** The value fills a type argument of the *call's*
///   answer, which carries the call's tier, so a guess here would be drawn as fact:
///   `[1].map { |v| prep(v) }` as `Array[Ledger]` because a local was spelled `ledger`.
fn block_return(
    sources: &Sources<'_>,
    uri_id: UriId,
    block: &Block,
    scope: &Scope,
) -> Option<Typed> {
    // Every exit `nil` is `NilClass` itself, not a `?` on it: the mark means *may be `nil`*, and a
    // position filled by a marked value is not held. Every exit raising is no value at all, and a
    // union is no one class for a position to hold.
    let joined = block_value(sources, uri_id, block, scope)?;
    joined.one()?;
    Some(joined)
}

/// What a written block hands back, whichever of its values ran: its tail and every `next`,
/// typed where the call is written. `None` where any is untyped or only guessed, or none can be
/// read (a forwarded `&blk`, a `&:name`).
fn block_value(
    sources: &Sources<'_>,
    uri_id: UriId,
    block: &Block,
    scope: &Scope,
) -> Option<Typed> {
    let exits = block.exits();
    if exits.is_empty() {
        return None;
    }
    let folds = Folds::of(sources.graph);
    let mut join = Join::default();
    for exit in exits {
        if unreached(sources, uri_id, exit) {
            continue;
        }
        let typed = method_receiver(sources, uri_id, exit, scope)?;
        if typed.derivation.tier() == Tier::Guessed {
            return None;
        }
        join.add(typed, &folds);
    }
    join.finish(&folds)
}

/// A `&:name` block's value, for a signature's `[U]`: `name` called on what the block is handed
///. One class or nothing, as [`block_return`] answers.
fn symbol_return(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: &Typed,
    found: DeclarationId,
    name: &str,
    scope: &Scope,
) -> Option<Typed> {
    let handed = handed_to(sources, owner, found, 0)?;
    let joined = called_by_name(sources, uri_id, handed, name, scope)?;
    joined.one()?;
    Some(joined)
}

/// A proc passed as the block (`&fmt`), for a signature's `[U]`: its literals read with what the
/// signature says the block is handed. One class or nothing, as [`block_return`]
/// answers.
fn passed_return(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: &Typed,
    found: DeclarationId,
    value: &Receiver,
    scope: &Scope,
) -> Option<Typed> {
    let passed = method_receiver(sources, uri_id, value, scope)?;
    if let Some(bound) = passed.bound.as_deref() {
        return bound_return(sources, uri_id, found, bound, scope);
    }
    let procs = passed.procs?;
    let handed: Vec<Option<Typed>> = (0..MAX_HANDED)
        .map_while(|index| {
            sources
                .types
                .yielded(found, index)
                .map(|_| handed_to(sources, owner, found, index))
        })
        .collect();
    let joined = proc_value(sources, &procs, &handed)?;
    joined.one()?;
    Some(joined)
}

/// How many values a signature's block parameters are read for, at most ([`passed_return`]): a
/// sanity guard far above any block RBS declares.
const MAX_HANDED: usize = 16;

/// What `Symbol#to_proc` makes of a value: `name` called on it, publicly, with nothing else
/// passed. A value that may be `nil` or is only guessed answers nothing.
fn called_by_name(
    sources: &Sources<'_>,
    uri_id: UriId,
    on: Typed,
    name: &str,
    scope: &Scope,
) -> Option<Typed> {
    if on.nilable || on.derivation.tier() == Tier::Guessed {
        return None;
    }
    returned_on(
        sources,
        uri_id,
        on,
        Call {
            method: name,
            arity: Arity::Exactly(0),
            block: &Block::None,
            written: Called {
                positional: &[],
                keywords: Some(&[]),
            },
            safe: false,
            on_self: false,
        },
        scope,
    )
}

/// A call whose block `break`s: what the call returns, or any `break`'s value, whichever ran
///. `None` where any of them is untyped or only guessed: a guess never becomes part of
/// another type.
fn broken(
    sources: &Sources<'_>,
    uri_id: UriId,
    answer: Typed,
    breaks: &[Receiver],
    scope: &Scope,
) -> Option<Typed> {
    if answer.derivation.tier() == Tier::Guessed {
        return None;
    }
    let folds = Folds::of(sources.graph);
    let mut join = Join::default();
    join.add(answer, &folds);
    for value in breaks {
        let typed = method_receiver(sources, uri_id, value, scope)?;
        if typed.derivation.tier() == Tier::Guessed {
            return None;
        }
        join.add(typed, &folds);
    }
    join.finish(&folds)
}

/// What the block of the call a body is read for hands back, at one `yield` in that body
///.
///
/// - **Only inside a body read for one call** ([`Sources::bound`]), and only for the method the
///   `yield` is written in, found as [`from_parameter`] finds a parameter's. The `def`'s own label
///   has no call, so no block, and answers nothing.
/// - **A written block is its value at the call** ([`Handed::Value`]), typed there.
/// - **`&:name` is `name` called on what this `yield` hands over**, exactly one value.
/// - **No block, a forwarded one, or one whose value is unknown answers nothing**: a `yield` with
///   no block raises, and the rest cannot be named.
fn yielded_to_block(
    sources: &Sources<'_>,
    uri_id: UriId,
    at: u32,
    method: &str,
    arguments: &[Receiver],
    scope: &Scope,
) -> Option<Typed> {
    let graph = sources.graph;
    let here = sources.scope_at(uri_id, at).caller(graph)?;
    let found = member_of(
        sources,
        uri_id,
        here,
        StringId::from(&format!("{method}()")),
    )?;
    let bound = sources.bound.and_then(|key| {
        sources
            .memo
            .reads
            .bindings
            .borrow()
            .get(&key)
            .filter(|bound| bound.method == found)
            .cloned()
    })?;
    match &bound.handed {
        Handed::Nothing => None,
        Handed::Value(value) => Some((**value).clone()),
        Handed::Symbol(name) => {
            let [first] = arguments else {
                return None;
            };
            let on = method_receiver(sources, uri_id, first, scope)?;
            called_by_name(sources, uri_id, on, name, scope)
        }
        // What this `yield` hands over, typed here; the procs read where they were passed.
        Handed::Procs {
            procs,
            object,
            bound,
            made,
        } => {
            let handed = typed_values(sources, uri_id, arguments, scope);
            let there = Sources {
                object: *object,
                bound: *bound,
                made: *made,
                ..*sources
            };
            proc_value(&there, procs, &handed)
        }
    }
}

/// Each value, typed where it is written; `None` for one that is untyped or only guessed.
fn typed_values(
    sources: &Sources<'_>,
    uri_id: UriId,
    values: &[Receiver],
    scope: &Scope,
) -> Vec<Option<Typed>> {
    values
        .iter()
        .map(|value| {
            method_receiver(sources, uri_id, value, scope)
                .filter(|typed| typed.derivation.tier() != Tier::Guessed)
        })
        .collect()
}

/// What one call of a proc or lambda literal passed: its positional parameters, bound, and the key
/// the body it is read under was read under before ([`proc_value`]).
#[derive(Debug)]
pub struct ProcBinding {
    /// The literal: its document, and where it starts.
    at: (UriId, u32),
    /// Each positional parameter, by position; `None` where nothing typed it.
    positional: Vec<Option<Typed>>,
    /// The key in force where the literal was called, so an outer literal's parameters stay bound
    /// inside an inner one.
    outer: Option<u64>,
}

/// What a proc literal's parameter holds in the call of it being read ([`Sources::bound`]), found
/// by walking out from the innermost literal read; `None` outside any.
fn proc_parameter(sources: &Sources<'_>, uri_id: UriId, at: u32, index: usize) -> Option<Typed> {
    let reads = &sources.memo.reads;
    let mut key = sources.bound?;
    loop {
        let binding = reads.proc_bindings.borrow().get(&key).cloned()?;
        if binding.at == (uri_id, at) {
            return binding.positional.get(index)?.clone();
        }
        key = binding.outer?;
    }
}

/// Whether a `lambda { }`, `proc { }` or `Proc.new { }` was answered by Ruby's own method, so the
/// `Proc` is the block written there ([`Receiver::Proc`]).
///
/// `Proc.new` is an instance of the class it names, as any `X.new` is. `lambda` and `proc` must
/// have been answered by `Kernel`'s signature alone: a class that defines its own `proc` decides
/// what it hands back.
fn made_by_ruby(call: &Receiver, typed: &Typed) -> bool {
    matches!(call, Receiver::Instance { .. })
        || (typed.derivation.body.is_none()
            && matches!(
                typed.derivation.signatures.as_slice(),
                [only] if only == "Kernel#lambda()" || only == "Kernel#proc()"
            ))
}

/// A call of a proc literal: `fmt.call(a)`, `fmt.(a)`, `fmt[a]`, `fmt.yield(a)`.
///
/// `None` where this is no such call, or the receiver is not only proc literals
/// ([`Typed::procs`]): the call rung answers as before. `Some` otherwise, with what the literals
/// hand back for these arguments, or nothing where keywords or an uncountable list were written.
#[allow(clippy::option_option)]
fn called_proc(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: &Typed,
    call: Call<'_>,
    scope: &Scope,
) -> Option<Option<Typed>> {
    if !matches!(call.method, "call" | "yield" | "[]") {
        return None;
    }
    let procs = owner.procs.clone()?;
    let Arity::Exactly(counted) = call.arity else {
        return Some(None);
    };
    if call.written.positional.len() != counted as usize
        || call
            .written
            .keywords
            .is_some_and(|written| !written.is_empty())
    {
        return Some(None);
    }
    let handed = typed_values(sources, uri_id, call.written.positional, scope);
    let value = proc_value(sources, &procs, &handed);
    // `fmt&.call(a)` on a `fmt` that may be `nil` answers `nil` there; without `&.`, `nil.call`
    // raises and adds nothing.
    Some(if call.safe && owner.nilable {
        value.map(Typed::or_nil)
    } else {
        value
    })
}

/// What proc or lambda literals hand back for one call's positional values, whichever ran
///.
///
/// - **Bound as Ruby binds**: a lambda refuses a count its parameters do not take; a proc gives a
///   missing required parameter `nil`, a missing optional one its default, drops extras, and
///   refuses one value it would unpack ([`cursor::ProcParameters::spreads`]).
/// - **Read under a key of its own** ([`ProcBinding`]), combined with the key in force, so the
///   enclosing method's parameters stay bound and every memo keeps the calls apart.
/// - **Refused** where a literal cannot be found or bound, or any value is untyped or guessed.
fn proc_value(
    sources: &Sources<'_>,
    procs: &[(UriId, u32)],
    handed: &[Option<Typed>],
) -> Option<Typed> {
    use std::hash::{Hash, Hasher};
    if sources.body_hops >= BODY_HOPS {
        return None;
    }
    let graph = sources.graph;
    let reads = &sources.memo.reads;
    let folds = Folds::of(graph);
    let mut join = Join::default();
    for &(uri_id, at) in procs {
        let uri = graph.documents().get(&uri_id)?.uri().to_owned();
        let document = reads.documents.of(&uri, sources.read)?;
        let start = document
            .rebase
            .span_to_buffer(ByteSpan { start: at, end: at })?
            .start;
        let shape = document
            .shapes(&uri, sources.held_exits)
            .procs
            .get(&start)?
            .clone();
        let parameters = shape.parameters.as_ref()?;
        let positions = parameters.required + parameters.defaults.len();
        if shape.lambda {
            if handed.len() < parameters.required || (!parameters.rest && handed.len() > positions)
            {
                return None;
            }
        } else if parameters.spreads && handed.len() == 1 {
            return None;
        }
        let scope = sources.scope_at(uri_id, at);
        let mut positional = Vec::with_capacity(positions);
        for index in 0..positions {
            positional.push(match handed.get(index) {
                Some(value) => value.clone(),
                None if index < parameters.required => Some(Typed::of(
                    declared(graph, "NilClass")?,
                    Derivation::default(),
                )),
                None => {
                    let default = parameters.defaults[index - parameters.required]
                        .rebased(&document.rebase)?;
                    method_receiver(sources, uri_id, &default, &scope)
                }
            });
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (sources.bound, uri_id, at).hash(&mut hasher);
        for value in &positional {
            value
                .as_ref()
                .map(|typed| {
                    (
                        &typed.classes,
                        typed.nilable,
                        typed.boolean,
                        &typed.arguments,
                    )
                })
                .hash(&mut hasher);
        }
        let key = hasher.finish().max(1);
        reads
            .proc_bindings
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| {
                Rc::new(ProcBinding {
                    at: (uri_id, at),
                    positional,
                    outer: sources.bound,
                })
            });
        // The enclosing method's binding, if a body is being read for one, stays in force.
        let outer = sources
            .bound
            .and_then(|bound| reads.bindings.borrow().get(&bound).cloned());
        if let Some(outer) = outer {
            reads.bindings.borrow_mut().entry(key).or_insert(outer);
        }
        let deeper = Sources {
            bound: Some(key),
            body_hops: sources.body_hops + 1,
            ..*sources
        };
        for exit in &shape.exits {
            let exit = exit.rebased(&document.rebase)?;
            if unreached(&deeper, uri_id, &exit) {
                continue;
            }
            let typed = method_receiver(&deeper, uri_id, &exit, &scope)?;
            if typed.derivation.tier() == Tier::Guessed {
                return None;
            }
            join.add(typed, &folds);
        }
    }
    join.finish(&folds)
}

/// The declaration one declared type names, against the receiver the call was written on.
///
/// **The one place a [`Return`] becomes a class.** Three readers need the same answers: what a call
/// returns, what it hands a block, and what a generic in either holds. If they spelled it
/// differently, a receiver's element could disagree with its own chain. [`returned_from`] and
/// [`yielded_by`] ask once for the whole type; [`held_by_return`] asks once per position.
fn resolved(
    sources: &Sources<'_>,
    of: &Return,
    owner: &Typed,
    handed: Option<&Typed>,
) -> Option<DeclarationId> {
    let graph = sources.graph;
    match of {
        Return::Class { name, scope, .. } => declared_in(graph, name, scope),
        // `bool` is the pair, and `TrueClass` is the half that carries the members
        // ([`Return::Bool`]). With `[rbs]` off neither half is declared, and the method falls to
        // the rung below.
        Return::Bool => declared(graph, "TrueClass"),
        // `self` is what the call was written on, which is what `owner` already is.
        Return::Same => owner.one(),
        // **Both receiver-relative returns are ActiveRecord's alone**, so one guard covers both.
        // With the generator off the name would not resolve anyway; the guard makes
        // `Story.where(...)` fall to the next rung on purpose, not by accident.
        Return::Element | Return::Collection if !sources.features.models => None,
        Return::Element => declared(graph, &model_of(graph, owner.one()?)?),
        // A grouped relation's chain stays grouped: its calculations are by group.
        Return::Collection => {
            let receiver = owner.one()?;
            match graph.declarations().get(&receiver) {
                Some(found) if generated::is_grouped(found.name()) => Some(receiver),
                _ => declared(
                    graph,
                    &generated::collection_of(&model_of(graph, receiver)?),
                ),
            }
        }
        // The receiver's own type argument. Usually unknown, but `[1, 2]` knows it (the element is
        // in the source), and so does anything a call already handed an argument to. `rows.first`
        // is nothing, exactly as `Array#first`'s `() -> E` says. [`Typed::argument`] holds the
        // safety rule.
        Return::Parameter { at, of } => owner.argument(of, *at),
        // The one answer from neither the receiver nor the signature: the block wrote it and
        // [`block_return`] resolved it. [`returned_by`] is the only caller with a block to hand
        // over; see [`Return::Block`].
        Return::Block => handed?.one(),
        // Several classes are no one declaration; see [`typed_return`].
        Return::Union(_) => None,
        // What a writer was given is read at the call, where the call is; see [`written_to`].
        // So is the call a member hands on; see [`forwarded`]. An argument is put in its place
        // before this is asked ([`bound_to_arguments`]), so one still here was not identified.
        Return::Written
        | Return::Shared
        | Return::Held
        | Return::Scoped
        | Return::Forwarded { .. }
        | Return::Argument { .. } => None,
    }
}

/// What a declared return is at a call, as a [`Typed`]: [`resolved`] and [`held_by_return`] for one
/// class, and each member of a [`Return::Union`] that way, joined.
///
/// `derivation` is the chain so far with the signature already named. A member that does not
/// resolve refuses the whole union, for [`union_of`]'s reason.
fn typed_return(
    sources: &Sources<'_>,
    returns: &Returned,
    owner: &Typed,
    handed: Option<&Typed>,
    derivation: &Derivation,
) -> Option<Typed> {
    let Return::Union(members) = &returns.of else {
        let declaration = resolved(sources, &returns.of, owner, handed)?;
        return Some(
            Typed::of(declaration, derivation.clone())
                .faceted(returns)
                .holding(held_by_return(sources, &returns.of, owner, handed)),
        );
    };
    let folds = Folds::of(sources.graph);
    let mut join = Join::default();
    for member in members {
        let declaration = resolved(sources, member, owner, handed)?;
        join.add(
            Typed::of(declaration, derivation.clone())
                .faceted(&Returned::plain(member.clone()))
                .holding(held_by_return(sources, member, owner, handed)),
            &folds,
        );
    }
    let mut joined = join.finish(&folds)?;
    joined.nilable |= returns.nilable;
    Some(joined)
}

/// What the class a call returned is itself written holding.
///
/// - **This carries the element across a call.** A method declared `-> Array[String]` returns an
///   `Array` that still says what it holds, so `"a,b".split(",").each { |part| ... }` and
///   `[1, 2].first(2).each { |n| ... }` answer like `["a"].each { |part| ... }`. It also carries
///   the query interface's `Array[<element>]` through `to_a` into the block below.
/// - **`self` passes the receiver's arguments through unchanged.**
///   `[1, 2].each { ... }.each { |n| ... }` is the same array twice; dropping them would make a
///   no-op step lose the element.
/// - **Every other answer holds nothing.** `bool` and the two sentinels have no positions. One
///   level only: `Array[Array[String]]` returns an `Array`, and what *that* holds is never asked.
fn held_by_return(
    sources: &Sources<'_>,
    of: &Return,
    owner: &Typed,
    handed: Option<&Typed>,
) -> Vec<Option<DeclarationId>> {
    match of {
        Return::Class { arguments, .. } => arguments
            .iter()
            .map(|argument| {
                let argument = argument.as_ref()?;
                // A block value filling a position is held as a bare class, so one that may be
                // `nil` or is `bool` is not held: `map { |r| r.name }` over a `String?` is an
                // `Array` whose elements may be `nil`, not an `Array[String]`.
                if matches!(argument, Return::Block)
                    && handed.is_some_and(|held| held.nilable || held.boolean)
                {
                    return None;
                }
                resolved(sources, argument, owner, handed)
            })
            .collect(),
        Return::Same => owner.arguments.clone(),
        // `File.open(path) { |f| f.readlines }` returns what the block did, arguments and all. The
        // block's value *is* the call's, so nothing is dropped between them.
        Return::Block => handed
            .map(|held| held.arguments.clone())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// What a member returns, once the member has been found.
///
/// The second half of [`returned_by`], split off because [`from_super`] finds the member
/// differently and must read the answer the same way. Above the split: the receiver, the name, and
/// rubydex's ancestor walk. Below it: one signature, one declaration, one note naming the
/// signature.
#[allow(clippy::too_many_arguments)]
fn returned_from(
    sources: &Sources<'_>,
    found: DeclarationId,
    owner: Typed,
    (arity, keywords): (Arity, Option<&[(String, Receiver)]>),
    block: bool,
    handed: Option<&Typed>,
    picked: Option<Returned>,
    passed: Option<&Passed>,
) -> Option<Typed> {
    let graph = sources.graph;
    // `picked` first: the arm the arguments picked where the partition said nothing, or the
    // declared return with the arguments it names put in place ([`bound_to_arguments`]). Both ran
    // in [`returned_for`], where the arguments are.
    let returns = match picked.as_ref() {
        Some(picked) => picked,
        None => match sources.types.returns_to(found, arity, block, keywords) {
            Some(returns) => returns,
            // **The seam.** Nothing declares what this method returns, which is true of every `def`
            // an application writes. This is where `order.customer.name` would stop; [`from_body`]
            // decides whether it does.
            None => return from_body(sources, found, owner, passed),
        },
    };
    // Looked up once and read twice (the gate and the note), which keeps the `?` in one place.
    let named = graph.declarations().get(&found)?;
    // Named by the declaration rubydex found, not by what was written: that is the signature the
    // answer came from. `[].tap` says `Kernel#tap()`, which is where a reader would go to check.
    let mut derivation = owner.derivation.clone();
    derivation.signatures.push(named.name().to_owned());
    let mut declared = typed_return(sources, returns, &owner, handed, &derivation)?;
    // **Read before `owner` is consumed, and only when the cheap gate says so** (see
    // [`disputed_by_bodies`]). The clone copies a class list and provenance strings, which no typed
    // call should pay for.
    let body = disputed_by_bodies(graph, named)
        .then(|| from_body(sources, found, owner.clone(), None))
        .flatten();

    // **The block's value is the answer whole**, where the return is the block's own `U`
    // (`then`): a block answering `String?` makes the call a `String?`, and one answering `bool`
    // a `bool`.
    if matches!(returns.of, Return::Block)
        && let Some(held) = handed
    {
        declared.nilable |= held.nilable;
        declared.boolean |= held.boolean;
    }
    Some(match body {
        Some(body) => disputed(graph, declared, body),
        None => declared,
    })
}

/// Whether more than one real Ruby `def` answers to this declaration.
///
/// - **The gate on [`disputed`], and the whole cost argument.** A method with one body is what its
///   signature describes, as for every method a `sig/` or `.gem_rbs_collection` covers. Those keep
///   their answer and pay one length check. Two bodies is the shape a signature cannot describe
///   both of.
/// - **`.rbs` and generated definitions are not counted**, as in [`body_return`]: an RBS `def:`
///   line is a `Definition::Method` like a real `def`, so counting it would call a method disputed
///   by its own signature.
/// - **A `Definition::MethodAlias` is skipped.** An alias has no body, so it can neither dispute a
///   signature nor be disputed.
fn disputed_by_bodies(graph: &Graph, declaration: &Declaration) -> bool {
    // Nearly every declaration has one definition, so the common call leaves here without touching
    // the definition table.
    if declaration.definitions().len() < 2 {
        return false;
    }
    let mut written = 0u8;
    for id in declaration.definitions() {
        let Some(definition) = graph.definitions().get(id) else {
            continue;
        };
        if !matches!(definition, Definition::Method(_)) {
            continue;
        }
        let Some(document) = graph.documents().get(definition.uri_id()) else {
            continue;
        };
        // A generated declaration is RBS too: the query interface's `create!` row
        // beside Rails' one `def` is not two bodies.
        if !writes_ruby(document.uri()) {
            continue;
        }
        written += 1;
        if written > 1 {
            return true;
        }
    }
    false
}

/// A signature and the Ruby that will have run, where two gems wrote the Ruby.
///
/// - **The case:** `vendor/rbs/stdlib/json/0/json.rbs` declares
///   `Symbol#as_json: (*untyped) -> Hash[String, String]` for the opt-in `json/add/symbol.rb`,
///   reopening thirteen core classes. ActiveSupport reopens nine of them with an `as_json` whose
///   body is `name` or `to_s`. The signature rung runs first, so `:x.as_json` would answer
///   `Hash[String, String]` in every Rails app, at a tier readers trust.
/// - **The signature is joined, not demoted.** The rung order is the safety argument (`types.md`),
///   and a signature is a maintained statement about every call. So the two answers are unioned,
///   and [`Typed::one`] makes a union terminal: a chain stops instead of stepping off a coin flip.
///   Where the body agrees, the signature is returned exactly as it was, with its tier, facets and
///   type arguments.
/// - **The reader gets both notes:** the signature the answer was declared under, and the file and
///   line of the disputing body. Other provenance merges `or` by `or`, as in [`from_body`], so a
///   *guessed* body's tier carries onto the pair: the weaker half decides, as in [`body_return`]
///   and [`split_bool`].
/// - **It cannot answer `String`**, which is what Ruby does here. That needs to know
///   `json/add/symbol.rb` was never `require`d, and this server reads a gem's whole `lib/`, not a
///   require graph. Both bodies are equally real to it.
fn disputed(graph: &Graph, declared: Typed, body: Typed) -> Typed {
    // The body added nothing the signature did not say, the ordinary case when the bodies agree
    // with each other and the declaration. **The signature is returned untouched**, not rebuilt
    // around the same class, so it cannot lose its arguments or facets here.
    if body
        .classes()
        .iter()
        .all(|class| declared.classes.contains(class))
    {
        return declared;
    }
    let mut derivation = declared.derivation.clone();
    derivation.absorb(body.derivation.clone());
    let folds = Folds::of(graph);
    let mut join = Join::default();
    join.add(declared.clone(), &folds);
    join.add(body, &folds);
    match join.finish(&folds) {
        Some(mut folded) => {
            folded.derivation = derivation;
            folded
        }
        None => declared,
    }
}

/// One call as the text wrote it: everything a call rung reads besides the receiver it resolved.
///
/// **One value, so every rung reads the same facts** and none re-decides one: whether the receiver
/// was `self` ([`reach`]'s privacy), whether a block was written (the arm), what was passed (the
/// binding). A tuple position ([`returned_element`]) reads the same value with no arguments.
#[derive(Clone, Copy)]
struct Call<'c> {
    /// The method's name as written.
    method: &'c str,
    arity: Arity,
    block: &'c Block,
    written: Called<'c>,
    /// Written with `&.`: on `nil` the call is skipped and answers `nil`.
    safe: bool,
    /// Written on `self`, or with no receiver: the one place Ruby lets a private member be called.
    on_self: bool,
}

impl Call<'_> {
    /// The member's key: rubydex keys members with parentheses on (`core-invariants.md`).
    fn member(&self) -> String {
        format!("{}()", self.method)
    }
}

/// The method a Ruby `alias` or `alias_method` renames, where the declaration a call reached is
/// only that alias and the table holds no rows for it ([`Types::adopt_aliases`] copies them where
/// the target is the same class's and has any). Calling an alias runs the method it renames, so
/// the call is read as one of that method: its signature where it has one, else its body.
///
/// - **Every definition is an alias, and they agree on the old name.** A `def` of the name is a
///   body of its own, and keeps it.
/// - **The old name is looked up on the class the alias is written in**, through its ancestors,
///   as Ruby does when the alias runs: an inherited or generated method is found too.
fn renamed(sources: &Sources<'_>, found: DeclarationId) -> Option<DeclarationId> {
    let graph = sources.graph;
    if sources.types.returns.contains_key(&found) {
        return None;
    }
    let declaration = graph.declarations().get(&found)?;
    let mut old: Option<&str> = None;
    for id in declaration.definitions() {
        let Some(Definition::MethodAlias(alias)) = graph.definitions().get(id) else {
            return None;
        };
        // rubydex's old name carries its parentheses (`size()`); see [`Types::adopt_aliases`].
        let name = graph
            .strings()
            .get(alias.old_name_str_id())?
            .as_str()
            .trim_end_matches("()");
        if old.is_some_and(|held| held != name) {
            return None;
        }
        old = Some(name);
    }
    let target = locator::find_member(
        graph,
        *declaration.owner_id(),
        StringId::from(&format!("{}()", old?)),
    )
    .ok()?;
    (target != found).then_some(target)
}

/// The members that take a method's name as their first argument, by the declaration a call
/// reaches ([`sent`], [`named_by_symbol`], [`bound_to`]).
///
/// - **`calls`**: the member makes that call with the arguments after the name, so the call is
///   typed as the one it makes (`send`, `public_send`). The rest only look the method up (`method`,
///   `respond_to?`, `instance_method`) or define it (`define_method`).
/// - **`object`**: the member hands back that method as a `Method` bound to the receiver, which a
///   call of it runs ([`Typed::bound`]).
/// - **`instance`**: the name is looked up on the receiver's instances, not the receiver: a class
///   object's `instance_method(:x)` and `define_method(:x)` name its instances' `x`.
/// - **`private`**: whether a private method counts, as Ruby's `send` and `method` reach one and
///   `public_send` does not.
///
/// `singleton_method` and `define_singleton_method` are not here: on an object that is not a class
/// they name a method of that one object, which its class's method of the same name is not.
#[derive(Clone, Copy)]
struct Namer {
    member: &'static str,
    calls: bool,
    object: bool,
    instance: bool,
    private: bool,
}

const NAMERS: [Namer; 9] = [
    Namer {
        member: "BasicObject#__send__()",
        calls: true,
        object: false,
        instance: false,
        private: true,
    },
    Namer {
        member: "Kernel#send()",
        calls: true,
        object: false,
        instance: false,
        private: true,
    },
    Namer {
        member: "Kernel#public_send()",
        calls: true,
        object: false,
        instance: false,
        private: false,
    },
    Namer {
        member: "Kernel#method()",
        calls: false,
        object: true,
        instance: false,
        private: true,
    },
    Namer {
        member: "Kernel#public_method()",
        calls: false,
        object: true,
        instance: false,
        private: false,
    },
    Namer {
        member: "Kernel#respond_to?()",
        calls: false,
        object: false,
        instance: false,
        private: false,
    },
    Namer {
        member: "Module#instance_method()",
        calls: false,
        object: false,
        instance: true,
        private: true,
    },
    Namer {
        member: "Module#public_instance_method()",
        calls: false,
        object: false,
        instance: true,
        private: false,
    },
    Namer {
        member: "Module#define_method()",
        calls: false,
        object: false,
        instance: true,
        private: true,
    },
];

/// The names of the calls that take a method's name, as a file spells them ([`NAMERS`], and every
/// generated member returning [`generated::SENT`]): where a method's references look for its name
/// handed as a symbol (`references::find`). By name, as the rest of a method's references are.
#[must_use]
pub fn naming_calls(graph: &Graph, types: &Types) -> Vec<String> {
    let bare = |name: &str| {
        name.rsplit_once('#')
            .map(|(_, method)| method.trim_end_matches("()").to_owned())
    };
    let mut names: Vec<String> = NAMERS
        .iter()
        .filter_map(|namer| bare(namer.member))
        .chain(
            types
                .sent
                .iter()
                .filter_map(|id| graph.declarations().get(id))
                .filter_map(|declaration| bare(declaration.name())),
        )
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Whether a member looks up the literal key its call passes first ([`generated::KEYED`]).
#[must_use]
pub fn looks_up_a_key(types: &Types, member: DeclarationId) -> bool {
    types.keyed.contains(&member)
}

/// What a call of a member that looks up its literal key answers ([`generated::KEYED`]): the type
/// the body of knowledge keeping the keys gives it (`t("users.show.title")` is a `String` where
/// the main locale holds one), asked through the registry, which names no module here.
///
/// - **A literal key only**, a string's or a symbol's text: an interpolation or a variable names
///   what only running Ruby knows.
/// - **Keywords the text can read**, each with its value where that is a literal: a `**opts`
///   splat could pass anything, and answers nothing.
/// - **Derived**, with the member's signature named.
fn keyed(
    sources: &Sources<'_>,
    owner: &Typed,
    found: DeclarationId,
    call: Call<'_>,
) -> Option<Typed> {
    let graph = sources.graph;
    let member = graph.declarations().get(&found)?.name().to_owned();
    let key = match call.written.positional.first()? {
        Receiver::Literal {
            symbol: Some(name), ..
        } => name.to_string(),
        Receiver::Literal {
            text: cursor::Text(Some(text)),
            ..
        } => text.to_string(),
        _ => return None,
    };
    let keywords: Vec<(String, knowledge::Written)> = call
        .written
        .keywords?
        .iter()
        .map(|(name, value)| {
            let written = match value {
                Receiver::Literal {
                    symbol: Some(symbol),
                    ..
                } => knowledge::Written::Symbol(symbol.to_string()),
                Receiver::Literal {
                    text: cursor::Text(Some(text)),
                    ..
                } => knowledge::Written::Text(text.to_string()),
                _ => knowledge::Written::Other,
            };
            (name.clone(), written)
        })
        .collect();
    let asked = knowledge::Keyed {
        member: &member,
        key: &key,
        keywords: &keywords,
        block: !matches!(call.block, cursor::Block::None),
    };
    let spelled = sources
        .knowledge
        .modules()
        .find_map(|module| module.keyed_type(&asked))?;
    let mut derivation = owner.derivation.clone();
    derivation.signatures.push(member);
    spelled_type(graph, spelled, derivation)
}

/// A type a body of knowledge spells (`String`, `Hash[Symbol, untyped]`), as a [`Typed`]: its head
/// and each argument the graph declares, `untyped` holding nothing.
fn spelled_type(graph: &Graph, spelled: &str, derivation: Derivation) -> Option<Typed> {
    let (head, arguments) = match spelled.split_once('[') {
        Some((head, rest)) => (head, rest.trim_end_matches(']')),
        None => (spelled, ""),
    };
    let held = arguments
        .split(',')
        .map(str::trim)
        .filter(|argument| !argument.is_empty())
        .map(|argument| declared(graph, argument))
        .collect();
    Some(Typed::of(declared(graph, head)?, derivation).holding(held))
}

/// How `found` reads a method's name ([`NAMERS`]); a generated member returning
/// [`generated::SENT`] calls it publicly, on the receiver. `None` for every other member.
fn namer(sources: &Sources<'_>, found: DeclarationId) -> Option<Namer> {
    if sources.types.sent.contains(&found) {
        return Some(Namer {
            member: "",
            calls: true,
            object: false,
            instance: false,
            private: false,
        });
    }
    let name = sources.graph.declarations().get(&found)?.name();
    NAMERS.iter().find(|namer| namer.member == name).copied()
}

/// The method a symbol names where it is the first argument of a call reaching `found`, a member
/// that takes a method's name ([`NAMERS`]), sent to `on`: `widget.send(:shout)` is `Widget#shout`,
/// `Widget.instance_method(:shout)` the same, `Widget.send(:build)` is `Widget.build`.
///
/// Found as a call to it would be ([`reach`]): the suite fence, the root gate, and privacy as the
/// member reads it.
pub(crate) fn named_by_symbol(
    sources: &Sources<'_>,
    uri_id: UriId,
    found: DeclarationId,
    on: DeclarationId,
    name: &str,
) -> Option<DeclarationId> {
    let namer = namer(sources, found)?;
    let owner = if namer.instance {
        instance_of(sources.graph, on)?
    } else {
        on
    };
    reach(
        sources,
        uri_id,
        owner,
        StringId::from(&format!("{name}()")),
        namer.private,
    )
}

/// A call of a member that calls another by name ([`NAMERS`] that make the call, or a generated
/// member returning [`generated::SENT`], which sends publicly): what the call its first argument names answers, on
/// the same receiver, with the arguments after it. `None` where `found` sends nothing, and
/// `Some(None)` where it does and the call cannot be read.
///
/// - **Ruby's own members only**, by the declaration found: a class writing its own `send` (a
///   socket, a mailer) keeps it.
/// - **A symbol literal only.** A string, an interpolation or a variable names what only running Ruby
///   knows.
/// - **The named call is an ordinary call**: every rule a call written out gets (`new`, `class`, a
///   collection's element, privacy, overloads, a body read) applies, and its tier and derivation
///   are the answer's. `send` reads it as written on `self`, which is how it passes `private`.
fn sent(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: &Typed,
    found: DeclarationId,
    call: Call<'_>,
    scope: &Scope,
) -> Option<Option<Typed>> {
    let namer = namer(sources, found).filter(|namer| namer.calls)?;
    Some(named_call(call, namer.private).and_then(|named| {
        let mut typed = returned_on(sources, uri_id, owner.clone(), named, scope)?;
        typed
            .derivation
            .sent
            .get_or_insert_with(|| Sent::Named(named.method.to_owned()));
        Some(typed)
    }))
}

/// What `method(:shout)` hands back, with the method it is bound to where the lookup finds one
/// ([`Typed::bound`]): `Widget#shout` on the receiver, private or not as the member reads it.
/// A name that is not a symbol literal, or that the receiver lacks, leaves a plain `Method`: Ruby
/// raises there, or binds what only running Ruby knows.
fn bound_to(
    sources: &Sources<'_>,
    uri_id: UriId,
    mut answer: Typed,
    owner: Typed,
    call: Call<'_>,
    namer: Namer,
) -> Typed {
    let Some(Receiver::Literal {
        symbol: Some(name), ..
    }) = call.written.positional.first()
    else {
        return answer;
    };
    let Some(method) = owner.one().and_then(|one| {
        reach(
            sources,
            uri_id,
            one,
            StringId::from(&format!("{name}()")),
            namer.private,
        )
    }) else {
        return answer;
    };
    answer.bound = Some(Rc::new(Bound {
        on: owner,
        method,
        name: name.to_string(),
        private: namer.private,
    }));
    answer
}

/// A call of a bound `Method` ([`Typed::bound`]): `formatter.call(2)`, `formatter.(2)`,
/// `formatter[2]`, `formatter === 2`, as the call of the method it is bound to, with these
/// arguments, on the object it was bound on.
///
/// `None` where this is no such call, or the receiver is bound to nothing: the call rung answers
/// as before. `f&.call(a)` on an `f` that may be `nil` answers `nil` there.
#[allow(clippy::option_option)]
fn called_method(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: &Typed,
    call: Call<'_>,
    scope: &Scope,
) -> Option<Option<Typed>> {
    if !matches!(call.method, "call" | "[]" | "===") {
        return None;
    }
    let bound = owner.bound.clone()?;
    let mut answer = returned_on(
        sources,
        uri_id,
        bound.on.clone(),
        Call {
            method: &bound.name,
            safe: false,
            on_self: bound.private,
            ..call
        },
        scope,
    );
    if let Some(typed) = answer.as_mut() {
        typed
            .derivation
            .sent
            .get_or_insert_with(|| Sent::Bound(bound.name.clone()));
    }
    Some(if call.safe && owner.nilable {
        answer.map(Typed::or_nil)
    } else {
        answer
    })
}

/// A bound `Method` passed as the block (`&method(:shout)`, `&formatter`), for a signature's
/// `[U]`: the method it is bound to, called on its object with as many values as the signature
/// hands the block. One class or nothing, as [`block_return`] answers.
fn bound_return(
    sources: &Sources<'_>,
    uri_id: UriId,
    found: DeclarationId,
    bound: &Bound,
    scope: &Scope,
) -> Option<Typed> {
    let handed = (0..MAX_HANDED)
        .take_while(|index| sources.types.yielded(found, *index).is_some())
        .count();
    // What the block is handed is typed where the signature says, but a `Method` binds its own
    // parameters, which this does not read: the count picks the arm, and nothing is bound.
    let arity = u32::try_from(handed)
        .ok()
        .filter(|count| *count > 0)
        .map_or(Arity::Unknown, Arity::Exactly);
    let joined = returned_on(
        sources,
        uri_id,
        bound.on.clone(),
        Call {
            method: &bound.name,
            arity,
            block: &Block::None,
            written: Called {
                positional: &[],
                keywords: Some(&[]),
            },
            safe: false,
            on_self: bound.private,
        },
        scope,
    )?;
    if joined.derivation.tier() == Tier::Guessed {
        return None;
    }
    joined.one()?;
    Some(joined)
}

/// The call a sender's first argument names ([`sent`]): that symbol, with the arguments after it,
/// the block, and `&.` as written. `private` reads it as a call on `self`.
fn named_call(call: Call<'_>, private: bool) -> Option<Call<'_>> {
    let (
        Receiver::Literal {
            symbol: Some(name), ..
        },
        positional,
    ) = call.written.positional.split_first()?
    else {
        return None;
    };
    let arity = match call.arity {
        Arity::Exactly(counted) => Arity::Exactly(counted.checked_sub(1)?),
        Arity::Keyed(counted) => Arity::Keyed(counted.checked_sub(1)?),
        Arity::Spread(counted) => Arity::Spread(counted.checked_sub(1)?),
        Arity::Unknown => Arity::Unknown,
    };
    Some(Call {
        method: name,
        arity,
        written: Called {
            positional,
            keywords: call.written.keywords,
        },
        on_self: private,
        ..call
    })
}

/// What a call wrote between its parentheses, as `cursor` recorded it: one shape per counted
/// positional, and the braceless keywords by name (`None` where they cannot be read).
#[derive(Clone, Copy)]
struct Called<'w> {
    positional: &'w [Receiver],
    keywords: Option<&'w [(String, Receiver)]>,
}

/// What a call passed, typed where it is written ([`passed_to`]).
struct Passed {
    /// One per counted positional, `None` where nothing typed it; `None` as a whole where the
    /// shapes are no claim ([`Receiver::Returned`]).
    positional: Option<Vec<Option<Typed>>>,
    /// The braceless keywords by name, `None` where they cannot be read.
    keywords: Option<Vec<(String, Option<Typed>)>>,
    /// Whether the call wrote a braceless hash ([`Arity::Keyed`]), which a method with no keyword
    /// parameters receives as one more positional.
    keyed: bool,
    /// The call's block, for a body that `yield`s to it ([`Receiver::Yield`]).
    handed: Handed,
    /// Whether the call passed a block, for a body that asks ([`Receiver::BlockGiven`]).
    /// `None` where the text does not say, or no `def` of the method asks.
    given: Option<bool>,
}

/// What a call's block is, for a body that `yield`s to it.
#[derive(Debug, Clone)]
enum Handed {
    /// Nothing a `yield` can be answered with: no block, a forwarded `&blk`, or a block whose
    /// value cannot be typed.
    Nothing,
    /// A written block whose every value is this, typed where the call is written.
    Value(Box<Typed>),
    /// `&:name`: `name` called on what each `yield` hands over.
    Symbol(String),
    /// `&value` where the value is a set of proc literals ([`Typed::procs`]), with the read context
    /// of the call that passed it: a `yield` reads them there, not in the body that yields.
    Procs {
        procs: Rc<[(UriId, u32)]>,
        object: Option<DeclarationId>,
        bound: Option<u64>,
        made: Option<u64>,
    },
}

/// The arguments one call passed, by parameter slot, for the body of the method it reached.
#[derive(Debug)]
pub struct Binding {
    /// The method whose parameters these are.
    method: DeclarationId,
    /// Each positional argument, by [`ParameterSlot::Positional`] index. `None` where the call
    /// passed nothing typed there.
    positional: Vec<Option<Typed>>,
    /// Each keyword argument the method declares a parameter for, by name.
    keywords: Vec<(String, Option<Typed>)>,
    /// What each parameter the call left out holds at that call: its default, as written in the
    /// method's one Ruby definition, placed in the graph.
    defaults: Vec<(ParameterSlot, Receiver)>,
    /// That definition's document and where it starts, the scope a default is read in.
    at: Option<(UriId, u32)>,
    /// The call's block, for a `yield` in the body ([`yielded_to_block`]).
    handed: Handed,
    /// Whether the call passed a block, for a `block_given?` in the body ([`unreached`]).
    given: Option<bool>,
    /// The [`ReadKey`] part: a hash of the method and of what is bound. Never `0`, which is none.
    key: u64,
}

impl Binding {
    /// What a call binds, or `None` where it binds nothing typed.
    ///
    /// - **Positionals only in a `def` of required and optional positionals**, in every definition
    ///   alike, since [`ParameterSlot::Positional`] counts exactly those, in written order. A rest
    ///   or a trailing required parameter moves which argument lands where by the call's count,
    ///   so no positional binds; keywords still do.
    /// - **Only a count Ruby accepts**: at least the required ones, at most every positional.
    /// - **Keywords by name, only where every definition declares that keyword.** A method that
    ///   takes none receives a braceless hash as one more positional, a `Hash` (Ruby 3's rule;
    ///   so `create!(name: x)` reads `attributes.is_a?(Array)` as ruled out).
    fn of(sources: &Sources<'_>, method: DeclarationId, passed: &Passed) -> Option<Self> {
        use std::hash::{Hash, Hasher};
        let graph = sources.graph;
        let mut shape: Option<Shape> = None;
        let mut ruby = Vec::new();
        for definition in locator::definitions_of(graph, method) {
            let Definition::Method(written) = definition else {
                continue;
            };
            // An RBS `def:` is a signature, which [`from_parameter`] reads first; only Ruby binds.
            // A generated declaration is RBS too: `create!`'s query-interface row beside Rails' own
            // `def` would otherwise disagree with it and bind nothing.
            let uri = graph.documents().get(definition.uri_id())?.uri();
            if !writes_ruby(uri) {
                continue;
            }
            ruby.push((
                uri.to_owned(),
                *definition.uri_id(),
                definition.offset().start(),
                definition.offset().end(),
            ));
            let this = Shape::of(graph, written.signatures().as_slice().first()?)?;
            match &shape {
                None => shape = Some(this),
                Some(seen) if *seen == this => {}
                Some(_) => return None,
            }
        }
        let shape = shape?;
        // A braceless hash to a method with no keyword parameters is one more positional.
        let hashed = passed.keyed && !shape.takes_keywords;
        let placed = passed.positional.as_ref().filter(|written| {
            let count = written.len() + usize::from(hashed);
            shape.placed && count >= shape.required && count <= shape.required + shape.optional
        });
        let mut positional = placed.cloned().unwrap_or_default();
        if hashed && placed.is_some() {
            positional
                .push(declared(graph, "Hash").map(|hash| Typed::of(hash, Derivation::default())));
        }
        let keywords: Vec<(String, Option<Typed>)> = passed
            .keywords
            .iter()
            .flatten()
            .filter(|(name, _)| shape.keywords.contains(name))
            .cloned()
            .collect();
        // **What the call left out.** A braceless hash to a method with no keyword parameters is one
        // more positional, so the first one after the written ones is not left out. A keyword is
        // left out only where the keywords could be read at all (`**opts` could pass any).
        let mut omitted = Vec::new();
        if let Some(written) = placed {
            let from = written.len() + usize::from(passed.keyed && !shape.takes_keywords);
            omitted.extend((from..shape.required + shape.optional).map(ParameterSlot::Positional));
        }
        if shape.takes_keywords
            && let Some(written) = &passed.keywords
        {
            omitted.extend(
                shape
                    .optional_keywords
                    .iter()
                    .filter(|name| !written.iter().any(|(passed, _)| passed == *name))
                    .map(|name| ParameterSlot::Keyword(name.clone())),
            );
        }
        // Read only from a method with one Ruby definition: two could default differently, and
        // nothing says which runs.
        let (defaults, at) = match ruby.as_slice() {
            [(uri, uri_id, start, end)] if !omitted.is_empty() => (
                defaults_of(sources, uri, *start, *end, &omitted).unwrap_or_default(),
                Some((*uri_id, *start)),
            ),
            _ => (Vec::new(), None),
        };
        if defaults.is_empty()
            && matches!(passed.handed, Handed::Nothing)
            && passed.given.is_none()
            && positional
                .iter()
                .chain(keywords.iter().map(|(_, typed)| typed))
                .all(Option::is_none)
        {
            return None;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        method.hash(&mut hasher);
        let held = |typed: &Option<Typed>,
                    hasher: &mut std::collections::hash_map::DefaultHasher| {
            match typed {
                Some(typed) => {
                    true.hash(hasher);
                    typed.classes.hash(hasher);
                    typed.nilable.hash(hasher);
                    typed.boolean.hash(hasher);
                    typed.arguments.hash(hasher);
                    typed.same.hash(hasher);
                    // An argument built by `new` carries its own binding, which reads differently.
                    typed.made.as_ref().map(|made| made.0.key).hash(hasher);
                }
                None => false.hash(hasher),
            }
        };
        for typed in &positional {
            held(typed, &mut hasher);
        }
        for (name, typed) in &keywords {
            name.hash(&mut hasher);
            held(typed, &mut hasher);
        }
        for (slot, _) in &defaults {
            format!("{slot:?}").hash(&mut hasher);
        }
        match &passed.handed {
            Handed::Nothing => 0u8.hash(&mut hasher),
            Handed::Value(value) => {
                1u8.hash(&mut hasher);
                held(&Some((**value).clone()), &mut hasher);
            }
            Handed::Symbol(name) => {
                2u8.hash(&mut hasher);
                name.hash(&mut hasher);
            }
            Handed::Procs {
                procs,
                object,
                bound,
                made,
            } => {
                3u8.hash(&mut hasher);
                procs.hash(&mut hasher);
                (object, bound, made).hash(&mut hasher);
            }
        }
        passed.given.hash(&mut hasher);
        Some(Self {
            method,
            positional,
            keywords,
            defaults,
            at,
            handed: passed.handed.clone(),
            given: passed.given,
            key: hasher.finish().max(1),
        })
    }
}

/// What one Ruby `def`'s parameter list says about binding a call ([`Binding::of`]).
#[derive(PartialEq, Eq)]
struct Shape {
    required: usize,
    optional: usize,
    /// No rest and no trailing required positional, so positionals land by index.
    placed: bool,
    /// Every keyword parameter's name, sorted.
    keywords: Vec<String>,
    /// The keywords with a default, sorted.
    optional_keywords: Vec<String>,
    /// Whether a braceless hash is keywords here (a named keyword or `**`), not a positional.
    takes_keywords: bool,
}

impl Shape {
    fn of(graph: &Graph, parameters: &[Parameter]) -> Option<Self> {
        let mut shape = Self {
            required: 0,
            optional: 0,
            placed: true,
            keywords: Vec::new(),
            optional_keywords: Vec::new(),
            takes_keywords: false,
        };
        for parameter in parameters {
            match parameter {
                Parameter::RequiredPositional(_) => shape.required += 1,
                Parameter::OptionalPositional(_) => shape.optional += 1,
                Parameter::RestPositional(_) | Parameter::Post(_) => shape.placed = false,
                Parameter::RequiredKeyword(named) => {
                    shape
                        .keywords
                        .push(graph.strings().get(named.str())?.as_str().to_owned());
                    shape.takes_keywords = true;
                }
                Parameter::OptionalKeyword(named) => {
                    let name = graph.strings().get(named.str())?.as_str().to_owned();
                    shape.keywords.push(name.clone());
                    shape.optional_keywords.push(name);
                    shape.takes_keywords = true;
                }
                // `...` takes a call's keywords as `**` does: a braceless hash is no
                // positional of a `def f(a, ...)`.
                Parameter::RestKeyword(_) | Parameter::Forward(_) => shape.takes_keywords = true,
                _ => {}
            }
        }
        shape.keywords.sort();
        shape.optional_keywords.sort();
        Some(shape)
    }
}

/// The defaults of the parameters a call left out, from the one `def` at `start..end` in `uri`,
/// placed in the graph's coordinates.
fn defaults_of(
    sources: &Sources<'_>,
    uri: &str,
    start: u32,
    end: u32,
    omitted: &[ParameterSlot],
) -> Option<Vec<(ParameterSlot, Receiver)>> {
    let document = sources.memo.reads.documents.of(uri, sources.read)?;
    let span = document.rebase.span_to_buffer(ByteSpan { start, end })?;
    let held = document
        .shapes(uri, sources.held_exits)
        .defaults
        .get(&(span.start, span.end))?;
    held.iter()
        .filter(|(slot, _)| omitted.contains(slot))
        .map(|(slot, value)| Some((slot.clone(), value.rebased(&document.rebase)?)))
        .collect()
}

/// What the one `def` at `start..end` in `uri` defaults its optional parameters to, as written:
/// each slot's expression, for a card to print where rubydex records only that it is optional.
///
/// Empty where the text cannot be read, or the `def` is in text the graph has not seen yet.
pub(super) fn written_defaults(
    sources: &Sources<'_>,
    uri: &str,
    start: u32,
    end: u32,
) -> Vec<(ParameterSlot, String)> {
    let Some(document) = sources.memo.reads.documents.of(uri, sources.read) else {
        return Vec::new();
    };
    let Some(span) = document.rebase.span_to_buffer(ByteSpan { start, end }) else {
        return Vec::new();
    };
    document
        .shapes(uri, sources.held_exits)
        .written_defaults
        .get(&(span.start, span.end))
        .map(|held| {
            held.iter()
                .filter_map(|(slot, (from, to))| {
                    let text = document.source.get(*from as usize..*to as usize)?;
                    Some((slot.clone(), text.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What a method's Ruby definitions do with a call's block, so a body read for a call carries
/// only what the body reads: every other difference between two calls makes the body read twice.
#[derive(Default)]
struct BlockUse {
    /// One yields or calls its own `&block` ([`cursor::Shapes::yields`]): it needs the
    /// block's value. A definition whose document cannot be read counts as one that does.
    hands: bool,
    /// One asks whether there is a block ([`cursor::Shapes::asks`]).
    asks: bool,
}

/// [`BlockUse`] of `method`.
fn block_use(sources: &Sources<'_>, method: DeclarationId) -> BlockUse {
    let graph = sources.graph;
    let mut asked = BlockUse::default();
    for definition in locator::definitions_of(graph, method) {
        if !matches!(definition, Definition::Method(_)) {
            continue;
        }
        let Some(uri) = graph
            .documents()
            .get(definition.uri_id())
            .map(|document| document.uri())
        else {
            asked.hands = true;
            continue;
        };
        if uri.ends_with(".rbs") {
            continue;
        }
        let Some((document, span)) =
            sources
                .memo
                .reads
                .documents
                .of(uri, sources.read)
                .and_then(|document| {
                    let span = document.rebase.span_to_buffer(ByteSpan {
                        start: definition.offset().start(),
                        end: definition.offset().end(),
                    })?;
                    Some((document, (span.start, span.end)))
                })
        else {
            asked.hands = true;
            continue;
        };
        let shapes = document.shapes(uri, sources.held_exits);
        asked.hands |= shapes
            .yields
            .get(&span)
            .is_none_or(|sites| sites.as_ref().is_none_or(|sites| !sites.is_empty()));
        asked.asks |= shapes.asks.contains(&span);
    }
    asked
}

/// Whether a call passed a block, where its text says.
///
/// - **Nothing in the block slot is none**, except under an uncountable arity: `...` passes the
///   caller's block on, and is counted as a splat is.
/// - **A written block and `&:name` are one.** An anonymous `&` is whatever the caller was
///   passed.
/// - **`&value` is one unless the value is `nil`**, which passes none. A value that may be `nil`,
///   or is only guessed, says nothing.
/// - **Only a call the text wrote reaches a body read with its block slot.** The calls a rule
///   stands in for pass none there: `&:name`'s, which is right, since `Symbol#to_proc` passes on
///   only a block its proc is called with; `new` on a literal reaches a body only where no
///   signature of its own answers, and then as an ordinary call.
fn passes_a_block(
    sources: &Sources<'_>,
    uri_id: UriId,
    arity: Arity,
    block: &Block,
    scope: &Scope,
) -> Option<bool> {
    match block {
        Block::None => (arity != Arity::Unknown).then_some(false),
        Block::Written(_) | Block::Breaking(_) | Block::Symbol(_) => Some(true),
        Block::Forwarded => None,
        Block::Passed(value) => {
            let typed = method_receiver(sources, uri_id, value, scope)
                .filter(|typed| typed.derivation.tier() != Tier::Guessed)?;
            let nil = declared(sources.graph, "NilClass");
            let nils = typed
                .classes()
                .iter()
                .filter(|class| Some(**class) == nil)
                .count();
            match (nils, typed.nilable) {
                (0, false) => Some(true),
                (only, _) if only == typed.classes().len() => Some(false),
                _ => None,
            }
        }
    }
}

/// Whether the body being read leaves `value` out: it is one side of `block_given?`
/// ([`Receiver::BlockGiven`]), and the call the body is read for is on the other.
fn unreached(sources: &Sources<'_>, uri_id: UriId, value: &Receiver) -> bool {
    let Receiver::BlockGiven {
        at,
        method,
        given,
        value,
    } = value
    else {
        return false;
    };
    given_here(sources, uri_id, *at, method) == Some(!*given) || unreached(sources, uri_id, value)
}

/// Whether the call the `def` at `at` named `method` is read for passed a block; `None` outside a
/// read for one call of that method, and where the call does not say.
///
/// **`block_given?` is `Kernel`'s**, unless a Ruby `def` of that name exists anywhere in the
/// graph, in which case nothing is left out: a class may answer it for itself. None of the six
/// reference corpora or their gems define one.
fn given_here(sources: &Sources<'_>, uri_id: UriId, at: u32, method: &str) -> Option<bool> {
    let graph = sources.graph;
    let bound = sources
        .memo
        .reads
        .bindings
        .borrow()
        .get(&sources.bound?)
        .cloned()?;
    let given = bound.given?;
    let here = sources.scope_at(uri_id, at).caller(graph)?;
    let found = member_of(
        sources,
        uri_id,
        here,
        StringId::from(&format!("{method}()")),
    )?;
    let kernels = graph.members_named("block_given?()").iter().all(|id| {
        graph
            .declarations()
            .get(id)
            .and_then(|declaration| declaration.name().rsplit_once('#'))
            .is_some_and(|(owner, _)| owner == "Kernel" || owner == "Kernel::<Kernel>")
    });
    (bound.method == found && kernels).then_some(given)
}

/// What this call passed where it is written, typed, for a body read at the seam.
///
/// - **A guessed argument binds nothing**, as it picks no overload: the body's answer would carry
///   the call's tier around a class read off a name.
/// - **Its assignment offsets are dropped**: they are lines of the caller's document, and the body
///   is read in another.
/// - **Positionals are no claim** where the count is unknown or the shapes are ([`Receiver::Returned`]).
fn passed_to(
    sources: &Sources<'_>,
    uri_id: UriId,
    arity: Arity,
    written: Called<'_>,
    block: &Block,
    scope: &Scope,
) -> Option<Passed> {
    let typed = |argument: &Receiver| {
        method_receiver(sources, uri_id, argument, scope)
            .filter(|typed| typed.derivation.tier() != Tier::Guessed)
            .map(|mut typed| {
                typed.derivation.assignments.clear();
                typed
            })
    };
    let positional = match arity {
        Arity::Exactly(counted) | Arity::Keyed(counted) | Arity::Spread(counted)
            if written.positional.len() == counted as usize =>
        {
            Some(written.positional.iter().map(typed).collect())
        }
        _ => None,
    };
    let keywords = written.keywords.map(|written| {
        written
            .iter()
            .map(|(name, value)| (name.clone(), typed(value)))
            .collect()
    });
    // The block, typed here, where its own variables are in scope, for a body that `yield`s.
    let handed = match block {
        Block::None | Block::Forwarded => Handed::Nothing,
        Block::Symbol(name) => Handed::Symbol(name.clone()),
        Block::Passed(value) => method_receiver(sources, uri_id, value, scope)
            .and_then(|typed| typed.procs)
            .map_or(Handed::Nothing, |procs| Handed::Procs {
                procs,
                object: sources.object,
                bound: sources.bound,
                made: sources.made,
            }),
        written => block_value(sources, uri_id, written, scope)
            .map_or(Handed::Nothing, |value| Handed::Value(Box::new(value))),
    };
    Some(Passed {
        positional,
        keywords,
        // A spread that may be empty still counts: the slot it may fill is then read as unknown,
        // not as its default.
        keyed: matches!(arity, Arity::Keyed(_) | Arity::Spread(_)),
        handed,
        given: None,
    })
}

/// What a **method parameter** is: what the enclosing `def`'s signature declares, and nothing else.
///
/// 1. **Finding the `def`**, as in [`from_super`]: the type of `self` where the name was written
///    names the class, and the method is looked up on it. Unlike `super`, this means *this* method,
///    so the ordinary ancestor walk is right; the nearest `def` is the one it is written in.
/// 2. **A default is not a type.** It says what the parameter holds when a caller passed nothing,
///    and a caller may pass anything: `def f(limit = 10)` with `f("all")` holds a `String`.
/// 3. **The tier is Derived, and the derivation names the declaration**, as [`returned_by`] and
///    [`yielded_by`] do: `def show(story)` with `-> Story` cites `StoriesController#show()`.
fn from_parameter(
    sources: &Sources<'_>,
    uri_id: UriId,
    at: u32,
    method: &str,
    slot: &ParameterSlot,
) -> Option<Typed> {
    let graph = sources.graph;
    // The class the `def` hangs off: the class for an instance method, its singleton for
    // `def self.`. The same value `Receiver::SelfObject` resolves to.
    let here = sources.scope_at(uri_id, at).caller(graph)?;
    // rubydex keys members with parentheses on; see `core-invariants.md`.
    let found = member_of(
        sources,
        uri_id,
        here,
        StringId::from(&format!("{method}()")),
    )?;
    // **Where no signature says, what this call passed** ([`Sources::bound`]), for this method's
    // own parameters only, **or what `new` passed** to the object whose body is read
    // ([`Sources::made`]), for its `initialize`'s.
    let Some(declared) = sources.types.parameter(found, slot) else {
        let reads = &sources.memo.reads;
        let bound = [sources.bound, sources.made]
            .into_iter()
            .flatten()
            .find_map(|key| {
                reads
                    .bindings
                    .borrow()
                    .get(&key)
                    .filter(|bound| bound.method == found)
                    .cloned()
            })?;
        // **Left out at this call, so it holds its default**, read in the method's own scope with
        // this call's binding: `def f(a, b = a)` gives `b` what was passed as `a`.
        if let Some((_, default)) = bound.defaults.iter().find(|(held, _)| held == slot) {
            let (written_in, start) = bound.at?;
            let scope = sources.scope_at(written_in, start);
            return method_receiver(sources, written_in, default, &scope);
        }
        return match slot {
            ParameterSlot::Positional(index) => bound.positional.get(*index)?.clone(),
            ParameterSlot::Keyword(name) => bound
                .keywords
                .iter()
                .find(|(written, _)| written == name)?
                .1
                .clone(),
        };
    };
    // **A declared type that names every object (`Object`, `BasicObject`, `Class`, `Module`) is no
    // type, and must not displace the rungs below.** The same refusal the module makes for a
    // `Namespace::Todo`, applied to a parameter. an engine writes
    // `@param preference_store_class [Class]` over a `def` whose body calls `preference_store` on
    // it: true of every class object, a member of `Class` on nothing, and enough to take away a
    // correct answer from below. Falling through costs only the members of these four names, which
    // nobody asks a parameter for.
    if let Return::Class { name, .. } = &declared.of
        && matches!(&**name, "Object" | "BasicObject" | "Class" | "Module")
    {
        return None;
    }
    // The owner is the class the `def` is written in, which is what `self` or `instance` in the
    // parameter's type means. [`yielded_by`] passes the same value.
    let owner = Typed::of(here, Derivation::default());
    let declaration = resolved(sources, &declared.of, &owner, None)?;
    let arguments = held_by_return(sources, &declared.of, &owner, None);
    let mut derivation = Derivation::default();
    derivation
        .signatures
        .push(graph.declarations().get(&found)?.name().to_owned());
    Some(
        Typed::of(declaration, derivation)
            .faceted(declared)
            .holding(arguments),
    )
}

/// What `super` returns: the same method, on the first thing above this one that declares it.
///
/// Ruby takes `super`'s name from the enclosing method and its lookup from the **receiver's**
/// ancestry, starting one past the class the method was found on. `Namespace::ancestors()` is
/// rubydex's copy of that linearization, so this finds `self` in it and asks each entry after it.
///
/// - **Not [`query::find_member_in_ancestors`] on `self`.** That would find *this* method and read
///   its own body as its own return: a loop. Starting after `self` is the difference, so the walk
///   is written out here.
/// - **`self` is not always first in its own ancestry.** A `prepend`ed module sits above the class,
///   and `super` from the class's method skips it, as Ruby does. So "after `self`" is found by
///   searching.
/// - **An unresolved ancestor stops the walk.** An `Ancestor::Partial` has no declaration, so
///   nobody can say whether it declares the method; stepping over it could answer from something
///   Ruby never reaches. A `Complete` entry is asked with the ordinary ancestor walk, because
///   `super` into an included module whose parent declares the method is still `super`.
///
/// Three shapes it cannot answer, none of them a gap in the walk:
///
/// 1. **`super` inside a `module`.** A concern's `super` resolves through the *including* class's
///    ancestry, which the module does not have.
/// 2. **`super` outside every `def`**, as in a `define_method` block. `cursor::Finder::super_in`
///    refuses it first, because the name is the macro's argument.
/// 3. **A method nothing above declares.** That is a `NoMethodError` at runtime; here it is no
///    answer.
///
/// The tier is **Derived** and the note names the declaration, as `returned_by` does:
/// `def message; super; end -> String` cites `StandardError#message()`.
fn from_super(
    sources: &Sources<'_>,
    uri_id: UriId,
    at: u32,
    method: &str,
    arity: Arity,
    block: bool,
) -> Option<Typed> {
    let graph = sources.graph;
    // The type of `self` where the keyword was written: the class for an instance method, its
    // singleton for `def self.`. The same value `Receiver::SelfObject` resolves to.
    let here = sources.scope_at(uri_id, at).caller(graph)?;
    let namespace = graph
        .declarations()
        .get(&here)
        .and_then(Declaration::as_namespace)?;
    // rubydex keys members with parentheses on; see `core-invariants.md`.
    let member = StringId::from(&format!("{method}()"));
    let mut above = false;
    for ancestor in namespace.ancestors() {
        match ancestor {
            Ancestor::Complete(id) if !above => above = *id == here,
            Ancestor::Complete(id) => {
                if let Some(found) = member_in(sources, uri_id, *id, member) {
                    let owner = Typed::of(here, Derivation::default());
                    let mut typed = returned_from(
                        sources,
                        found,
                        owner,
                        (arity, None),
                        block,
                        None,
                        None,
                        None,
                    )?;
                    // Named after the answer, so a `super` that reached a method with no type
                    // leaves no note about a class that tells the reader nothing.
                    typed.derivation.superclass =
                        Some(graph.declarations().get(&found)?.name().to_owned());
                    return Some(typed);
                }
            }
            // A name with nothing behind it. Before `self`, it is a `prepend` this method is
            // already below. After it, it is a place `super` may land and this cannot read, so the
            // walk stops.
            Ancestor::Partial(_) => {
                if above {
                    return None;
                }
            }
        }
    }
    None
}

/// One position of the tuple a call returns: the `write_io` of `read_io, write_io = IO.pipe`.
///
/// - **[`returned_by`] up to the member lookup, then a different table.** The return side cannot
///   answer: `[IO, IO]` is not one class, so [`class_of`] refuses it.
/// - **Derived, and it says so through the same field the return side uses.** The reader can go and
///   check `IO.pipe()`.
/// - **`a, b = x&.pair` is two `nil`s where `x` is `nil`**, so each position carries the mark, as
///   [`returned_by`] marks a return. `NilClass` spreads no tuple, so a plain `.` on a `T?` is
///   answered from `T` alone.
fn returned_element(
    sources: &Sources<'_>,
    uri_id: UriId,
    on: &Receiver,
    call: Call<'_>,
    scope: &Scope,
    index: u32,
) -> Option<Typed> {
    let graph = sources.graph;
    let owner = method_receiver(sources, uri_id, on, scope)?;
    let skipped = call.safe && owner.nilable;
    let found = reach(
        sources,
        uri_id,
        owner.one()?,
        StringId::from(&call.member()),
        call.on_self,
    )?;
    // **A call that wrote a block is refused.** The table holds only what blockless arms agree on
    // (see [`declared_tuple`]). `IO.pipe` hands its block the tuple and returns what the block
    // returned, so answering from the blockless arm would read the wrong half of the signature.
    // The arity is not read: the tuple table is one entry per method, for `Types::yields`' reason,
    // and the number of arguments does not change what the result is spread into.
    if call.block.written() {
        return None;
    }
    let declaration = declared(graph, sources.types.tupled(found, index as usize)?)?;
    let mut derivation = owner.derivation;
    derivation
        .signatures
        .push(graph.declarations().get(&found)?.name().to_owned());
    let element = Typed::of(declaration, derivation);
    Some(if skipped { element.or_nil() } else { element })
}

/// Whether `new` on `class_object` is **not** Ruby's rule (an instance of that class): the class's
/// own `self.new` declares a return, and it names something else.
///
/// - **Its own, not inherited.** stdlib's `Tempfile.new: (…) -> Tempfile` reached from
///   `Paperclip::Tempfile` names the parent, while Ruby builds the subclass.
/// - **Something else.** `Net::HTTP.new: (…) -> Net::HTTP` is Ruby's rule spelled out, and the rule
///   keeps what `new` passed ([`made_by`]); a signature naming the class itself adds nothing.
fn new_overridden(
    sources: &Sources<'_>,
    class_object: DeclarationId,
    new: DeclarationId,
    arity: Arity,
) -> bool {
    let graph = sources.graph;
    let own = graph
        .declarations()
        .get(&new)
        .and_then(|declaration| declaration.name().rsplit_once('#'))
        .zip(graph.declarations().get(&class_object))
        .is_some_and(|((owner, _), class)| owner == class.name());
    if !own {
        return false;
    }
    let Some(returns) = sources.types.returns(new, arity, false) else {
        return false;
    };
    let receiver = Typed::of(class_object, Derivation::default());
    resolved(sources, &returns.of, &receiver, None)
        .is_some_and(|declared| Some(declared) != instance_of(graph, class_object))
}

/// The class a class object belongs to: `Foo::<Foo>` is `Foo`.
///
/// - **The inverse of [`singleton_name`]**, and a sibling of [`model_of`]. All split on the same
///   `::<`, rubydex's singleton spelling. `hover::attached_name` is the third place that must know
///   it.
/// - **The lookup makes it safe.** A name that only *looks* like a singleton answers nothing unless
///   its class was really indexed.
fn instance_of(graph: &Graph, class_object: DeclarationId) -> Option<DeclarationId> {
    let name = graph.declarations().get(&class_object)?.name();
    let (attached, _) = name.rsplit_once("::<")?;
    declared(graph, attached)
}

/// What a method returns, read from its **body**, where no signature says.
///
/// **The last rung.** Every rung above reads something written down (an RBS type, a
/// `CONST = Klass.new`, a path convention). This reads the Ruby that will have run, the only
/// evidence an application's own `def` offers: `order.customer.name` would stop at `customer`, an
/// `attr_reader` no `sig/` declares. Always on, [`BODY_HOPS`] deep.
///
/// # What makes it safe, in the order the checks run
///
/// 1. **Every definition of the method, not the first.** A name reopened across files, or
///    overridden, has several bodies and no reason to prefer one; each counts.
/// 2. **Every exit of every body, not the last statement.** A guard-clause `return` and a final
///    chain are both resolved and joined ([`Join`]): two classes are a union, which a label can
///    show and a chain cannot step off ([`Typed::one`]).
/// 3. **One exit that answers nothing declines the method.** Two branches of three is not the
///    return.
/// 4. **The weakest exit decides the tier.** A body whose exit was guessed from a name is a guess,
///    as [`Derivation::tier`] says about `guess`.
/// 5. **A recursion is refused where it comes back** ([`Reads::bodies`]), and
///    [`Sources::body_hops`] bounds what is left at [`BODY_HOPS`]. `def a; b; end` beside
///    `def b; a; end` is legal Ruby, and an overflow aborts the process.
///
/// [`Derivation::assignment`]'s offset is deliberately **not** brought back. Readers draw it
/// against the *request's* text, and this one came from another document (`types.md`).
fn from_body(
    sources: &Sources<'_>,
    found: DeclarationId,
    owner: Typed,
    passed: Option<&Passed>,
) -> Option<Typed> {
    // A generated member standing for a `def` Ruby extends onto the receiver is that `def`.
    let (found, extended) = match defined_as_own(sources, found) {
        Some((written, module)) => (written, Some(module)),
        None => (found, None),
    };
    let binding = passed
        .and_then(|passed| Binding::of(sources, found, passed))
        .map(Rc::new);
    // What built the object, for its `initialize`'s parameters.
    let made = owner.made.as_ref().map(|made| Rc::clone(&made.0));
    if binding.is_none() && made.is_none() {
        return read_body(sources, found, owner, (None, None), extended);
    }
    let bound = binding.as_ref().map(|binding| binding.key);
    let built = made.as_ref().map(|made| made.key);
    let held = |reads: &Reads| {
        for binding in binding.iter().chain(made.iter()) {
            reads
                .bindings
                .borrow_mut()
                .entry(binding.key)
                .or_insert_with(|| Rc::clone(binding));
        }
    };
    held(&sources.memo.reads);
    read_body(sources, found, owner, (bound, built), extended)
}

/// The Ruby `def` a generated member stands for ([`generated::DEFINED`]), and the module it is
/// written in: `Account.find_local` is `Account::Finder#find_local()`, the `def` in the concern's
/// `class_methods do`, which rubydex files as the module's own.
///
/// The declaration must exist, so a `def` since deleted answers nothing rather than a name.
fn defined_as_own(
    sources: &Sources<'_>,
    found: DeclarationId,
) -> Option<(DeclarationId, DeclarationId)> {
    let graph = sources.graph;
    let module = sources.types.defined.get(&found)?;
    let holder = declared(graph, module)?;
    let (_, member) = graph.declarations().get(&found)?.name().rsplit_once('#')?;
    let written = DeclarationId::from(format!("{module}#{member}").as_str());
    graph.declarations().get(&written)?;
    Some((written, holder))
}

/// [`from_body`] once the bindings are settled: the body read for `owner`, bound to `bound`, on the
/// object `made` built.
fn read_body(
    sources: &Sources<'_>,
    found: DeclarationId,
    owner: Typed,
    (bound, made): (Option<u64>, Option<u64>),
    extended: Option<DeclarationId>,
) -> Option<Typed> {
    // The classes and both facets travel; only the *derivation* is merged. A body answering a union
    // answers one here too. The rung above stops at the `?` in [`Typed::one`], like every other
    // rung.
    let Typed {
        classes,
        derivation: inner,
        nilable,
        boolean,
        arguments,
        same,
        made: returned,
        procs,
        bound: held,
    } = {
        let object = match owner.classes.as_slice() {
            [one] if !owner.nilable => Some(*one),
            _ => None,
        };
        body_return(
            &Sources {
                object,
                bound,
                made: object.and(made),
                extended: object.and(extended),
                ..*sources
            },
            found,
        )?
    };
    // A body answering `self` answers the receiver, which is the object `owner` built.
    let made = if same { owner.made.clone() } else { returned };
    // **`self` is what the call was written on, which is what `owner` already is.** The same rule
    // [`resolved`] applies to RBS's `self`, refusal included: `owner.one()` answers nothing for a
    // union. This line makes `1.probe` an `Integer` and `"x".presence` a `String?`, not the class
    // the `def` is written in.
    let (classes, arguments) = if same {
        (vec![owner.one()?], owner.arguments.clone())
    } else {
        (classes, arguments)
    };
    let mut derivation = owner.derivation;
    derivation.absorb(inner);
    Some(Typed {
        classes,
        derivation,
        nilable,
        boolean,
        arguments,
        // Answered here, so it goes no further: the next link asks about the class this returns,
        // not the receiver two steps back.
        same: false,
        made,
        procs,
        bound: held,
    })
}

/// What a method returns, asked of the declaration alone, with where the answer came from: the one
/// place a surface asks it, so a margin and a card cannot tell a method's return two ways.
pub struct MethodReturn {
    pub typed: Typed,
    /// A signature declared it ([`Types::declared_return`]); otherwise its body was read.
    pub declared: bool,
}

/// What a method returns with no receiver and no call: a `def`'s margin and its card.
///
/// - **A signature first**, spelled as a label can spell it ([`Typed::declared_as`]).
/// - **Disputed by more than one Ruby body, the union a chain reads** ([`disputed`]), so the
///   margin does not state the signature where every call is a union.
/// - **Otherwise the body** ([`body_return`]).
#[must_use]
pub fn method_return(sources: &Sources<'_>, found: DeclarationId) -> Option<MethodReturn> {
    let graph = sources.graph;
    let Some(returns) = sources.types.declared_return(found) else {
        return body_return(sources, found).map(|typed| MethodReturn {
            typed,
            declared: false,
        });
    };
    let declared = Typed::declared_as(graph, returns)?;
    let typed = match graph.declarations().get(&found) {
        Some(declaration) if disputed_by_bodies(graph, declaration) => {
            match body_return(sources, found) {
                Some(body) => disputed(graph, declared, body),
                None => declared,
            }
        }
        _ => declared,
    };
    Some(MethodReturn {
        typed,
        declared: true,
    })
}

/// What **this `def`** returns, asked of the declaration alone.
///
/// [`from_body`] is this plus the chain that reached it. The other caller has no chain: an inlay
/// hint on a `def`'s own signature. One set of rules for both, so a margin and a card cannot
/// disagree about what a body returns.
fn body_return(sources: &Sources<'_>, found: DeclarationId) -> Option<Typed> {
    if sources.body_hops >= BODY_HOPS {
        return None;
    }
    let reads = &sources.memo.reads;
    let key = (found, sources.object, sources.bound, sources.made);
    let open = reads.stack.borrow().len();
    if reads.bodies.borrow().contains(&(key, open)) {
        return None;
    }
    reads.bodies.borrow_mut().push((key, open));
    let answer = read_exits(sources, found);
    reads.bodies.borrow_mut().pop();
    answer
}

// How many method bodies have been read since a test last asked ([`bodies_read`]).
//
// Counted, not timed, for `cursor::CLASSIFIED`'s reason: refusing a recursion at once and unrolling
// it [`BODY_HOPS`] times answer the same, and only the work tells them apart.
#[cfg(test)]
thread_local! {
    static BODIES_READ: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many bodies were read since the last call, and resets it.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn bodies_read() -> usize {
    BODIES_READ.replace(0)
}

/// Whether one of the method's Ruby `def`s writes `raise` or `fail` in its own body
/// ([`cursor::raises_in`]): the `!` on the card's return.
///
/// Read like [`read_exits`] reads the exits, from the same held shapes: a signature has no body,
/// and a `def` whose span an unsettled edit moved is not claimed.
#[must_use]
pub fn raises(sources: &Sources<'_>, found: DeclarationId) -> bool {
    let graph = sources.graph;
    locator::definitions_of(graph, found)
        .iter()
        .filter(|definition| matches!(definition, Definition::Method(_)))
        .filter_map(|definition| {
            let uri = graph.documents().get(definition.uri_id())?.uri();
            // A signature has no body to read.
            (!uri.ends_with(".rbs")).then_some((uri, definition))
        })
        .any(|(uri, definition)| {
            let Some(read) = sources.memo.reads.documents.of(uri, sources.read) else {
                return false;
            };
            read.rebase
                .span_to_buffer(ByteSpan {
                    start: definition.offset().start(),
                    end: definition.offset().end(),
                })
                .is_some_and(|span| {
                    read.shapes(uri, sources.held_exits)
                        .raising
                        .contains(&(span.start, span.end))
                })
        })
}

/// [`body_return`] once the read is known not to be a recursion: every exit of every definition.
fn read_exits(sources: &Sources<'_>, found: DeclarationId) -> Option<Typed> {
    #[cfg(test)]
    BODIES_READ.set(BODIES_READ.get() + 1);
    let graph = sources.graph;
    let name = graph.declarations().get(&found)?.name().to_owned();
    let deeper = Sources {
        body_hops: sources.body_hops + 1,
        ..*sources
    };
    if sources.types.blocked.contains(&found) {
        return made_from_block(&deeper, found, name);
    }

    // Sorted, not in graph order, as in [`assigned_to`]: an answer must not depend on which worker
    // indexed which file first. A member standing for a `def` reads that `def` alone.
    let mut written: Vec<(String, UriId, u32, u32)> = if sources.types.bodied.contains(&found) {
        own_defs(sources, found)?
    } else {
        locator::definitions_of(graph, found)
            .iter()
            .filter(|definition| matches!(definition, Definition::Method(_)))
            .filter_map(|definition| {
                let uri_id = *definition.uri_id();
                let offset = definition.offset();
                let uri = graph.documents().get(&uri_id)?.uri().to_owned();
                // **Skip RBS definitions.** An RBS `def:` line is a `Definition::Method` like a real
                // `def`, and `.gem_rbs_collection` adds one beside a Ruby method under the same name.
                // The read loop declines the *whole* method as soon as one definition has no exits, and
                // a signature parsed as Ruby never has any. So reading it would throw away a correct
                // answer. `declared_return` answers for signatures; this rung is only about `def`s with
                // Ruby behind them.
                if uri.ends_with(".rbs") {
                    return None;
                }
                Some((uri, uri_id, offset.start(), offset.end()))
            })
            .collect()
    };
    written.sort();
    written.dedup();
    if written.is_empty() {
        return reader_return(&deeper, found, &name);
    }

    // The three classes the fold uses, resolved once. With `[rbs]` off none is declared, so nothing
    // folds: the comparisons miss and every exit stays its own class.
    let folds = Folds::of(graph);
    // **Every exit is joined** ([`Join`]), facets included: one exit of `maybe` calls `predicate`, a
    // `bool`, and reading only its carrier class would relabel the method `true`. A bailing guard,
    // a missing `else` and a bare `return` each add a `nil` exit, which becomes a mark on the other
    // answer, or `NilClass` where every exit is `nil`.
    let mut join = Join::default();
    // Where the weakest exit's `def` was written, for the note: a union is only as good as its
    // worst half, and the note names that half's body. Its file and line are spelled once, after
    // the loop: a later exit replaces it, and a body read inside another is replaced by the outer.
    let mut noted: Option<(&str, Rc<Document>, u32)> = None;
    // **Whether every exit that adds a class is a bare `self`** ([`Typed::same`]). A `nil` exit
    // does not clear it: `self if present?` is the shape this is for.
    let mut only_self = true;
    for (uri, written_in, start, end) in &written {
        // Read once per document per request ([`Memo`]).
        let document = sources.memo.reads.documents.of(uri, sources.read)?;
        // The graph's span in buffer coordinates, as in `assigned_to`: this starts from a span the
        // graph recorded and must find it in text the user may have edited since. `None` is an
        // unsettled edit. Decline, as the whole module does, rather than read whichever `def` sits
        // at that offset now.
        let span = document.rebase.span_to_buffer(ByteSpan {
            start: *start,
            end: *end,
        })?;
        // **Missing from the map** is a span no `def` in this text starts and ends at, which an
        // unsettled edit can produce. **An empty entry** is a `def` with no readable exit. Both
        // decline.
        let exits = document
            .shapes(uri, sources.held_exits)
            .returns
            .get(&(span.start, span.end))?;
        if exits.is_empty() {
            return None;
        }
        // The `def`'s own scope, so `self` or a constant in the body resolves in the body's
        // nesting, not the caller's.
        //
        // Through `scope_at`, not `Scope::at`: `Scope::at` walks every definition in the document,
        // and this line runs once per `def` a request asks about.
        let scope = sources.scope_at(*written_in, *start);
        // **An exit this call cannot reach is no exit** ([`unreached`]), and a `def` left with none
        // hands back nothing, as one whose every exit raises.
        let mut reached = false;
        for exit in exits {
            // Parsed from that document's buffer; everything below keys the graph.
            let exit = exit.rebased(&document.rebase)?;
            if unreached(sources, *written_in, &exit) {
                continue;
            }
            let Some(typed) = method_receiver(&deeper, *written_in, &exit, &scope) else {
                // An exit a check leaves no value for never runs.
                if dead(&deeper, *written_in, &exit) {
                    continue;
                }
                return None;
            };
            reached = true;
            let adds_a_class = typed
                .classes()
                .iter()
                .any(|class| Some(*class) != folds.nil);
            only_self &= !adds_a_class || matches!(exit.unguarded(), Receiver::SelfObject(_));
            if join.add(typed, &folds) {
                noted = Some((uri, Rc::clone(&document), span.start));
            }
        }
        if !reached {
            return None;
        }
    }
    let mut folded = join.finish(&folds)?;
    let (uri, document, at) = noted?;
    // The outermost body is the one a reader opens, so it replaces any inner note. At depth 1 there
    // is none to replace.
    folded.derivation.body = Some(FromBody {
        method: name,
        file: where_written(sources, uri),
        line: line_of(&document.source, at),
    });
    // Only where some exit added a class: a body answering `NilClass` alone answers no receiver.
    folded.same = only_self
        && folded
            .classes()
            .iter()
            .any(|class| Some(*class) != folds.nil);
    Some(folded)
}

/// Where the `def` a generated member stands for is written ([`generated::OWN_DEF`]): each
/// definition's mapped place, as [`read_exits`] takes a `def`'s. A definition with no place refuses.
fn own_defs(sources: &Sources<'_>, found: DeclarationId) -> Option<Vec<(String, UriId, u32, u32)>> {
    locator::definitions_of(sources.graph, found)
        .iter()
        .filter(|definition| matches!(definition, Definition::Method(_)))
        .map(|definition| {
            let Origin::Declared(site) = sources
                .generated
                .origin(definition.uri_id(), definition.offset().start())
            else {
                return None;
            };
            Some((
                site.uri.clone(),
                UriId::from(site.uri.as_str()),
                site.full.0,
                site.full.1,
            ))
        })
        .collect()
}

/// The generated member standing for the `def` whose name is at `name` in `source`
/// ([`generated::OWN_DEF`]), where exactly one does: the method the `def` really defines, which
/// the margin and the card read in place of the one rubydex filed it under.
#[must_use]
pub fn own_def_member(
    sources: &Sources<'_>,
    source: &str,
    name: (u32, u32),
) -> Option<DeclarationId> {
    let graph = sources.graph;
    let mut members = sources
        .generated
        .generated_at(source, name)
        .into_iter()
        .filter_map(|(document, span)| {
            graph
                .documents()
                .get(&document)?
                .definitions()
                .iter()
                .filter_map(|id| graph.definitions().get(id))
                .filter(|definition| matches!(definition, Definition::Method(_)))
                .find(|definition| (span.0..span.1).contains(&definition.offset().start()))
                .and_then(|definition| graph.definition_to_declaration_id(definition).copied())
        })
        .filter(|member| sources.types.bodied.contains(member));
    let member = members.next()?;
    members.next().is_none().then_some(member)
}

/// What a member a call defines from its block returns ([`generated::BLOCK`]): that block's value,
/// read as a `def`'s body is.
///
/// - **The block is found at the member's own place**: the generated definition's mapping names
///   the call ([`Synthesized::origin`]), and the block written on it is read
///   ([`cursor::blocks_handed_back`]). A definition with no place, or a call whose block has moved
///   in an unsettled edit, refuses.
/// - **Every definition's block is joined**, as a method's `def`s are, and one exit nothing types
///   refuses the whole, as [`read_exits`] does.
/// - **`self` inside is what the block's own rules say at each exit** (a signature's
///   `[self: instance]` on the call), as for any expression in that text: the block is read where
///   it is written, not for the receiver.
/// - **The derivation names the call's line** ([`FromBody`]), the line a reader would open.
fn made_from_block(sources: &Sources<'_>, found: DeclarationId, name: String) -> Option<Typed> {
    let graph = sources.graph;
    let mut places: Vec<(String, u32)> = Vec::new();
    for definition in locator::definitions_of(graph, found) {
        if !matches!(definition, Definition::Method(_)) {
            continue;
        }
        let Origin::Declared(site) = sources
            .generated
            .origin(definition.uri_id(), definition.offset().start())
        else {
            return None;
        };
        places.push((site.uri.clone(), site.full.0));
    }
    // Sorted, as the `def`s are: an answer must not depend on indexing order.
    places.sort();
    places.dedup();
    let folds = Folds::of(graph);
    let mut join = Join::default();
    // Spelled once, after the loop, for [`read_exits`]' reason.
    let mut noted: Option<(&str, Rc<Document>, u32)> = None;
    for (uri, call) in &places {
        let uri_id = UriId::from(uri.as_str());
        let document = sources.memo.reads.documents.of(uri, sources.read)?;
        let at = document
            .rebase
            .span_to_buffer(ByteSpan {
                start: *call,
                end: *call,
            })?
            .start;
        let exits = document.blocks().get(&at)?;
        let scope = sources.scope_at(uri_id, *call);
        let mut reached = false;
        for exit in exits {
            let exit = exit.rebased(&document.rebase)?;
            if unreached(sources, uri_id, &exit) {
                continue;
            }
            reached = true;
            if join.add(method_receiver(sources, uri_id, &exit, &scope)?, &folds) {
                noted = Some((uri, Rc::clone(&document), at));
            }
        }
        if !reached {
            return None;
        }
    }
    let mut folded = join.finish(&folds)?;
    let (uri, document, at) = noted?;
    folded.derivation.body = Some(FromBody {
        method: name,
        file: where_written(sources, uri),
        line: line_of(&document.source, at),
    });
    Some(folded)
}

/// What an **`attr_reader`** returns: its instance variable, read on the object.
///
/// - **Why:** a reader has no body, so [`body_return`] had nothing to read, and every
///   `attr_reader :story` stopped a chain even where `@story` is typed.
/// - **The variable is read as [`instance_read`] reads `@story` in a method of the same class**:
///   every write any class of the object makes, narrowed to the object a body is read for
///   ([`Sources::object`]), with `nil` joining unless every `initialize` writes it. An
///   `attr_accessor`'s setter is a write nothing can type, so it answers nothing.
/// - **Which object's variable comes from [`scopes::readers`]**, the walk that places setters: 0
///   for a reader in a class body, 1 in `class << self`. A reader it does not place (in a block
///   straight in a namespace body, or on another object) answers nothing.
/// - **Only where no `def` of the name exists.** A `def` after the reader is the idiom's
///   override, and [`body_return`] reads it as before.
/// - **The derivation names the reader** ([`FromBody`]), the line a reader would open.
fn reader_return(sources: &Sources<'_>, found: DeclarationId, name: &str) -> Option<Typed> {
    let reads = &sources.memo.reads;
    let graph = sources.graph;
    // Sorted, as the `def`s are: an answer must not depend on indexing order.
    let mut declared: Vec<(String, UriId, u32, u32)> = locator::definitions_of(graph, found)
        .iter()
        .filter(|definition| {
            matches!(
                definition,
                Definition::AttrReader(_) | Definition::AttrAccessor(_)
            )
        })
        .filter_map(|definition| {
            let uri_id = *definition.uri_id();
            let uri = graph.documents().get(&uri_id)?.uri().to_owned();
            // A signature's `attr_reader` has no Ruby behind it to place a variable in.
            (!uri.ends_with(".rbs")).then(|| {
                (
                    uri,
                    uri_id,
                    definition.offset().start(),
                    definition.offset().end(),
                )
            })
        })
        .collect();
    declared.sort();
    declared.dedup();
    let mut typed = Vec::new();
    let mut first = None;
    for (uri, uri_id, start, end) in &declared {
        let document = reads.documents.of(uri, sources.read)?;
        let at = document
            .rebase
            .span_to_buffer(ByteSpan {
                start: *start,
                end: *end,
            })?
            .start;
        // The reader `scopes` placed at this name says which variable, on which object.
        let variables = &document.shapes(uri, sources.held_exits).variables;
        let reader = variables.reader_at(at)?;
        let reaching = cursor::Reaching {
            writes: Rc::from([]),
            nil: false,
            bound: None,
            instance: None,
            narrowed: Box::default(),
        };
        // No method: a reader is called, never dispatched, so no callback runs before it.
        let instance = cursor::InstanceRead {
            name: reader.name.clone(),
            level: Some(reader.level),
            set_here: false,
            loose: false,
            method: None,
            decided: None,
        };
        typed.push(instance_read(
            sources, *uri_id, &document, variables, at, &reaching, &instance,
        )?);
        first.get_or_insert_with(|| (where_written(sources, uri), line_of(&document.source, at)));
    }
    let (file, line) = first?;
    let mut answer = fold_reached(graph, typed, false)?;
    answer.derivation.body = Some(FromBody {
        method: name.to_owned(),
        file,
        line,
    });
    Some(answer)
}

/// The three classes the return fold uses, looked up once per ask.
///
/// `nil`, `true` and `false` are what Ruby returns without anyone naming a type. They are resolved
/// against the graph, not matched by name, because everything downstream keys the graph. A project
/// with signatures off declares none of them, so every fold becomes a no-op, not a wrong answer.
struct Folds {
    nil: Option<DeclarationId>,
    truth: Option<DeclarationId>,
    falsehood: Option<DeclarationId>,
}

impl Folds {
    fn of(graph: &Graph) -> Self {
        Self {
            nil: declared(graph, "NilClass"),
            truth: declared(graph, "TrueClass"),
            falsehood: declared(graph, "FalseClass"),
        }
    }

    /// `true` and `false` together are `bool`; one alone is itself.
    ///
    /// A method that can return either is a predicate, and RBS calls that `bool`. A method that
    /// only ever returns `true` is not a predicate, and `true` is both narrower and correct. So
    /// only the **pair** folds, which is what RBS' `bool` means.
    fn fold(
        &self,
        mut classes: Vec<DeclarationId>,
        carried: bool,
        derivation: Derivation,
    ) -> Option<Typed> {
        let pair = self.truth.is_some_and(|id| classes.contains(&id))
            && self.falsehood.is_some_and(|id| classes.contains(&id));
        if pair {
            // `TrueClass` stays as the carrier and `FalseClass` is dropped, as [`Return::Bool`]
            // does: either half answers a `.` identically, and the reader sees `bool` either way.
            classes.retain(|class| Some(*class) != self.falsehood);
        }
        let mut typed = Typed::over(classes, derivation)?;
        typed.boolean = pair || carried;
        Some(typed)
    }
}

/// One operand of a shortcut, split the way Ruby's truthiness splits a value.
///
/// - **Falsiness depends only on the class.** `nil` and `false` are the only falsy values, and each
///   is the sole instance of its class. So known classes mean a known branch, and `&&` can be
///   *executed* here, not declined or widened.
/// - **The same split [`Typed`] carries, read back out.** `nilable` is a folded-out `nil` and
///   `boolean` a folded-out `FalseClass`; both are put back before the operator runs.
#[derive(Default)]
struct Sides {
    /// Every class other than the three, all truthy.
    classes: Vec<DeclarationId>,
    /// `true` is one of the values this can be.
    truth: bool,
    nil: bool,
    falsehood: bool,
}

impl Sides {
    fn of(typed: &Typed, folds: &Folds) -> Self {
        let holds =
            |fold: Option<DeclarationId>| fold.is_some_and(|fold| typed.classes().contains(&fold));
        Self {
            classes: typed
                .classes()
                .iter()
                .copied()
                .filter(|class| ![folds.nil, folds.truth, folds.falsehood].contains(&Some(*class)))
                .collect(),
            truth: holds(folds.truth),
            // A `bool` carries `TrueClass` and means the pair, so its `false` is read back here.
            // See [`Typed::boolean`].
            falsehood: typed.boolean || holds(folds.falsehood),
            nil: typed.nilable || holds(folds.nil),
        }
    }

    /// This value or `other`: every class of either, and each of `true`, `false` and `nil` either
    /// can be.
    fn absorb(&mut self, other: Sides) {
        self.truth |= other.truth;
        self.falsehood |= other.falsehood;
        self.nil |= other.nil;
        for class in other.classes {
            if !self.classes.contains(&class) {
                self.classes.push(class);
            }
        }
    }

    /// Whether Ruby would take the false branch on this value. Never `false` together with
    /// [`Self::truthy`]: [`Typed`]'s class list is never empty, so a value is always one or the
    /// other, usually only one.
    fn falsy(&self) -> bool {
        self.nil || self.falsehood
    }

    fn truthy(&self) -> bool {
        self.truth || !self.classes.is_empty()
    }

    /// The parts reassembled as a type, through the same two folds as every answer.
    ///
    /// **A part with no declaration declines the whole value.** With `[rbs]` off there is no
    /// `NilClass`, and rebuilding `String | nil` as `String` would turn a switched-off table into a
    /// wrong answer instead of a missing one.
    fn typed(
        self,
        folds: &Folds,
        derivation: Derivation,
        arguments: Vec<Option<DeclarationId>>,
    ) -> Option<Typed> {
        let mut classes = self.classes;
        for (present, fold) in [(self.truth, folds.truth), (self.falsehood, folds.falsehood)] {
            if present {
                classes.push(fold?);
            }
        }
        if self.nil && folds.nil.is_none() {
            return None;
        }
        let folded = folds.fold(classes, false, derivation)?;
        // Arguments are kept only where the fold left one class ([`body_return`]'s rule): a union's
        // positions belong to one half, and nothing says which.
        let folded = match folded.one() {
            Some(_) => folded.holding(arguments),
            None => folded,
        };
        Some(if self.nil { folded.or_nil() } else { folded })
    }
}

/// Values any one of which can be the value at one place, joined into one type.
///
/// **The one rule for joining answers**, so every place several meet says the same thing: the
/// writes reaching a read ([`fold_reached`]), both halves of a `T?` or a `bool`
/// ([`either_half`]), a signature and the bodies disputing it ([`disputed`]), a method's exits
/// ([`read_exits`]) and a block's ([`block_return`]).
///
/// - **Classes are unioned, `nil` folds into the mark and `true` beside `false` is `bool`**,
///   through [`Sides`], however each value spelled it: a `?`, a `NilClass` of its own, a `bool`.
/// - **What a class holds is kept only where every value that adds a class agrees**
///   ([`agreed_arguments`]). `nil` holds nothing, so it has no vote.
/// - **The weakest value decides the tier** ([`weaker`]): a value that can be either is only as
///   good as its worse half. The first of equals is kept, and every assignment any value names.
/// - **What travels with one value is dropped**: [`Typed::same`] and [`Typed::made`] describe an
///   object, and a join has none.
#[derive(Default)]
struct Join {
    kept: Sides,
    /// What every value that adds a class says it holds; `None` until one does.
    arguments: Option<Vec<Option<DeclarationId>>>,
    derivation: Option<Derivation>,
    assignments: Vec<u32>,
    /// A value or a `nil` was added: a join of nothing is no answer.
    any: bool,
    /// The proc literals every value adding a class says it is ([`Typed::procs`]): `None` until
    /// one adds a class, `Some(None)` once one is anything else.
    procs: Option<Option<Vec<(UriId, u32)>>>,
    /// The method every value adding a class is bound to ([`Typed::bound`]), the same way: kept
    /// only while each is bound to the same method on the same classes.
    bound: Option<Option<Rc<Bound>>>,
}

impl Join {
    /// One more value. Answers whether it is now the weakest, the one whose provenance the answer
    /// carries.
    fn add(&mut self, typed: Typed, folds: &Folds) -> bool {
        let side = Sides::of(&typed, folds);
        if !side.classes.is_empty() || typed.boolean {
            self.procs = Some(match (self.procs.take(), &typed.procs) {
                (None, Some(held)) => Some(held.to_vec()),
                (Some(Some(mut seen)), Some(held)) => {
                    for literal in held.iter() {
                        if !seen.contains(literal) {
                            seen.push(*literal);
                        }
                    }
                    Some(seen)
                }
                _ => None,
            });
            self.bound = Some(match (self.bound.take(), &typed.bound) {
                (None, Some(held)) => Some(Rc::clone(held)),
                (Some(Some(seen)), Some(held))
                    if seen.method == held.method && seen.on.classes == held.on.classes =>
                {
                    Some(seen)
                }
                _ => None,
            });
        }
        if !side.classes.is_empty() {
            self.arguments = Some(match self.arguments.take() {
                None => typed.arguments.clone(),
                Some(held) => agreed_arguments(held, &typed.arguments),
            });
        }
        self.kept.absorb(side);
        self.any = true;
        self.assignments
            .extend(typed.derivation.assignments.iter().copied());
        let weakest = self
            .derivation
            .as_ref()
            .is_none_or(|held| weaker(&typed.derivation, held));
        if weakest {
            self.derivation = Some(typed.derivation);
        }
        weakest
    }

    /// A `nil` no value carries: an unwritten branch, or a read no write has reached yet.
    fn nil(&mut self) {
        self.kept.nil = true;
        self.any = true;
    }

    /// The joined type. `None` where nothing was added, or where a part has no declaration
    /// ([`Sides::typed`]).
    fn finish(self, folds: &Folds) -> Option<Typed> {
        if !self.any {
            return None;
        }
        let mut derivation = self.derivation.unwrap_or_default();
        let mut assignments = self.assignments;
        assignments.sort_unstable();
        assignments.dedup();
        derivation.assignments = assignments;
        if self.kept.classes.is_empty() && !self.kept.truth && !self.kept.falsehood {
            // `nil` alone: nothing to mark, so the answer is `NilClass` itself.
            return Some(Typed::of(folds.nil?, derivation));
        }
        let mut joined = self
            .kept
            .typed(folds, derivation, self.arguments.unwrap_or_default())?;
        joined.procs = self.procs.flatten().map(Rc::from);
        joined.bound = self.bound.flatten();
        Some(joined)
    }
}

/// Evaluate `&&` or `||` against the classes, rather than widen it into a union.
///
/// Ruby's rule: **`&&` returns the right operand when the left is truthy, and the left one
/// otherwise. `||` swaps the sides.** The operator is not a call; its value *is* one of its
/// operands. Only `nil` and `false` are falsy, so the left operand's **class** decides. Three exact
/// cases:
///
/// 1. **The left can only be falsy** (`nil`, `false`): `a && b` is `a`, `a || b` is `b`.
/// 2. **The left is never falsy** (a `String`, any instance): `a && b` is `b`, `a || b` is `a`.
/// 3. **The left can be either** (`String?`, `bool`): both branches, so a union of the taken side
///    with the untaken half of the left.
///
/// **The right operand is resolved only where it can be reached.** `record || raise(...)` answers
/// `record`'s class, because a receiver that cannot be `nil` never evaluates the right side. A
/// policy ending in `a? && b?` is the common case this types.
///
/// **A guessed left operand decides nothing.** Taking the right side alone because a guess says
/// the left is never (or always) falsy would hand back the right side's tier for an answer that
/// rests on the guess. So a guessed left always takes the both-branches arm, whose weakest-tier rule
/// keeps the guess on the answer.
fn shortcut(
    sources: &Sources<'_>,
    uri_id: UriId,
    left: &Receiver,
    right: &Receiver,
    and: bool,
    scope: &Scope,
) -> Option<Typed> {
    let folds = Folds::of(sources.graph);
    let held = method_receiver(sources, uri_id, left, scope)?;
    let side = Sides::of(&held, &folds);
    // The untaken branch is never resolved, so its operand costs nothing and needs no type.
    if and && !side.truthy() || !and && !side.falsy() {
        return Some(held);
    }
    // **`x || raise` is `x` where it is truthy**: the right side never hands anything back, so
    // only the left side's classes and `true` reach the end, without its `nil` and `false`. A left
    // side that is only ever falsy leaves nothing, and [`Sides::typed`] declines.
    if !and && cursor::never_returns(right) {
        let kept = Sides {
            nil: false,
            falsehood: false,
            ..side
        };
        let arguments = held.arguments.clone();
        return kept.typed(&folds, held.derivation, arguments);
    }
    let other = method_receiver(sources, uri_id, right, scope)?;
    reaching_end(held, side, other, and, &folds)
}

/// What `held && other` or `held || other` hands back once both operands are typed and the left
/// one may go either way ([`shortcut`], [`scoped_beside`]).
fn reaching_end(held: Typed, side: Sides, other: Typed, and: bool, folds: &Folds) -> Option<Typed> {
    let guessed = held.derivation.tier() == Tier::Guessed;
    if !guessed && (and && !side.falsy() || !and && !side.truthy()) {
        return Some(other);
    }
    // Both branches are live. The taken side is the right operand, whole. The other side is the
    // half of the left operand that reaches the end: its `nil` and `false` for `&&`, its classes
    // and `true` for `||`.
    let mut kept = Sides::of(&other, folds);
    let reaching = if and {
        Sides {
            nil: side.nil,
            falsehood: side.falsehood,
            ..Sides::default()
        }
    } else {
        Sides {
            classes: side.classes.clone(),
            truth: side.truth,
            ..Sides::default()
        }
    };
    kept.absorb(reaching);
    // Only a side that added a **class** can say what that class holds. The falsy half of a `&&`
    // adds `nil` and `false`, which hold nothing.
    let arguments = if and || side.classes.is_empty() {
        other.arguments.clone()
    } else {
        agreed_arguments(held.arguments.clone(), &other.arguments)
    };
    // The weakest operand decides the tier ([`body_return`]'s rule): a value that can be either is
    // only as good as its worse half.
    let derivation = if weaker(&other.derivation, &held.derivation) {
        other.derivation
    } else {
        held.derivation
    };
    kept.typed(folds, derivation, arguments)
}

/// Evaluate a unary `!` against the operand's classes, rather than look up a member called `!`.
///
/// Ruby's rule: `!x` is `false` when `x` is truthy and `true` when it is falsy, and only `nil` and
/// `false` are falsy. So the operand's class decides, as it does for `&&` ([`shortcut`]). Three
/// cases:
///
/// 1. **Only ever truthy** (anything but `NilClass` and `FalseClass`): `!x` is the literal `false`.
/// 2. **Only ever falsy** (`nil`, `false`, or exactly those two): `!x` is the literal `true`.
/// 3. **Either, or the operand does not resolve**: `!x` is `bool`. Ruby returns one of the two
///    whatever the receiver was, so this needs no typed operand, unlike a shortcut.
/// 4. **Only a name guess types the operand**: `bool` too, as if it did not resolve. Narrowing on
///    a guess would turn Ruby's own guarantee into a guessed `false`, which no hint draws, so
///    `!admin` would lose the label `!foo` has. Knowing more must not answer less.
///
/// Case 3 types the two commonest Rails predicates. `Object#blank?` is
/// `respond_to?(:empty?) ? !!empty? : false`, and one untyped arm declines a whole ternary. So
/// `blank?`, `present?` and every `def valid?; !errors.any?; end` depend on it.
///
/// **Why not a member lookup:** `!` is a real method and rubydex finds it. `vendor/rbs` declares
/// `TrueClass#!: () -> false`, and an unresolved `bool` carries `TrueClass`, so the lookup would
/// answer a confident `false` for an undetermined value. Deciding it here makes the `bool` carrier
/// safe. Nothing is lost: every `!` in `vendor/rbs` returns `true`, `false`, `bool` or `untyped`.
fn negated(sources: &Sources<'_>, uri_id: UriId, on: &Receiver, scope: &Scope) -> Option<Typed> {
    let folds = Folds::of(sources.graph);
    // Both halves: `bool`. Built through [`Sides::typed`] like every other answer, so with `[rbs]`
    // off (neither half declared) this declines rather than inventing a class, as the shortcut
    // does.
    let either = |derivation| {
        Sides {
            classes: Vec::new(),
            truth: true,
            falsehood: true,
            nil: false,
        }
        .typed(&folds, derivation, Vec::new())
    };
    // **The operand is read, but not needed.** A `?` here would lose case 3: `!` answers `bool` for
    // an operand nothing can type, because the guarantee is Ruby's. The tier is `Resolved` for the
    // same reason: it is the language's own rule, like `&&`. A guessed operand is case 4, and is
    // treated as none.
    let held = pending_aware(sources, || method_receiver(sources, uri_id, on, scope));
    // Still being answered inside a cycle: not unknown, just not known yet.
    if held.is_none() && pending(sources) {
        return None;
    }
    let Some(held) = held.filter(|held| held.derivation.tier() != Tier::Guessed) else {
        return either(Derivation::default());
    };
    let side = Sides::of(&held, &folds);
    // The operand's evidence carries over: `!` states a fact about a class, so an answer read off a
    // signature or an assignment says which.
    let derivation = held.derivation;
    match (side.truthy(), side.falsy()) {
        // Only ever truthy, so the negation is only ever `false`.
        (true, false) => Sides {
            classes: Vec::new(),
            truth: false,
            falsehood: true,
            nil: false,
        }
        .typed(&folds, derivation, Vec::new()),
        // Only ever falsy, so the negation is only ever `true`. `!nil` and `!false` reach this, and
        // so would a `String?` narrowed to `nil`.
        (false, true) => Sides {
            classes: Vec::new(),
            truth: true,
            falsehood: false,
            nil: false,
        }
        .typed(&folds, derivation, Vec::new()),
        // Either (a `bool`, a `String?`, any class list spanning both sides), so the honest answer
        // is the pair. `Sides` guarantees a value is one or the other, so the last arm is
        // unreachable in practice and folded in here, not left as an untestable branch.
        _ => either(derivation),
    }
}

/// What two exits agree their class holds: the positions that match.
///
/// [`Return::agreed`]'s rule for Ruby. Two bodies that both return an `Array` are one method
/// returning an `Array`, so a position they differ at is dropped and the class kept, rather than
/// declining the method over a facet no step reads. Lists of different lengths agree on nothing.
fn agreed_arguments(
    held: Vec<Option<DeclarationId>>,
    seen: &[Option<DeclarationId>],
) -> Vec<Option<DeclarationId>> {
    if held.len() != seen.len() {
        return Vec::new();
    }
    held.into_iter()
        .zip(seen)
        .map(|(held, seen)| held.filter(|held| Some(held) == seen.as_ref()))
        .collect()
}

/// Whether `candidate` rests on weaker evidence than `held`.
///
/// Only the tier is compared. Within a tier there is no defensible order; across tiers there is the
/// order the card prints.
fn weaker(candidate: &Derivation, held: &Derivation) -> bool {
    fn rank(derivation: &Derivation) -> u8 {
        match derivation.tier() {
            Tier::Resolved => 0,
            Tier::Derived => 1,
            Tier::Guessed => 2,
        }
    }
    rank(candidate) > rank(held)
}

/// What the block written on one call is handed, at one position.
///
/// - **[`returned_by`]'s twin.** The receiver is typed, rubydex finds the member, and the table is
///   asked about the declaration found, not the name written. Only the last step differs: the
///   block's parameter instead of the return. So `Story::Relation#each` and `#first` read one
///   signature two ways.
/// - **[`Return::Same`] means the receiver**, as for a return. `Kernel#tap` is
///   `() { (self) -> void } -> self`, so `"x".tap { |it| ... }` hands the block a `String`.
///
/// # A receiver that may be `nil`
///
/// [`returned_by`]'s rule, for the block: on a `T?` the call runs on whichever the value is, so
/// `nil.then { |v| }` hands `v` a `nil`. `NilClass` is asked the same question and the halves fold.
/// Where `NilClass` has no such member, `nil.each` raises, the block never runs on `nil`, and `T`'s
/// answer stands. A `&.` call never runs the block on `nil`.
fn yielded_by(
    sources: &Sources<'_>,
    uri_id: UriId,
    on: &Receiver,
    call: Call<'_>,
    handing: Handing<'_>,
    scope: &Scope,
) -> Option<Typed> {
    let owner = method_receiver(sources, uri_id, on, scope)?;
    // A private member hands a block nothing on a written receiver, as it returns nothing
    // ([`reach`]): `call.on_self` carries which.
    if !owner.nilable || call.safe {
        return yielded_on(sources, uri_id, owner, call, handing, scope);
    }
    let derivation = owner.derivation.clone();
    let carried = Typed {
        nilable: false,
        ..owner
    };
    let here = yielded_on(sources, uri_id, carried, call, handing, scope)?;
    let graph = sources.graph;
    // `nil`'s half, written on a receiver, never on `self`: where `NilClass` lacks the member or
    // keeps it private, `nil.each` raises, the block never runs on `nil`, and `T`'s answer stands.
    let there = declared(graph, "NilClass").and_then(|nil| {
        yielded_on(
            sources,
            uri_id,
            Typed::of(nil, derivation),
            Call {
                on_self: false,
                ..call
            },
            handing,
            scope,
        )
    });
    match there {
        Some(there) => either_half(graph, here, there),
        None => Some(here),
    }
}

/// [`yielded_by`] for one receiver, whatever its mark says.
fn yielded_on(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: Typed,
    call: Call<'_>,
    handing: Handing<'_>,
    scope: &Scope,
) -> Option<Typed> {
    let found = reach(
        sources,
        uri_id,
        owner.one()?,
        StringId::from(&call.member()),
        call.on_self,
    )?;
    // A signature first; where none says, the method's own Ruby `yield`s, for this call.
    if sources.types.yielded(found, handing.index).is_some() {
        return handed_to(sources, &owner, found, handing.index);
    }
    yielded_from_body(sources, uri_id, &owner, found, call, handing, scope)
}

/// Which block parameter a read asks about, and how the block binds what it is handed.
#[derive(Clone, Copy)]
struct Handing<'a> {
    /// Its position among the block's positional parameters.
    index: usize,
    /// Ruby unpacks one `Array` handed to this block ([`Receiver::Yielded`]'s field).
    spreads: bool,
    /// An optional parameter's default, for a `yield` that hands fewer values.
    default: Option<&'a Receiver>,
}

/// What a Ruby method hands its block at `index`, where no signature says: every `yield` in its
/// `def`s, read with this call's arguments bound, whichever ran.
///
/// - **Ruby's binding rules for a block**, not a method's: a `yield` handing fewer values than
///   `index` hands `nil` there, and extra values are dropped. One value handed to a block Ruby
///   unpacks ([`Receiver::Yielded::spreads`]) says nothing positional, and refuses.
/// - **Refused** where no `def` has Ruby behind it, a `def` never yields (the block never runs),
///   a site cannot be read, or any value is untyped or only guessed.
/// - **Read like a body** ([`read_body`]): the object the call is written on, and what it passed.
fn yielded_from_body(
    sources: &Sources<'_>,
    uri_id: UriId,
    owner: &Typed,
    found: DeclarationId,
    call: Call<'_>,
    handing: Handing<'_>,
    scope: &Scope,
) -> Option<Typed> {
    if sources.body_hops >= BODY_HOPS {
        return None;
    }
    let Handing {
        index,
        spreads,
        default,
    } = handing;
    let graph = sources.graph;
    let reads = &sources.memo.reads;
    let binding = passed_to(
        sources,
        uri_id,
        call.arity,
        call.written,
        &Block::None,
        scope,
    )
    .and_then(|passed| Binding::of(sources, found, &passed))
    .map(Rc::new);
    let made = owner.made.as_ref().map(|made| Rc::clone(&made.0));
    for held in binding.iter().chain(made.iter()) {
        reads
            .bindings
            .borrow_mut()
            .entry(held.key)
            .or_insert_with(|| Rc::clone(held));
    }
    let object = match owner.classes.as_slice() {
        [one] if !owner.nilable => Some(*one),
        _ => None,
    };
    let deeper = Sources {
        object,
        bound: binding.as_ref().map(|binding| binding.key),
        made: object.and(made.as_ref().map(|made| made.key)),
        body_hops: sources.body_hops + 1,
        ..*sources
    };
    let mut written: Vec<(String, UriId, u32, u32)> = locator::definitions_of(graph, found)
        .iter()
        .filter(|definition| matches!(definition, Definition::Method(_)))
        .filter_map(|definition| {
            let uri_id = *definition.uri_id();
            let uri = graph.documents().get(&uri_id)?.uri().to_owned();
            let offset = definition.offset();
            (!uri.ends_with(".rbs")).then_some((uri, uri_id, offset.start(), offset.end()))
        })
        .collect();
    written.sort();
    written.dedup();
    let folds = Folds::of(graph);
    let mut join = Join::default();
    let mut noted: Option<(String, u32)> = None;
    for (uri, written_in, start, end) in &written {
        let document = reads.documents.of(uri, sources.read)?;
        let span = document.rebase.span_to_buffer(ByteSpan {
            start: *start,
            end: *end,
        })?;
        let sites = document
            .shapes(uri, sources.held_exits)
            .yields
            .get(&(span.start, span.end))?
            .clone()?;
        // The `yield`s are read in the method's scope; a default in the caller's, with the block.
        let within = sources.scope_at(*written_in, *start);
        for handed in &sites {
            if spreads && handed.len() == 1 {
                return None;
            }
            let typed = match handed.get(index) {
                Some(value) => {
                    let value = value.rebased(&document.rebase)?;
                    let typed = method_receiver(&deeper, *written_in, &value, &within)?;
                    if typed.derivation.tier() == Tier::Guessed {
                        return None;
                    }
                    typed
                }
                // Handed fewer: an optional parameter holds its default, read where the block is
                // written, and a required one `nil`.
                None => match default {
                    Some(value) => {
                        let typed = method_receiver(sources, uri_id, value, scope)?;
                        if typed.derivation.tier() == Tier::Guessed {
                            return None;
                        }
                        typed
                    }
                    None => Typed::of(declared(graph, "NilClass")?, Derivation::default()),
                },
            };
            join.add(typed, &folds);
            noted = Some((
                where_written(sources, uri),
                line_of(&document.source, span.start),
            ));
        }
    }
    let mut folded = join.finish(&folds)?;
    let (file, line) = noted?;
    folded.derivation.yielded = Some(FromBody {
        method: graph.declarations().get(&found)?.name().to_owned(),
        file,
        line,
    });
    Some(folded)
}

/// What a signature says the method `found` hands its block at `index`, on `owner`.
fn handed_to(
    sources: &Sources<'_>,
    owner: &Typed,
    found: DeclarationId,
    index: usize,
) -> Option<Typed> {
    let graph = sources.graph;
    let yielded = sources.types.yielded(found, index)?;
    // The same answers the return side reads, from the same functions (see [`resolved`]). What the
    // receiver holds is what `Array#each`'s `{ (E element) -> void }` asks for. A block parameter
    // that is itself a generic (`each_slice`'s `(Array[E] slice)`) carries its element into the
    // block below.
    let declaration = resolved(sources, &yielded.of, owner, None)?;
    let arguments = held_by_return(sources, &yielded.of, owner, None);

    let mut derivation = owner.derivation.clone();
    derivation
        .signatures
        .push(graph.declarations().get(&found)?.name().to_owned());
    Some(
        Typed::of(declaration, derivation)
            .faceted(yielded)
            .holding(arguments),
    )
}

/// A receiver that is only a name, answered by the one rung below the graph: its spelling.
///
/// None of this runs until rubydex and every derivation above come back empty
/// ([`locator::resolve_typed`]), so a guess never displaces an answer the code states. An instance
/// variable's writes are asked before this, as a read ([`instance_read`]); what reaches here is a
/// read they could not type.
fn named(sources: &Sources<'_>, name: &str, scope: &Scope) -> Option<Typed> {
    if !sources.guess {
        return None;
    }
    guessed(sources.graph, name, scope)
}

/// The class a template's path names: the controller Rails renders it from, or the mailer.
///
/// `None` for anything that is not a template of an existing class, and when `[rails] views` is off
/// (see [`renderer_documents`]).
fn rendered_by(sources: &Sources<'_>, uri_id: UriId) -> Option<views::RenderedBy> {
    let graph = sources.graph;
    let path = DocUri::from_graph_uri(graph.documents().get(&uri_id)?.uri())?.to_file_path()?;
    sources.views.rendered_by(graph, &path)
}

/// The class a template's path names, and every document that class is written in.
///
/// Shared by [`instance_read`] (what the assignment *is*, through [`rendered_by`]) and
/// [`renderer_writes`] (*where* it is written). Both must agree on the class, or a card and a jump
/// at the same `@story` would name two classes.
///
/// 1. **[`views::Views::rendered_by`] decides the class, not this module.** Rails renders a
///    template from the controller its directory names, and a mailer's views from the mailer
///    (`app/views/user_mailer/welcome.html.erb` is `UserMailer`). The mailer half needs a gate: the
///    mailer list the view-context pass already holds. One copy of the rule keeps a card, a jump
///    and a view context from naming three classes for one path.
/// 2. **`[rails] views` is the gate, travelling with the value.** `rendered_by` reads a path, not a
///    call, so it would apply to any project with `app/views/`, Rails or not. A project that turned
///    the convention off gets a switched-off `Views`, and `rendered_by` declines. This does not
///    read [`Sources::features`](Sources): one gate, not two flags that could disagree.
/// 3. **A partial is not special-cased.** `rails::controller_of` reads the directory, so
///    `stories/_story.html.erb` names `StoriesController`, and `shared/_header.html.erb` names a
///    `SharedController` that does not exist, which `rendered_by` refuses (and a `Shared` mailer
///    too, unless one exists). A path names one class or none; there is no evidence in a path to
///    rank candidates with.
/// 4. **Sorted, not graph order.** A class reopened across files is answered from whichever
///    document declares the variable, so the answer must not depend on indexing order.
fn renderer_documents(
    sources: &Sources<'_>,
    uri_id: UriId,
    name: &str,
) -> Option<(views::RenderedBy, Vec<(String, UriId)>)> {
    let graph = sources.graph;
    // A local or a receiverless call is not an instance variable, and only instance variables are
    // handed from a controller or mailer to a template.
    if !name.starts_with('@') {
        return None;
    }
    // The exact name. A template whose class does not exist answers nothing, rather than reaching
    // for a similarly spelled one.
    let rendered = rendered_by(sources, uri_id)?;

    let mut documents: Vec<(String, UriId)> = locator::definitions_of(graph, rendered.declaration)
        .iter()
        .filter_map(|definition| {
            let id = *definition.uri_id();
            Some((graph.documents().get(&id)?.uri().to_owned(), id))
        })
        .collect();
    documents.sort();
    documents.dedup();
    Some((rendered, documents))
}

/// Every place a template's instance variable is **written**, in the class its path names.
///
/// [`instance_read`] is the card's half; this is the jump's. Same convention, same class. The card
/// folds every class of the controller's object; the jump names the controller's own writes, the
/// lines a reader of the template means by "where is this set".
///
/// - **Every write, typed or not**, because `@stories = Story.where(live: true)` is a line a reader
///   wants to land on even when this crate cannot type it. [`scopes::writes_to`] also tells a
///   `def self.`'s same-named `@story` apart.
/// - **The spans are in the controller's *buffer*; nothing is rebased.** They come from the text
///   [`Sources::read`](Sources) returned (the open buffer, else the file), and the caller measures
///   them against that same text.
/// - **One entry per file, none empty**, because the caller reads each file it is handed.
#[must_use]
pub fn renderer_writes(
    sources: &Sources<'_>,
    uri_id: UriId,
    name: &str,
) -> Vec<(String, Vec<(u32, u32)>)> {
    let Some((rendered, documents)) = renderer_documents(sources, uri_id, name) else {
        return Vec::new();
    };
    documents
        .into_iter()
        .filter_map(|(uri, _)| {
            let (source, _) = (sources.read)(&uri)?;
            let writes: Vec<(u32, u32)> = scopes::writes_to(&source, &rendered.name, name)
                .into_iter()
                .map(|at| (at.start, at.end))
                .collect();
            (!writes.is_empty()).then_some((uri, writes))
        })
        .collect()
}

/// Whether a document is Ruby, the one kind that can assign an instance variable.
///
/// A class is also declared by its signatures and by what ya-lsp generates for it, and neither
/// writes a variable, so neither is read for one. Every other document of the class is, and one
/// that cannot be read refuses the answer: it may hold the write that decides it.
fn writes_ruby(uri: &str) -> bool {
    uri.starts_with("file:") && !uri.ends_with(".rbs")
}

/// The one-based line `offset` is on, in a file only this module ever reads.
fn line_of(source: &str, offset: u32) -> u32 {
    let offset = (offset as usize).min(source.len());
    u32::try_from(
        source[..offset]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1,
    )
    .unwrap_or(u32::MAX)
}

/// The last rung: a class spelled like the receiver, resolved in the cursor's nesting.
///
/// - **`@user` is a `User`, `person` a `Person`, `first_name` a `FirstName`.** Rails and most Ruby
///   name variables after their classes, which is why this is worth having.
/// - **A guess in the strict sense.** Nothing in the code says so. The card and the completion row
///   both say the answer was guessed, and [`Sources::guess`] turns it off.
/// - **Resolved outwards through the nesting, like a constant.** `User` inside
///   `Admin::UsersController` tries `Admin::UsersController::User`, then `Admin::User`, then
///   `User`. A template's nesting is the top level, where Rails models live.
fn guessed(graph: &Graph, name: &str, scope: &Scope) -> Option<Typed> {
    let class = class_named_like(name)?;
    let nesting = scope
        .nesting_id(graph)
        .and_then(|id| graph.declarations().get(&id))
        .map(Declaration::name);
    Some(Typed::of(
        constant_named(graph, nesting, &class)?,
        Derivation {
            guess: Some(name.to_owned()),
            ..Derivation::default()
        },
    ))
}

/// `@user_session` -> `UserSession`, and `None` for a spelling no constant could have.
///
/// - **The inflection is [`rails::camelize`]'s, shared, not copied.** `user_sessions/` naming
///   `UserSessionsController` and `@user_session` naming `UserSession` are one rule, and two
///   inflectors would disagree.
/// - **The guard is Ruby's.** `valid?`, `[]` or `+` is a punctuation method name, which no constant
///   has. Leading sigils are stripped (`@@count` is a `Count`), and the rest must be a name.
fn class_named_like(receiver: &str) -> Option<String> {
    let bare = receiver.trim_start_matches('@');
    if bare.is_empty()
        || !bare
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    rails::camelize(bare)
}

/// A constant resolved the way Ruby resolves one: outwards through the nesting, then the top level.
///
/// - **Not [`locator::locate`]'s job.** That reads a reference rubydex resolved against the real
///   nesting and ancestors. Here nobody wrote `User`, so there is no reference; only the lexical
///   half of the lookup is left.
/// - **Ancestors are not walked.** A guess reaching through an inheritance chain has more ways to
///   be wrong and no more ways to be right.
fn constant_named(graph: &Graph, nesting: Option<&str>, name: &str) -> Option<DeclarationId> {
    let mut scopes: Vec<&str> = nesting
        .map(|path| path.split("::").collect())
        .unwrap_or_default();
    while !scopes.is_empty() {
        if let Some(found) = declared(graph, &format!("{}::{name}", scopes.join("::"))) {
            return Some(found);
        }
        scopes.pop();
    }
    declared(graph, name)
}

/// The declaration a constant whose path ends at `offset` resolves to, as [`Receiver::Constant`]
/// and every other constant `cursor` records are placed.
///
/// - **Through the locator, not a re-derived constant lookup:** rubydex resolved the reference
///   under the cursor against the real nesting and ancestors, which beats a name match.
/// - **Located at the path's last byte, not at `offset`.** A reference that begins at the offset
///   outranks one that ends there (`locator::narrowest`), and in `ENV["HOME"]` the `[]` call's
///   begins exactly where `ENV` ends, so the constant was never found and `ENV[…]` was untyped.
#[must_use]
pub fn constant_at(
    graph: &Indexed,
    uri_id: UriId,
    offset: u32,
    layout: environment::Layout<'_>,
) -> Option<DeclarationId> {
    locator::locate_held(graph, uri_id, offset.checked_sub(1)?)
        .into_iter()
        .find_map(|located| match located.target {
            // Tree fence off, outside fence on: the pair every `locator::resolve` caller takes. A
            // constant declared in a spec is still the code saying so. A constant declared only
            // outside the project is a type this project does not have, and offering its members in
            // `app/` would offer a receiver the application could never build.
            locator::Target::Constant(_) => locator::resolve(
                graph,
                &located,
                environment::Fence::uses(locator::uri_of(graph, uri_id), layout),
            )
            .declarations
            .into_iter()
            .next(),
            _ => None,
        })
}

/// The **model** a receiver is about, for [`Return::Element`] and [`Return::Collection`].
///
/// - **Two spellings, both made by this crate or rubydex.** `Story::Relation` is the class this
///   crate invents for a collection; `Story::<Story>` is rubydex's name for a singleton class. So
///   the model is read off the receiver's *name*, the only thing the two share.
/// - **Anything else answers `None`, and that is the safety.** These returns are declared only on
///   the relation base class and a model's class side. Any other receiver reached them through an
///   unintended ancestor, and naming the receiver would be a guess. `None` stops the chain.
fn model_of(graph: &Graph, receiver: DeclarationId) -> Option<String> {
    let name = graph.declarations().get(&receiver)?.name();
    if let Some(element) = generated::element_of(name) {
        return Some(element.to_owned());
    }
    name.rsplit_once("::<").map(|(class, _)| class.to_owned())
}

/// The declaration a name refers to, when the graph holds one.
///
/// A `DeclarationId` is a hash of the name, so the key needs no lookup. The lookup still runs,
/// because an id for a never-indexed declaration is well formed and answers nothing.
#[must_use]
pub fn declared(graph: &Graph, name: &str) -> Option<DeclarationId> {
    let id = DeclarationId::from(name);
    graph.declarations().contains_key(&id).then_some(id)
}

/// The declaration a [`Return::Class`]'s `name` refers to, searched outward from `scope` unless the
/// signature wrote it `::`-absolute.
///
/// The same walk as Ruby's constant lookup, which RBS follows: the enclosing scope, then each outer
/// scope, then the top level. `Error` inside `class Errors` under `module ActiveModel` tries:
///
/// 1. `ActiveModel::Errors::Error`, which almost never exists but must win when it does, as in
///    Ruby;
/// 2. `ActiveModel::Error`, a sibling of `Errors`, where real cases resolve;
/// 3. bare `Error`.
///
/// An empty `scope` means no search, `declared`'s rule: the name was written `::`-absolute, or this
/// module built it from `owner.name` and knows it is exact.
#[must_use]
fn declared_in(graph: &Graph, name: &str, scope: &str) -> Option<DeclarationId> {
    if scope.is_empty() {
        return declared(graph, name);
    }
    let mut outer = scope;
    loop {
        if let Some(found) = declared(graph, &format!("{outer}::{name}")) {
            return Some(found);
        }
        match outer.rfind("::") {
            Some(at) => outer = &outer[..at],
            None => return declared(graph, name),
        }
    }
}

/// What a constant holds, for its card: what its signature says, else what the Ruby that assigns it
/// builds. The two rungs `Receiver::Constant` asks past the class object, which a class or a module
/// is instead, and whose card is its own.
pub(super) fn constant_type(sources: &Sources<'_>, constant: DeclarationId) -> Option<Typed> {
    held_by(sources, constant).or_else(|| assigned_to(sources, constant))
}

/// What a signature says this constant holds, as a receiver's type.
///
/// - **Rung two: a signature was read**, so it is *derived* and the card says so. The only rung
///   reached from a `Receiver::Constant`, which is otherwise a class object (the name is the type)
///   or nothing.
/// - **The class is looked up, not trusted.** A signature may name a class this workspace never
///   declares. Where the lookup misses, the caller falls through to the arms below, so the rung
///   only adds answers.
fn held_by(sources: &Sources<'_>, constant: DeclarationId) -> Option<Typed> {
    let held = sources.types.held(constant)?;
    let declaration = declared(sources.graph, &held.class)?;
    Some(Typed::of(
        declaration,
        Derivation {
            constant: Some(held.constant.to_string()),
            ..Derivation::default()
        },
    ))
}

/// What the Ruby that **assigns** this constant says it holds.
///
/// Rung two (an assignment was read), so it is *derived* and the card says so. The Ruby sibling of
/// [`held_by`]: `ENV` is typed by Ruby's own `sig/`, while an application's config constant is
/// typed by the line that builds it, since no shipped signature mentions it.
///
/// 1. **Read from the file the graph says declares it, not by name.** rubydex files a
///    `Definition::Constant` under the span of its name, and [`cursor::constant_assignment`] finds
///    that exact span, so two `HANDLE`s in two namespaces never answer for each other.
/// 2. **The last assignment that produces a type wins**, last by document then by offset (the order
///    below, walked backwards), as for instance variables. The derivation names the file and
///    line.
///    Assigning a constant twice is a Ruby warning, not a case worth its own policy.
/// 3. **The assignment resolves in its own document's coordinates and nesting.**
///    `CONFIG = Settings.new` inside `module Store` names `Store::Settings`; the *caller's* nesting
///    would resolve it elsewhere or nowhere.
fn assigned_to(sources: &Sources<'_>, constant: DeclarationId) -> Option<Typed> {
    // The cycle guard. See `Sources::constant_hops`: this rung can come back to the constant it
    // started from.
    if sources.constant_hops >= CONSTANT_HOPS {
        return None;
    }
    let graph = sources.graph;
    let name = graph.declarations().get(&constant)?.name().to_owned();
    let deeper = Sources {
        constant_hops: sources.constant_hops + 1,
        ..*sources
    };

    // A constant reopened across files is answered in a stable order: sorted, not graph order, so
    // the answer does not depend on indexing order (as in `renderer_documents`).
    let mut written: Vec<(String, UriId, u32, u32)> = locator::definitions_of(graph, constant)
        .iter()
        .filter(|definition| matches!(definition, Definition::Constant(_)))
        .filter_map(|definition| {
            let uri_id = *definition.uri_id();
            let offset = definition.offset();
            let uri = graph.documents().get(&uri_id)?.uri().to_owned();
            Some((uri, uri_id, offset.start(), offset.end()))
        })
        .collect();
    written.sort();
    written.dedup();

    for (uri, written_in, start, end) in written.iter().rev() {
        let Some((source, rebase)) = (sources.read)(uri) else {
            continue;
        };
        // **The graph's span, in the buffer's coordinates.** The opposite direction from
        // `instance_read`: that rung finds writes in the buffer and translates them *into* the
        // graph; this one starts from a span the graph recorded and must find it in text the user
        // may have edited since. `None` is a span overlapping an unsettled edit. Falling
        // through is how every rung here declines, rather than reading whatever constant is at that
        // offset now.
        let Some(found) = rebase.span_to_buffer(ByteSpan {
            start: *start,
            end: *end,
        }) else {
            continue;
        };
        let (from, to) = (found.start, found.end);
        let Some(receiver) = cursor::constant_assignment(&source, (from, to)) else {
            continue;
        };
        // Parsed from that document's *buffer*; everything below keys the graph.
        let Some(receiver) = receiver.rebased(&rebase) else {
            continue;
        };
        let scope = sources.scope_at(*written_in, *start);
        let Some(mut typed) = method_receiver(&deeper, *written_in, &receiver, &scope) else {
            continue;
        };
        typed.derivation.assigned_constant = Some(FromAssignment {
            constant: name.clone(),
            file: where_written(sources, uri),
            // `from`, not `start`: this names a line for a reader in the text just read, the buffer
            // (the same rule as in `instance_read`).
            line: line_of(&source, from),
        });
        return Some(typed);
    }
    None
}

/// The file `uri` names, as the shortest path that still identifies it for a reader.
///
/// - **Workspace-relative when the file is in the workspace**, the case it is written for: an
///   initializer eight directories down is unreadable as an absolute path.
/// - **The whole path otherwise.** That is a gem's file: long, mostly a version number, but still
///   something an editor can open, which a bare file name is not.
/// - **Never empty.** A URI this cannot parse is printed as itself, so the footnote always names a
///   place.
fn where_written(sources: &Sources<'_>, uri: &str) -> String {
    let Some(path) = DocUri::from_graph_uri(uri).and_then(|uri| uri.to_file_path()) else {
        return uri.to_owned();
    };
    let root = DocUri::from_graph_uri(sources.layout.root).and_then(|root| root.to_file_path());
    match root.and_then(|root| path.strip_prefix(root).ok().map(Path::to_path_buf)) {
        Some(relative) => relative.display().to_string(),
        None => path.display().to_string(),
    }
}

/// Whether rubydex invented this namespace because something named it and nothing defined it.
///
/// Not folded into [`singleton_of`]: `completion`'s `Distance` calls that one to *rank* rows, not
/// to decide what a receiver is, and the two want different answers.
#[must_use]
fn is_todo(graph: &Graph, id: DeclarationId) -> bool {
    matches!(
        graph.declarations().get(&id),
        Some(Declaration::Namespace(Namespace::Todo(_)))
    )
}

#[must_use]
pub fn singleton_of(graph: &Graph, id: DeclarationId) -> Option<DeclarationId> {
    match graph.declarations().get(&id)? {
        Declaration::Namespace(namespace) => namespace.singleton_class().copied(),
        _ => None,
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {

    use proptest::prelude::*;

    use super::*;
    use crate::analysis::testing::*;

    proptest! {
        /// [`spells`] is `str::contains` made faster, never a different question: an empty name, a
        /// name across a multibyte character and a text shorter than the name included.
        #[test]
        fn spelling_is_containing(text in "[a-c@_é]{0,24}", name in "[a-c@_é]{0,4}") {
            prop_assert_eq!(spells(&text, &name), text.contains(name.as_str()));
        }
    }

    /// What a signature says each constant holds, for the constants it says anything about.
    ///
    /// The refusals matter most. A constant is *one* thing, so a union names no class to offer, and
    /// `untyped` names nothing. Both fall through to the older arms, which is what keeps the rung
    /// additive.
    /// `Attribute.new` where `Attribute = Attributes::Attribute` builds the class the alias names,
    /// as arel's `Arel::Table#[]` does: the alias holds no member of its own.
    #[test]
    fn new_on_a_constant_alias_builds_the_class_it_names() {
        let source = "\
module Nodes
  class Match
    def name
      \"x\"
    end
  end

  Alias = Match
  Again = Alias
end

b = Nodes::Again.new.name
";
        let (mut harness, uri) = with_rbs(source, "class String\nend\n");
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "    def name -> String\nb: String = Nodes::Again.new.name"
        );
    }

    #[test]
    fn what_a_signature_says_a_constant_holds() {
        let mut types = Types::new();
        assert!(types.harvest(
            "file:///sig/constants.rbs",
            "\
ENV: RBS::Unnamed::ENVClass
RUBY_VERSION: String
ARGV: Array[String]
MAYBE: String?
UNKNOWN: untyped
EITHER: String | Symbol
LITERAL: 3
NOTHING: nil

class Float
  INFINITY: Float
end

module Deep
  class Inner
    HANDLE: String
  end
end
"
        ));

        let held: Vec<(&str, Option<&str>)> = [
            "ENV",
            "RUBY_VERSION",
            // A generic's head, for `class_of`'s reason: `Array[String]` and `Array[Integer]` reach
            // the same declarations, and this module does not answer the element type here.
            "ARGV",
            // `String?` is `String | nil`: the same one inexact entry the return side has.
            "MAYBE",
            "UNKNOWN",
            "EITHER",
            // A literal names one class exactly (`LITERAL: 3` holds an `Integer`), read through the
            // same `class_of` arm as the return side. It only fills a `None`: the `Todo`, the
            // singleton and the assignment below are still asked wherever this answers nothing.
            "LITERAL",
            "NOTHING",
            // The constant's own nesting is part of the key, so two classes' `INFINITY`s are two
            // keys.
            "Float::INFINITY",
            "Deep::Inner::HANDLE",
            "INFINITY",
        ]
        .into_iter()
        .map(|name| {
            (
                name,
                types
                    .held(DeclarationId::from(name))
                    .map(|held| &*held.class),
            )
        })
        .collect();

        assert_eq!(
            held,
            [
                ("ENV", Some("RBS::Unnamed::ENVClass")),
                ("RUBY_VERSION", Some("String")),
                ("ARGV", Some("Array")),
                ("MAYBE", Some("String")),
                ("UNKNOWN", None),
                ("EITHER", None),
                ("LITERAL", Some("Integer")),
                ("NOTHING", None),
                ("Float::INFINITY", Some("Float")),
                ("Deep::Inner::HANDLE", Some("String")),
                ("INFINITY", None),
            ]
        );

        // `instance` and `class` cannot reach this: RBS's grammar refuses them in a constant's
        // type, and rejects the whole document, not the one line. That is why this is a separate
        // assertion, not another `None` row above: a harvest returning `false` would take the other
        // constants with it.
        assert!(!Types::new().harvest(
            "file:///sig/holder.rbs",
            "class Holder\n  SELFISH: instance\nend\n"
        ));
    }

    /// The table built from one RBS document, as sorted `method -> class` lines.
    ///
    /// - **One row per call shape, not per method**, because that is the table's key:
    ///   `Text#round/0` is a call with no arguments, `/1` one argument, `/2+` two or more, and a
    ///   trailing ` { }` marks the block side.
    /// - **Printed, not asserted per entry.** The policy is a set of decisions, and reading them
    ///   side by side shows one arm answering where its neighbour does not.
    fn harvested(source: &str) -> Vec<(String, String)> {
        // The names are rebuilt here, because the table is keyed by a hash that cannot be printed
        // back. Every name a test asks about must be spelled the way the harvest spelled it, which
        // is the property under test.
        let mut types = Types::new();
        types.harvest("file:///sig/one.rbs", source);
        let mut rows: Vec<(String, String)> = Vec::new();
        for name in candidate_names(source) {
            let Some(overloads) = types.returns.get(&DeclarationId::from(name.as_str())) else {
                continue;
            };
            // The rubydex key carries empty parentheses; the call's own shape follows here, and two
            // sets side by side would read as one.
            let called = name.strip_suffix("()").unwrap_or(&name);
            for (label, arity) in probes(overloads) {
                let plain = overloads.plain.at(arity);
                let with_block = overloads.with_block.at(arity);
                if let Some(returns) = plain {
                    rows.push((format!("{called}{label}"), spelled(returns)));
                }
                // Only where the block changes the answer, which is what the fixtures are about.
                // Most methods answer the same either way, and a second identical row is noise.
                if let Some(returns) = with_block
                    && with_block != plain
                {
                    rows.push((format!("{called}{label} {{ }}"), spelled(returns)));
                }
            }
        }
        rows.sort();
        rows
    }

    /// What the block of every method in one document is handed, as `method[index] -> class`.
    fn yielded_by_document(source: &str) -> Vec<(String, String)> {
        let mut types = Types::new();
        types.harvest("file:///sig/one.rbs", source);
        let mut rows: Vec<(String, String)> = Vec::new();
        for name in candidate_names(source) {
            let Some(parameters) = types.yields.get(&DeclarationId::from(name.as_str())) else {
                continue;
            };
            let called = name.strip_suffix("()").unwrap_or(&name);
            for (index, parameter) in parameters.iter().enumerate() {
                rows.push((
                    format!("{called}[{index}]"),
                    parameter
                        .as_ref()
                        .map_or_else(|| "-".to_owned(), |returns| spelled(returns).to_string()),
                ));
            }
        }
        rows.sort();
        rows
    }

    /// What a signature says its block receives, and every shape that says nothing.
    #[test]
    fn what_a_block_is_handed() {
        let source = "\
class Relation
  def each: () { (Story) -> void } -> Relation
  def each_with_index: () { (Story, Integer) -> void } -> Relation
  def tap: () { (self) -> void } -> self
  def optional: () ?{ (Story) -> void } -> Relation
  def untyped_block: () { (untyped) -> void } -> Relation
  def no_block: () -> Relation
  def untyped_return: () { (Story) -> untyped } -> untyped
  def no_parameters: () { () -> void } -> Relation
  def disagreeing: () { (Story) -> void } -> Relation
                 | () { (Integer) -> void } -> Relation
  def agreeing: () { (Story) -> void } -> Relation
              | (Integer) { (Story) -> void } -> Relation
end
";
        assert_eq!(
            yielded_by_document(source),
            vec![
                // Parameters stay in order, so `|_, index|` reaches the second.
                ("Relation#agreeing[0]".to_owned(), "Story".to_owned()),
                ("Relation#each[0]".to_owned(), "Story".to_owned()),
                ("Relation#each_with_index[0]".to_owned(), "Story".to_owned()),
                (
                    "Relation#each_with_index[1]".to_owned(),
                    "Integer".to_owned()
                ),
                // `?{ }` is read too: the call that reaches this wrote a block, so what the
                // signature says that block receives applies.
                ("Relation#optional[0]".to_owned(), "Story".to_owned()),
                // `self` means the receiver, as it does for a return type.
                ("Relation#tap[0]".to_owned(), "self".to_owned()),
                // The tables are independent: a method whose *return* is refused still says what
                // its block receives. So a relation could declare `map` (whose element type is a
                // block's return, untypable) and still type the block's parameter.
                ("Relation#untyped_return[0]".to_owned(), "Story".to_owned()),
            ]
        );
        // The absences are the test: a method with no block, a block with no parameters, a refused
        // parameter, and above all two arms disagreeing about what the block is handed.
    }

    #[test]
    fn a_type_variable_is_the_receivers_own_argument_and_a_methods_own_is_not() {
        // `E` is `class Held[E]`'s, so the block gets whatever a `Held` holds. `U` is the
        // *method's*, bound by the call, so it stays refused. `Elem` belongs to the module, whose
        // own list the position counts along; the caller checks that list is the receiver's, which
        // is why `Held[0]` and `Walked[0]` are different rows.
        let source = "\
class Held[E]
  def each: () { (E) -> void } -> self
  def mapped: [U] () { (E) -> U } -> Array[U]
  def collected: [U] () { (U) -> void } -> self
  def shadowed: [E] () { (E) -> void } -> self
end

module Walked[Elem]
  def walk: () { (Elem) -> void } -> self
end
";
        assert_eq!(
            yielded_by_document(source),
            vec![
                ("Held#each[0]".to_owned(), "Held[0]".to_owned()),
                ("Held#mapped[0]".to_owned(), "Held[0]".to_owned()),
                ("Walked#walk[0]".to_owned(), "Walked[0]".to_owned()),
            ]
        );
        // Absent, each for its own reason: `collected`'s `U` is not in the class's list, and
        // `shadowed` declares its own `E`, which drops the whole list for that arm.
    }

    #[test]
    fn an_alias_is_the_method_it_renames_in_every_table() {
        // Core RBS writes many members this way. The kind comes from whether `self.` was written,
        // and both sides of an `alias` share it, so the singleton pair files under the singleton's
        // own name.
        let source = "\
class Ledger
  def write: () -> String
  alias put write
  def each: () { (Integer) -> void } -> self
  alias each_entry each
  def self.load: () -> Ledger
  alias self.parse self.load
  alias dangling nothing_declares_this
end
";
        let mut types = Types::new();
        types.harvest("file:///sig/one.rbs", source);
        let says = |method: &str| {
            types
                .returns(DeclarationId::from(method), Arity::Exactly(0), false)
                .map_or_else(|| "(none)".to_owned(), spelled)
        };
        assert_eq!(says("Ledger#put()"), "String");
        assert_eq!(says("Ledger::<Ledger>#parse()"), "Ledger");
        // The block table is copied too, independently: `each_entry` is `each`.
        assert_eq!(
            types.yielded(DeclarationId::from("Ledger#each_entry()"), 0),
            types.yielded(DeclarationId::from("Ledger#each()"), 0)
        );
        // An alias of an undeclared name copies nothing and files nothing, as does an alias whose
        // target is in another document.
        assert_eq!(says("Ledger#dangling()"), "(none)");
    }

    /// Every arity worth asking about: each one an arm names exactly, plus one past the last, where
    /// only a rest parameter answers.
    fn probes(overloads: &Overloads) -> Vec<(String, Arity)> {
        let exact = overloads
            .plain
            .by_arity
            .len()
            .max(overloads.with_block.by_arity.len());
        let mut probes: Vec<(String, Arity)> = (0..exact)
            .map(|written| (format!("/{written}"), Arity::Exactly(written as u32)))
            .collect();
        probes.push((format!("/{exact}+"), Arity::Exactly(exact as u32)));
        probes
    }

    /// A [`Returned`] as the table's tests read it. `self` stays `self`, because that is what the
    /// table holds (resolving it needs a receiver). The `nil` mark is spelled as a reader sees it,
    /// since it is part of the stored answer.
    fn spelled(returns: &Returned) -> String {
        let of = spelled_return(&returns.of);
        if returns.nilable {
            format!("{of}?")
        } else {
            of
        }
    }

    /// One [`Return`], including what it holds: `Array[String]`, or `Array[Array[0]]` where the
    /// position is still a question about the receiver.
    ///
    /// A refused position prints as `_`, which no RBS type spells: it means the position was kept
    /// but not named, which differs from having no argument list.
    fn spelled_return(of: &Return) -> String {
        match of {
            Return::Class {
                name, arguments, ..
            } if arguments.is_empty() => name.to_string(),
            Return::Class {
                name, arguments, ..
            } => {
                let written: Vec<String> = arguments
                    .iter()
                    .map(|argument| argument.as_ref().map_or("_".to_owned(), spelled_return))
                    .collect();
                format!("{name}[{}]", written.join(", "))
            }
            Return::Bool => "bool".to_owned(),
            Return::Same => "self".to_owned(),
            Return::Element => ELEMENT.to_owned(),
            Return::Collection => COLLECTION.to_owned(),
            // Spelled as RBS wrote it. The table holds the *position*; the name is what a signature
            // reader would look for.
            Return::Parameter { at, of } => format!("{of}[{at}]"),
            // `{ }` for the block that wrote it, again a spelling no RBS type has. The table does
            // not hold the variable's name (`U`), and two arms with differently named variables are
            // the same answer here.
            Return::Block => "{ }".to_owned(),
            // `(1)` for the argument at 1, again a spelling no RBS type has.
            Return::Argument { at } => format!("({at})"),
            Return::Union(members) => members
                .iter()
                .map(spelled_return)
                .collect::<Vec<_>>()
                .join(" | "),
            Return::Written => WRITTEN.to_owned(),
            Return::Shared => SHARED.to_owned(),
            Return::Held => HELD.to_owned(),
            Return::Scoped => SCOPED.to_owned(),
            Return::Forwarded { through, method } => {
                format!("{FORWARDED}[\"{through}\", \"{method}\"]")
            }
        }
    }

    /// Every name the harvest could have filed a method under, from the same document.
    ///
    /// A second, simpler walk, so the assertions can name a method rather than a hash. Deliberately
    /// not shared with the harvest.
    fn candidate_names(source: &str) -> Vec<String> {
        struct Names {
            nesting: Vec<String>,
            found: Vec<String>,
        }
        impl Visit for Names {
            fn visit_class_node(&mut self, node: &ClassNode<'_>) {
                self.nesting
                    .push(qualified(&self.nesting, &type_name(&node.name())));
                ruby_rbs::node::visit_class_node(self, node);
                self.nesting.pop();
            }
            fn visit_module_node(&mut self, node: &ModuleNode<'_>) {
                self.nesting
                    .push(qualified(&self.nesting, &type_name(&node.name())));
                ruby_rbs::node::visit_module_node(self, node);
                self.nesting.pop();
            }
            fn visit_method_definition_node(&mut self, node: &MethodDefinitionNode<'_>) {
                let Some(owner) = self.nesting.last() else {
                    return;
                };
                let symbol = node.name();
                let name = symbol.as_str();
                if !matches!(node.kind(), MethodDefinitionKind::Singleton) {
                    self.found.push(format!("{owner}#{name}()"));
                }
                if !matches!(node.kind(), MethodDefinitionKind::Instance) {
                    self.found
                        .push(format!("{}#{name}()", singleton_name(owner)));
                }
            }
        }
        fn qualified(nesting: &[String], written: &Written) -> String {
            match (written.absolute, nesting.last()) {
                (true, _) | (false, None) => written.path.clone(),
                (false, Some(outer)) => format!("{outer}::{}", written.path),
            }
        }
        let Ok(signature) = parse(source) else {
            return Vec::new();
        };
        let mut names = Names {
            nesting: Vec::new(),
            found: Vec::new(),
        };
        names.visit(&signature.as_node());
        names.found
    }

    fn drawn(source: &str) -> String {
        harvested(source)
            .into_iter()
            .map(|(method, class)| format!("{method} -> {class}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_whole_policy_in_one_document() {
        // Every arm of `class_of` side by side, so what is taken and what is dropped reads as one
        // decision.
        //
        // - **Two arms are folds, each written both ways**, because RBS writes both: `String?` =
        //   `String | nil`, and `bool` = `true | false`.
        // - **A union with two classes left after the fold** is kept whole (`Return::Union`),
        //   `nil` riding beside it as the mark (`a_wide_union`). A call answers each member
        //   and joins them.
        // - **The five literals are not folds.** Each names one class, so `() -> false` is
        //   `FalseClass` outright. They print as the carrier the table holds; `render::typed` draws
        //   a lone `TrueClass` as `true`, and `render`'s tests pin that.
        let source = "\
class Policy
  def plain: () -> String
  def generic: () -> Array[Integer]
  def optional: () -> String?
  def me: () -> self
  def made: () -> instance
  def klass: () -> class
  def named_singleton: () -> singleton(Policy)
  def qualified: () -> Enumerator::Lazy
  def a_union: () -> (String | Integer)
  def a_bool: () -> bool
  def a_spelled_bool: () -> (true | false)
  def a_nil_union: () -> (String | nil)
  def a_nilable_bool: () -> bool?
  def a_wide_union: () -> (String | Integer | nil)
  def nothing: () -> void
  def anything: () -> untyped
  def nil_only: () -> nil
  def a_proc: () -> ^() -> void
  def a_tuple: () -> [String, Integer]
  def a_record: () -> { name: String }
  def an_interface: () -> _ToS
  def a_symbol_literal: () -> :symbol
  def a_string_literal: () -> \"x\"
  def an_integer_literal: () -> 0
  def a_bool_literal_true: () -> true
  def a_bool_literal_false: () -> false
end
";
        assert_eq!(
            drawn(source),
            "\
Policy#a_bool/0 -> bool
Policy#a_bool_literal_false/0 -> FalseClass
Policy#a_bool_literal_true/0 -> TrueClass
Policy#a_nil_union/0 -> String?
Policy#a_nilable_bool/0 -> bool?
Policy#a_spelled_bool/0 -> bool
Policy#a_string_literal/0 -> String
Policy#a_symbol_literal/0 -> Symbol
Policy#a_tuple/0 -> Array
Policy#a_union/0 -> String | Integer
Policy#a_wide_union/0 -> String | Integer?
Policy#an_integer_literal/0 -> Integer
Policy#generic/0 -> Array[Integer]
Policy#klass/0 -> Policy::<Policy>
Policy#made/0 -> Policy
Policy#me/0 -> self
Policy#named_singleton/0 -> Policy::<Policy>
Policy#optional/0 -> String?
Policy#plain/0 -> String
Policy#qualified/0 -> Enumerator::Lazy"
        );
    }

    #[test]
    fn a_singleton_method_is_filed_on_the_singleton_class() {
        assert_eq!(
            drawn("class Widget\n  def self.build: () -> Widget\n  def self.me: () -> self\nend\n"),
            "\
Widget::<Widget>#build/0 -> Widget
Widget::<Widget>#me/0 -> self"
        );
    }

    #[test]
    fn instance_on_a_singleton_method_is_the_class_and_not_its_singleton() {
        // `def self.new: () -> instance` is how every RBS constructor is written, and the one place
        // `self` and `instance` differ by more than a word.
        assert_eq!(
            drawn("class Widget\n  def self.new: () -> instance\nend\n"),
            "Widget::<Widget>#new/0 -> Widget"
        );
    }

    #[test]
    fn a_module_function_is_filed_on_both_sides() {
        // `def self?.` declares the method twice and rubydex files two declarations, so both must
        // be in the table or half the calls answer nothing.
        assert_eq!(
            drawn("module Kernel\n  def self?.format: () -> String\nend\n"),
            "\
Kernel#format/0 -> String
Kernel::<Kernel>#format/0 -> String"
        );
    }

    #[test]
    fn nesting_is_how_rubydex_qualifies_it() {
        assert_eq!(
            drawn(
                "module Shelf\n  class Book\n    def title: () -> String\n    def self.open: () -> instance\n  end\nend\n"
            ),
            "\
Shelf::Book#title/0 -> String
Shelf::Book::<Book>#open/0 -> Shelf::Book"
        );
    }

    #[test]
    fn an_absolute_name_ignores_what_it_is_written_inside() {
        assert_eq!(
            drawn("module Outer\n  class ::Free\n    def x: () -> String\n  end\nend\n"),
            "Free#x/0 -> String"
        );
    }

    #[test]
    fn overloads_have_to_agree() {
        // Two arms naming one class is one answer; two naming two classes is a union, which is
        // dropped. Both arms take one argument, so a one-argument call reaches them. A call with
        // none reaches neither (`a_call_no_arm_accepts_is_answered_for_by_none_of_them`).
        assert_eq!(
            drawn(
                "class Text\n  def sub: (String) -> String\n         | (Regexp) -> String\nend\n"
            ),
            "Text#sub/1 -> String"
        );
        // Two *blockless* arms naming two classes: the union this rule is for. With a block it
        // would not be one (see `a_block_tells_two_overloads_apart`).
        assert_eq!(
            drawn(
                "class Text\n  def each: (Integer) -> Enumerator\n          | (String) -> Array\nend\n"
            ),
            ""
        );
        // One usable arm beside a refused one is still refused: the method can return something the
        // table cannot name.
        assert_eq!(
            drawn("class Text\n  def maybe: () -> String\n           | () -> void\nend\n"),
            ""
        );
    }

    #[test]
    fn a_block_tells_two_overloads_apart() {
        // A common shape in core, and not a union: whether the caller wrote a block picks the arm,
        // and the cursor has already read that. Both answers are exact.
        assert_eq!(
            drawn(
                "class Text\n  def bytes: () -> Array[Integer]\n           | () { (Integer byte) -> void } -> self\nend\n"
            ),
            "\
Text#bytes/0 -> Array[Integer]
Text#bytes/0 { } -> self"
        );
        // An arm with a *required* block answers only the block side; a call without one reaches
        // nothing.
        assert_eq!(
            drawn("class Text\n  def each: () { (String) -> void } -> self\nend\n"),
            "Text#each/0 { } -> self"
        );
        // `?{ ... }` is optional, so its arm applies either way and both sides answer the same.
        assert_eq!(
            drawn("class Text\n  def count: () ?{ (String) -> bool } -> Integer\nend\n"),
            "Text#count/0 -> Integer"
        );
        // The rule does not rescue a side that disagrees with itself: two blockless arms naming two
        // classes are still a union.
        assert_eq!(
            drawn(
                "class Text\n  def mixed: (Integer) -> Enumerator\n           | (String) -> Array\n           | () { (String) -> void } -> self\nend\n"
            ),
            "Text#mixed/0 { } -> self"
        );
    }

    #[test]
    fn an_arity_tells_two_overloads_apart() {
        // Arity, the block's other half. `Float#round` is the shape: as one partition, `Integer`
        // and a union disagree; by arity, each side agrees with itself. A fact, not a guess, like
        // `"x".bytes.`.
        assert_eq!(
            drawn(
                "class Text\n  def round: (?half: Symbol) -> Integer\n          | (Integer digits, ?half: Symbol) -> (Integer | Float)\nend\n"
            ),
            "Text#round/0 -> Integer\nText#round/1 -> Integer | Float"
        );
        // Keywords are not positional on either side: the arm above declares only `?half:`, and
        // `3.7.round(half: :up)` writes only `half:`. `cursor::arity_of` is the half of that rule
        // this module cannot see.
        //
        // Both sides of a split can answer, with different classes. `Array#first` has this shape in
        // real signatures, where the zero-argument arm is also a type variable. See `types.md`.
        assert_eq!(
            drawn(
                "class Text\n  def first: (Integer count) -> Array\n          | () -> String\nend\n"
            ),
            "\
Text#first/0 -> String
Text#first/1 -> Array"
        );
    }

    #[test]
    fn an_optional_positional_applies_to_every_arity_it_covers() {
        // `?String` is to arity what `?{ }` is to the block: one arm answering on both sides of the
        // split.
        assert_eq!(
            drawn("class Text\n  def strip: (?String chars) -> String\nend\n"),
            "\
Text#strip/0 -> String
Text#strip/1 -> String"
        );
        // A rest parameter has no most, so it answers past every exact arity (the `+` row).
        // `(Integer, *String)` starts at one and never stops.
        assert_eq!(
            drawn("class Text\n  def join: (Integer, *String) -> String\nend\n"),
            "\
Text#join/1 -> String
Text#join/2+ -> String"
        );
    }

    #[test]
    fn a_call_no_arm_accepts_is_answered_for_by_none_of_them() {
        // What keeps this a reading of the signature, not a guess: a call writing an argument no
        // arm takes reaches no arm, not the nearest one. `"x".sub.` is not a `String`, and RBS says
        // so.
        assert_eq!(
            drawn("class Text\n  def sub: (String) -> String\nend\n"),
            "Text#sub/1 -> String"
        );
        // Over a thousand methods in Ruby's signatures require an argument, so a zero-argument call
        // reaches no arm and answers nothing, rather than the only arm there is. `types.md` has the
        // decision.
        let mut types = Types::new();
        types.harvest(
            "file:///sig/text.rbs",
            "class Text\n  def sub: (String) -> String\nend\n",
        );
        let id = DeclarationId::from("Text#sub()");
        assert_eq!(types.returns(id, Arity::Exactly(0), false), None);
        assert_eq!(types.returns(id, Arity::Exactly(2), false), None);
        // A call whose arguments cannot be counted gets what every arm agrees on. `foo.sub(*args).`
        // is why this is a partition, not a filter.
        assert_eq!(
            types.returns(id, Arity::Unknown, false),
            Some(&Returned::plain(Return::Class {
                name: "String".into(),
                scope: "Text".into(),
                arguments: Box::default(),
            }))
        );
    }

    #[test]
    fn a_required_keyword_rules_its_arm_out_of_every_call_that_writes_none() {
        // Ruby raises on a call missing a required keyword, so a call writing no keywords never
        // runs that arm, at a counted arity or past every one. And a partition only a keyword hash
        // reaches still keeps the row: `match` disagrees for one positional, but a
        // hash is not a `Symbol`, so a keyword call reaches only the `bool` arm.
        let mut types = Types::new();
        types.harvest(
            "file:///sig/loader.rbs",
            "class Loader\n  def read: (String, headers: true) -> Integer\n           | (String) -> String\n  \
             def many: (*String, headers: true) -> Integer\n          | (*String) -> String\n  \
             def match: (Symbol) -> Integer\n           | (untyped) -> bool\nend\n",
        );
        let answer = |method: &str, arity: Arity| {
            types
                .returns(DeclarationId::from(method), arity, false)
                .map_or_else(|| "(none)".to_owned(), spelled)
        };
        assert_eq!(answer("Loader#read()", Arity::Exactly(1)), "String");
        assert_eq!(answer("Loader#read()", Arity::Keyed(1)), "Integer");
        assert_eq!(answer("Loader#many()", Arity::Exactly(3)), "String");
        assert_eq!(answer("Loader#many()", Arity::Keyed(3)), "(none)");
        assert_eq!(answer("Loader#match()", Arity::Exactly(1)), "(none)");
        assert_eq!(answer("Loader#match()", Arity::Keyed(0)), "bool");
    }

    #[test]
    fn an_interfaces_members_are_not_in_the_table() {
        // `signatures::without_interfaces`' rule, enforced on the walk: an interface's methods
        // never enter the graph, so a row for one would be keyed by a declaration that does not
        // exist, or worse, by the enclosing class's.
        assert_eq!(
            drawn(
                "class Array\n  interface _Rand\n    def rand: () -> Integer\n  end\n  def sample: () -> String\nend\n"
            ),
            "Array#sample/0 -> String"
        );
    }

    #[test]
    fn a_return_a_label_can_carry_is_one_the_whole_signature_agrees_on() {
        // `returns` is asked about a call and reads what it wrote. This is asked about the `def`,
        // where there is no call, so both sides of the block must agree, or a label on the
        // declaration would be wrong half the time.
        let mut types = Types::new();
        types.harvest(
            "file:///sig/text.rbs",
            "class Text\n  \
             def upcase: () -> String\n  \
             def bytes: () -> Array[Integer]\n         \
             | () { (Integer byte) -> void } -> self\n  \
             def each: () { (String line) -> void } -> self\n\
             end\n",
        );

        // One side only, so nothing disagrees with it.
        assert_eq!(
            types.declared_return(DeclarationId::from("Text#upcase()")),
            Some(&Returned::plain(Return::Class {
                name: "String".into(),
                scope: "Text".into(),
                arguments: Box::default(),
            }))
        );
        // The block side only: every arm declares a block, so the plain side has no arms at all,
        // not contradicting ones.
        assert_eq!(
            types.declared_return(DeclarationId::from("Text#each()")),
            Some(&Returned::plain(Return::Same))
        );
        // Both sides, disagreeing: exactly the shape a label must refuse.
        assert_eq!(
            types.declared_return(DeclarationId::from("Text#bytes()")),
            None
        );
        // A method the table has never seen.
        assert_eq!(
            types.declared_return(DeclarationId::from("Text#gone()")),
            None
        );
    }

    #[test]
    fn the_tier_is_the_weakest_rung_the_answer_rests_on() {
        // The one place the three tiers are a value, not a sentence. A guess beats everything above
        // it (a chain through three signatures that *ended* at a guess is a guess), so that field
        // is tested first.
        assert_eq!(Derivation::default().tier(), Tier::Resolved);

        let signature = Derivation {
            signatures: vec!["String#upcase()".to_owned()],
            ..Derivation::default()
        };
        assert_eq!(signature.tier(), Tier::Derived);

        let guessed = Derivation {
            guess: Some("person".to_owned()),
            ..signature
        };
        assert_eq!(guessed.tier(), Tier::Guessed);

        // Every other kind of provenance is derived. Each is named here: the destructuring in
        // `tier` makes a new kind a compile error, and this test says which tier the new arm should
        // give.
        for derivation in [
            Derivation {
                assignments: vec![4],
                ..Derivation::default()
            },
            Derivation {
                renderer: Some(FromRenderer {
                    renderer: "StoriesController".to_owned(),
                    controller: true,
                    lines: vec![9],
                }),
                ..Derivation::default()
            },
            Derivation {
                named_by: Some("before_save".to_owned()),
                ..Derivation::default()
            },
            Derivation {
                view: Some(views::InView::Helper),
                ..Derivation::default()
            },
            Derivation {
                closure: Some("ScopeParser".to_owned()),
                ..Derivation::default()
            },
        ] {
            assert_eq!(derivation.tier(), Tier::Derived, "{derivation:?}");
        }
    }

    #[test]
    fn a_signature_that_does_not_parse_contributes_nothing() {
        let mut types = Types::new();
        types.harvest("file:///sig/foo.rbs", "class Foo\n  def");
        assert!(types.is_empty());
    }

    #[test]
    fn harvesting_the_same_document_twice_replaces_only_that_document_s_arms() {
        // **One rule, two directions.** Re-reading an edited buffer must not leave both the old and
        // new claims. Two *different* documents declaring one method must not have the second
        // replace the first. The document key tells the cases apart.
        let integer = Some(&Returned::plain(Return::Class {
            name: "Integer".into(),
            scope: "Foo".into(),
            arguments: Box::default(),
        }));
        let bar = |types: &Types| {
            types
                .returns(DeclarationId::from("Foo#bar()"), Arity::Exactly(0), false)
                .cloned()
        };

        // One document, read twice: the second reading is what the method says.
        let mut types = Types::new();
        types.harvest(
            "file:///sig/foo.rbs",
            "class Foo\n  def bar: () -> String\nend\n",
        );
        types.harvest(
            "file:///sig/foo.rbs",
            "class Foo\n  def bar: () -> Integer\nend\n",
        );
        assert_eq!(types.len(), 1);
        assert_eq!(bar(&types).as_ref(), integer);

        // Two documents saying the same two things: they disagree, so the method answers nothing.
        let mut types = Types::new();
        types.harvest(
            "file:///sig/one.rbs",
            "class Foo\n  def bar: () -> String\nend\n",
        );
        types.harvest(
            "file:///sig/two.rbs",
            "class Foo\n  def bar: () -> Integer\nend\n",
        );
        assert_eq!(bar(&types), None);

        // Re-reading one of *those* replaces only its own arms: `two.rbs` now agrees with
        // `one.rbs`, so the method answers again, with nothing left of the old claim.
        types.harvest(
            "file:///sig/two.rbs",
            "class Foo\n  def bar: () -> Integer\nend\n",
        );
        assert_eq!(bar(&types), None);
        types.harvest(
            "file:///sig/one.rbs",
            "class Foo\n  def bar: () -> Integer\nend\n",
        );
        assert_eq!(bar(&types).as_ref(), integer);

        types.clear();
        assert!(types.is_empty());
    }

    #[test]
    fn a_document_that_declares_only_untyped_does_not_veto_one_that_declares_a_class() {
        // `-> untyped` is RBS for *no claim*, and the generator copying a gem's
        // `module ClassMethods` writes it for every member. Two generated documents reopen
        // `ActiveRecord::Base`: the query interface declares
        // `def self.find_by!: (*untyped) -> Element`, and the concern beside it
        // `(*untyped) -> untyped`. If the silent arm voted, `find`, `find_by` and `find_by!` would
        // lose their type, and every `@story = Story.find(...)` would fall from *derived* to
        // *guessed*.
        let string = Some(&Returned::plain(Return::Class {
            name: "String".into(),
            scope: "Foo".into(),
            arguments: Box::default(),
        }));
        let bar = |types: &Types, name: &str| {
            types
                .returns(
                    DeclarationId::from(&format!("Foo#{name}()")),
                    Arity::Exactly(0),
                    false,
                )
                .cloned()
        };

        let mut types = Types::new();
        types.harvest(
            "file:///sig/one.rbs",
            "class Foo\n  def bar: (*untyped) -> String\nend\n",
        );
        types.harvest(
            "file:///sig/two.rbs",
            "class Foo\n  def bar: (*untyped) -> untyped\nend\n",
        );
        assert_eq!(bar(&types, "bar").as_ref(), string);

        // **Both orders**: a document read first must not silence one read second either.
        let mut types = Types::new();
        types.harvest(
            "file:///sig/two.rbs",
            "class Foo\n  def bar: (*untyped) -> untyped\nend\n",
        );
        types.harvest(
            "file:///sig/one.rbs",
            "class Foo\n  def bar: (*untyped) -> String\nend\n",
        );
        assert_eq!(bar(&types, "bar").as_ref(), string);

        // **A document that made a claim can still disagree.** One readable arm and one unreadable
        // is an overload list with a hole, and the hole might be the arm this call reaches, so the
        // method answers nothing. The rule that keeps an overload list honest, inside one document.
        let mut types = Types::new();
        types.harvest(
            "file:///sig/one.rbs",
            "class Foo\n  def half: (*untyped) -> String\nend\n",
        );
        types.harvest(
            "file:///sig/two.rbs",
            "class Foo\n  def half: (*untyped) -> String | (*untyped) -> untyped\nend\n",
        );
        assert_eq!(bar(&types, "half"), None);

        // A method that *only* the silent document declares still has no row.
        let mut types = Types::new();
        types.harvest(
            "file:///sig/two.rbs",
            "class Foo\n  def quiet: (*untyped) -> untyped\nend\n",
        );
        assert_eq!(bar(&types, "quiet"), None);
        assert!(types.is_empty());
    }

    #[test]
    fn a_method_outside_every_declaration_has_no_owner() {
        // The other way a method can have no owner: rbs parses a bare `def` at the top level of a
        // file, and there is no declaration to key it under. (Interfaces, the first way, are
        // covered above.)
        let mut types = Types::new();
        types.harvest(
            "file:///sig/free.rbs",
            "interface _Free\n  def x: () -> String\nend\n",
        );
        assert!(types.is_empty());

        // The walk's guard is for a shape rbs never produces: a `def` outside every declaration.
        // Asserted here, because "rbs refuses this" is the whole reason that arm is unreachable.
        assert!(parse("def free: () -> String\n").is_err());
        types.harvest("file:///sig/toplevel.rbs", "def free: () -> String\n");
        assert!(types.is_empty());
    }

    #[test]
    fn rubydex_spells_a_signature_the_way_this_keys_it() {
        // The contract the module rests on, checked against a real graph rather than this module's
        // own idea of it. Every name here is one the harvest builds; a rubydex rename would make
        // every lookup miss silently.
        use rubydex::{indexing::LanguageId, model::graph::Graph, resolution::Resolver};

        use super::super::indexer;

        let source = "\
module Shelf
  class Book
    def title: () -> String
    def self.open: () -> instance
    attr_reader author: String
    attr_accessor pages: Integer
    attr_reader self.shelf: String
  end
end

module Util
  def self?.format: () -> String
end
";
        let mut graph = Graph::new();
        assert!(indexer::index_source(
            &mut graph,
            "file:///pin.rbs",
            source,
            &LanguageId::Rbs
        ));
        Resolver::new(&mut graph).resolve();

        let mut types = Types::new();
        types.harvest("file:///sig/one.rbs", source);

        let mut missing: Vec<&str> = Vec::new();
        for name in [
            "Shelf::Book#title()",
            "Shelf::Book::<Book>#open()",
            "Shelf::Book#author()",
            "Shelf::Book#pages()",
            "Shelf::Book::<Book>#shelf()",
            "Util#format()",
            "Util::<Util>#format()",
        ] {
            let id = DeclarationId::from(name);
            if !graph.declarations().contains_key(&id)
                || types.returns(id, Arity::Exactly(0), false).is_none()
            {
                missing.push(name);
            }
        }
        assert!(missing.is_empty(), "{missing:?}");
    }

    /// Every receiver spelling the last rung is asked about, with the class it would try.
    ///
    /// A table rather than a test each, because the `None`s matter most: a guess that fires on
    /// `valid?` or `[]` offers a class's methods for something that is not a thing.
    #[test]
    fn the_class_a_receiver_is_spelled_like() {
        let rows = [
            ("@user", Some("User")),
            ("person", Some("Person")),
            ("first_name", Some("FirstName")),
            // Both sigils: both are stripped, and the rule is Ruby's.
            ("@@count", Some("Count")),
            ("@user_session", Some("UserSession")),
            // Ruby constants begin with an ASCII capital, so nothing else can name a class.
            ("_", None),
            ("@", None),
            ("123", None),
            // A punctuation method name is not the name of a thing.
            ("valid?", None),
            ("save!", None),
            ("[]", None),
            ("+", None),
        ];
        let answers: Vec<(&str, Option<String>)> = rows
            .iter()
            .map(|(receiver, _)| (*receiver, class_named_like(receiver)))
            .collect();
        let expected: Vec<(&str, Option<String>)> = rows
            .iter()
            .map(|(receiver, class)| (*receiver, class.map(str::to_owned)))
            .collect();
        assert_eq!(answers, expected);
    }

    #[test]
    fn a_guessed_constant_is_resolved_outwards_through_the_nesting() {
        // Ruby's lexical lookup, spelled out here rather than asked of rubydex, because there is no
        // reference to resolve: nobody wrote `Story`.
        use rubydex::{indexing::LanguageId, model::graph::Graph, resolution::Resolver};

        use super::super::indexer;

        let mut graph = Graph::new();
        assert!(indexer::index_source(
            &mut graph,
            "file:///app.rb",
            "module Admin
  class Story
  end
end

class Story
end
",
            &LanguageId::Ruby
        ));
        Resolver::new(&mut graph).resolve();

        // Innermost first: inside `Admin::Posts`, `Story` is `Admin::Story`.
        assert_eq!(
            constant_named(&graph, Some("Admin::Posts"), "Story"),
            declared(&graph, "Admin::Story")
        );
        // Then outwards to the top level, where a template's nesting starts.
        assert_eq!(
            constant_named(&graph, Some("Object"), "Story"),
            declared(&graph, "Story")
        );
        // No nesting is the same lookup with nothing to walk, not no lookup.
        assert_eq!(
            constant_named(&graph, None, "Story"),
            declared(&graph, "Story")
        );
        assert_eq!(constant_named(&graph, None, "Ghost"), None);
    }

    #[test]
    fn a_tuple_return_is_read_by_position_and_only_from_the_arms_a_blockless_call_reaches() {
        // Real core signatures again, as in `ruby_s_own_signatures...`: the policy is a claim about
        // a corpus, and RBS written here to exercise it can only agree with itself.
        let mut types = Types::new();
        for file in ["io.rbs", "math.rbs", "method.rbs", "string.rbs"] {
            let path = format!("vendor/rbs/core/{file}");
            types.harvest(
                &path,
                &std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("{path}")),
            );
        }
        let at = |method: &str, index: usize| {
            types
                .tupled(DeclarationId::from(method), index)
                .unwrap_or("(none)")
                .to_owned()
        };

        // **The method this rung is for.** `IO.pipe` is declared `-> [IO, IO]` and
        // `[X] (...) { ([IO, IO]) -> X } -> X`, and reading both arms together would refuse it. A
        // destructure reaches the blockless arm, so the other says nothing here. `returned_element`
        // refuses a call that wrote a block, which is what makes skipping it sound.
        assert_eq!(at("IO::<IO>#pipe()", 0), "IO");
        assert_eq!(at("IO::<IO>#pipe()", 1), "IO");
        // Past the end is `None`, never the last element: `a, b, c = IO.pipe` leaves `c` untyped,
        // as Ruby does.
        assert_eq!(at("IO::<IO>#pipe()", 2), "(none)");

        // Two different classes, so the position is the whole answer.
        assert_eq!(at("Math::<Math>#frexp()", 0), "Float");
        assert_eq!(at("Math::<Math>#frexp()", 1), "Integer");

        // `[String, Integer]?` is a tuple the way `String?` is a class: the module's one inexact
        // entry, for `class_of`'s reason.
        assert_eq!(at("Method#source_location()", 0), "String");
        assert_eq!(at("Method#source_location()", 1), "Integer");

        // `Math.lgamma` is `-> [Float, -1 | 1]`: position one is two literals of one class, which
        // is that class.
        assert_eq!(at("Math::<Math>#lgamma()", 0), "Float");
        assert_eq!(at("Math::<Math>#lgamma()", 1), "Integer");
        // An ordinary return is not a tuple and gets no entry.
        assert_eq!(at("String#upcase()", 0), "(none)");
    }

    /// A decoy declaring every method the two tuple tests read.
    ///
    /// Without it, a name written once in the fixture is answered exactly by the **name** rung, and
    /// every case passes whether the receiver was typed or not.
    const TUPLE_DECOY: &str = "class Ghost\n  \
                               def upcase\n  end\n\n  \
                               def digits\n  end\n\n  \
                               def length\n  end\n\n  \
                               def scan\n  end\n\n  \
                               def bytes\n  end\nend\n";

    #[test]
    fn a_multiple_assignment_takes_the_tuple_position_its_name_was_written_at() {
        // `pair` is declared `() -> [String, Integer]`. A tuple is not one class, so `class_of`
        // refuses it and the call itself has no type; each target must get its position from the
        // tuple table.
        let source = "first, second = \"x\".pair\nfirst.upcase\nsecond.digits\n";
        let (mut harness, uri) = with_types(source);
        let ghost = harness.write("lib/ghost.rb", TUPLE_DECOY);
        harness.watch(&[&ghost]);

        let left = card(&mut harness, &uri, source, "upcase");
        assert!(left.contains("String#upcase"), "{left}");
        assert!(
            !left.contains("possible definitions") && !left.contains("Guessed from name alone"),
            "exactly, not by the name — the decoy declares `upcase` too: {left}"
        );
        // The *other* position, with a different class: what reading an index buys over giving
        // every target the same answer.
        let right = card(&mut harness, &uri, source, "digits");
        assert!(right.contains("Integer#digits"), "{right}");
        assert!(
            !right.contains("possible definitions"),
            "and exactly here too: {right}"
        );
    }

    #[test]
    fn a_multiple_assignment_this_cannot_count_off_is_refused_rather_than_answered() {
        // Three refusals, each of which would otherwise be a *wrong* answer, not a missing one. A
        // different member per case keeps each card's needle unique.
        let source = "a, b, c = \"x\".pair\n\
                      head, *rest = \"x\".pair\n\
                      one, two = \"x\".pair { |p| p }\n\
                      c.length\nhead.scan\none.bytes\n";
        let (mut harness, uri) = with_types(source);
        let ghost = harness.write("lib/ghost.rb", TUPLE_DECOY);
        harness.watch(&[&ghost]);

        for (needle, owned, why) in [
            (
                "length",
                "String#length",
                "a third name against a two-element tuple",
            ),
            (
                "scan",
                "String#scan",
                "a `*rest` fixes no position after itself",
            ),
            (
                "bytes",
                "String#bytes",
                "the call wrote a block, and `pair` hands the block the tuple and the caller \
                 whatever the block returned",
            ),
        ] {
            let written = card(&mut harness, &uri, source, needle);
            assert!(
                !written.contains(owned) || written.contains("Guessed from name alone"),
                "{why}: {written}"
            );
        }
    }

    #[test]
    fn new_written_without_a_constant_is_still_an_instance_of_the_class_it_is_written_in() {
        // `def self.call; new(...).call; end` is the standard Rails service-object entry point, and
        // `cursor::instantiated` cannot see it: it reads the *text*, where a receiver is a constant
        // only when one was written. Three spellings of the same call, plus the one that must stay
        // untyped: a bare `new` in an instance method is someone's private method, never
        // `Class#new`.
        let source = "class Service\n  \
                      def self.run\n    new.alpha\n  end\n\n  \
                      def self.run_forwarded(...)\n    new(...).beta\n  end\n\n  \
                      def self.run_on_self\n    self.new.gamma\n  end\n\n  \
                      def instance_side\n    new.delta\n  end\n\n  \
                      def alpha\n  end\n\n  \
                      def beta\n  end\n\n  \
                      def gamma\n  end\n\n  \
                      def delta\n  end\nend\n";
        let (mut harness, uri) = with_types(source);
        let ghost = harness.write(
            "lib/ghost.rb",
            "class Ghost\n  \
             def alpha\n  end\n\n  \
             def beta\n  end\n\n  \
             def gamma\n  end\n\n  \
             def delta\n  end\nend\n",
        );
        harness.watch(&[&ghost]);

        for (needle, spelling) in [
            ("alpha", "new"),
            ("beta", "new(...)"),
            ("gamma", "self.new"),
        ] {
            let written = card(&mut harness, &uri, source, needle);
            assert!(
                written.contains(&format!("Service#{needle}")),
                "`{spelling}` inside `def self.` is an instance of `Service`: {written}"
            );
            assert!(
                !written.contains("possible definitions")
                    && !written.contains("Guessed from name alone"),
                "exactly, not by the name — the decoy declares `{needle}` too: {written}"
            );
        }

        // **The control, and why the check is not just `method == \"new\"`.** `self` here is the
        // instance, not the class object, so `new` is someone's private method and `instance_of`
        // answers nothing.
        let instance = card(&mut harness, &uri, source, "delta");
        assert!(
            instance.contains("possible definitions")
                || instance.contains("Guessed from name alone"),
            "a bare `new` in an instance method is not `Class#new`: {instance}"
        );
    }

    #[test]
    fn a_class_that_declares_its_own_new_keeps_what_the_signature_says() {
        // The rung runs **after** the signature, only where it answered nothing, so a declared
        // `self.new` is not displaced by Ruby's default rule. The fixture declares `Minted.new` as
        // `() -> String`, a lie about Ruby on purpose: if the rule ran first, the card would say
        // `Minted` and the signature would never be read.
        let source = "class Minted\n  \
                      def self.make\n    new.upcase\n  end\nend\n";
        let (mut harness, uri) = with_types(source);
        let written = card(&mut harness, &uri, source, "upcase");
        assert!(
            written.contains("String#upcase"),
            "the signature wins over the `new` rule: {written}"
        );
    }

    #[test]
    fn ruby_s_own_signatures_are_what_the_policy_was_written_against() {
        // Real core signatures, not hand-written RBS: the policy is a claim about a corpus, and a
        // document written to exercise it can only agree with itself. These files are in the
        // repository, so this needs no Ruby and no network.
        let mut types = Types::new();
        for file in ["string.rbs", "hash.rbs", "array.rbs", "float.rbs"] {
            let path = format!("vendor/rbs/core/{file}");
            types.harvest(
                &path,
                &std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("{path}")),
            );
        }

        let called = |method: &str, arity: u32, block: bool| {
            types
                .returns(DeclarationId::from(method), Arity::Exactly(arity), block)
                .map_or_else(|| "(none)".to_owned(), spelled)
        };
        let answers = |method: &str| called(method, 0, false);
        let with_block = |method: &str| called(method, 0, true);

        // Taken. `upcase` and `sort` are several overloads naming one class. `upcase!` is `self?`:
        // the class stays `self` (it depends on the receiver), and `nil` rides beside it as the
        // mark, since `"x".upcase!` returns `nil` when nothing changed.
        assert_eq!(answers("String#upcase()"), "String");
        assert_eq!(answers("String#upcase!()"), "self?");

        // **The head brings what it holds.** For generics like `keys` and `sort`, the head answers
        // a `.`, and the *argument* answers the next call's block. It is stored as a position,
        // because `K` and `Elem` mean the receiver's own arguments and nothing until there is a
        // receiver.
        assert_eq!(answers("Hash#keys()"), "Array[Hash[0]]");
        assert_eq!(answers("Array#sort()"), "Array[Array[0]]");

        // `bytes` declares `() -> Array[Integer]` and `() { (Integer) -> void } -> self`. Not a
        // union: the block picks the arm, both answers are exact, and this one names its element,
        // so `"x".bytes.first` is an `Integer`.
        assert_eq!(answers("String#bytes()"), "Array[Integer]");
        assert_eq!(with_block("String#bytes()"), "self");
        // `round` declares `(?half: ...) -> Integer` beside
        // `(int, ?half: ...) -> (Integer | Float)`. A written digit count picks the arm, and each
        // side agrees with itself; the one-digit side is the union it declares.
        assert_eq!(answers("Float#round()"), "Integer");
        assert_eq!(called("Float#round()", 1, false), "Integer | Float");

        // Dropped. `gsub` declares three blockless arms of one arity naming two classes: a union,
        // with no block to tell them apart.
        assert_eq!(answers("String#gsub()"), "(none)");
        // `first` declares `() -> E` and `(int count) -> Array[E]`, which arity tells apart:
        // `[1, 2].first(3)` is exactly an `Array`. The zero-argument side is the receiver's first
        // type argument, held as a **question** and answered at the call, only by a receiver that
        // carries one.
        assert_eq!(answers("Array#first()"), "Array[0]");
        assert_eq!(called("Array#first()", 1, false), "Array[Array[0]]");
        // An alias is the method it renames, in all three tables: `alias map collect`.
        //
        // - **`map`'s own `[U]` is the block's answer**, shown as `{ }`. A method's type variable
        //   is bound by the call, so the receiver says nothing and the block says everything:
        //   `[1, 2].map { |n| n.to_s }` is an `Array` of whatever `n.to_s` is.
        // - **The blockless arm keeps the `_`.** `() -> Enumerator[Elem, Array[U]]` has no block
        //   for `U` to come from, so it stays refused.
        assert_eq!(answers("Array#collect()"), "Enumerator[Array[0], Array[_]]");
        assert_eq!(with_block("Array#collect()"), "Array[{ }]");
        assert_eq!(answers("Array#map()"), "Enumerator[Array[0], Array[_]]");
        assert_eq!(with_block("Array#map()"), "Array[{ }]");
        assert_eq!(answers("Array#length()"), "Integer");
        assert_eq!(answers("Array#size()"), "Integer");

        // Pinned, not bounded, as `rbs-signatures.md` says: bumping `vendor/rbs` changes ya-lsp's
        // answers, and so does a policy change. Both should fail visibly, not drift.
        // Three more since a union return is read, and ten since a tuple return is an
        // `Array`: `divmod`, `partition`, `minmax` and their kin. Two more whose own `[X]`
        // one argument binds, answered only at a call that passes a typed one.
        assert_eq!(types.len(), 357, "methods typed across four core files");
    }

    #[test]
    fn a_default_gem_reopening_a_core_class_no_longer_replaces_what_core_declared() {
        // The real pair, from the repository: `vendor/rbs/core/float.rbs` declares
        // `Float#+: (Complex) -> Complex | (Numeric) -> Float`, and bigdecimal's stdlib signatures
        // reopen `Float` to declare `def +: (BigDecimal) -> BigDecimal`. bigdecimal is a **default
        // gem**, loaded almost everywhere, so if the second read won, `1.5 + 1` would answer
        // `BigDecimal` in every project.
        let harvest = |files: &[&str]| {
            let mut types = Types::new();
            for file in files {
                let path = format!("vendor/rbs/{file}");
                types.harvest(
                    &path,
                    &std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("{path}")),
                );
            }
            types
        };
        let core = "core/float.rbs";
        let gem = "stdlib/bigdecimal/0/big_decimal.rbs";
        let plus = DeclarationId::from("Float#+()");
        let of_one = |types: &Types, method: &str| {
            types
                .returns(DeclarationId::from(method), Arity::Exactly(1), false)
                .map_or_else(|| "(none)".to_owned(), spelled)
        };

        // Core alone declines: two one-argument arms naming `Complex` and `Float` are a union,
        // which the policy drops. That is the honest answer.
        assert_eq!(of_one(&harvest(&[core]), "Float#+()"), "(none)");
        // The gem alone answers, because alone it is the only declaration.
        assert_eq!(of_one(&harvest(&[gem]), "Float#+()"), "BigDecimal");

        // **Together, in either order, they decline.** Order-independence is the point: the answer
        // must not depend on which document the walk read second.
        for order in [[core, gem], [gem, core]] {
            let types = harvest(&order);
            assert_eq!(of_one(&types, "Float#+()"), "(none)", "{order:?}");
            // **No row at all**, not a row that answers nothing: every partition of the union
            // disagrees, which is `Overloads`' emptiness rule, applied after the merge.
            assert!(!types.returns.contains_key(&plus), "{order:?}");
            // What only one document declares is untouched by the merge: the agreement widened; the
            // reopening was not dropped.
            assert_eq!(of_one(&types, "Float#to_d()"), "BigDecimal", "{order:?}");
        }
    }

    #[test]
    fn a_bare_literal_return_is_a_class_and_it_is_most_of_what_the_boolean_pair_declares() {
        // The companion fixture, over the three files where a *literal* return is the rule.
        // `TrueClass` and `FalseClass` declare six names each, and five of the six are written as
        // literals, so `bool` chaining depends on `class_of` reading a literal on its own.
        let mut types = Types::new();
        for file in ["true_class.rbs", "false_class.rbs", "nil_class.rbs"] {
            let path = format!("vendor/rbs/core/{file}");
            types.harvest(
                &path,
                &std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("{path}")),
            );
        }
        let called = |method: &str, arity: u32| {
            types
                .returns(DeclarationId::from(method), Arity::Exactly(arity), false)
                .map_or_else(|| "(none)".to_owned(), spelled)
        };
        let answers = |method: &str| called(method, 0);

        // **The half that was written, not the pair.** `TrueClass#!` only ever returns `false`, so
        // `bool` would be wider, not truer. This arm is not a fold. The table holds the carrier;
        // `render` draws it in lower case.
        assert_eq!(answers("TrueClass#!()"), "FalseClass");
        assert_eq!(answers("FalseClass#!()"), "TrueClass");
        assert_eq!(answers("NilClass#!()"), "TrueClass");

        // A string literal is a `String` and an integer literal an `Integer`: this arm is about
        // literals, not booleans.
        assert_eq!(answers("TrueClass#to_s()"), "String");
        assert_eq!(answers("FalseClass#to_s()"), "String");
        assert_eq!(answers("NilClass#to_s()"), "String");
        assert_eq!(answers("NilClass#inspect()"), "String");
        assert_eq!(answers("NilClass#to_i()"), "Integer");

        // `nil?` reaches past the pair: `Kernel#nil?` is also `() -> false`, and this is how
        // `.nil?` resolves on almost any receiver.
        assert_eq!(answers("NilClass#nil?()"), "TrueClass");

        // An empty tuple is an `Array` like any tuple. An empty record is still refused:
        // RBS reads `{}` as a record type, which names no class.
        assert_eq!(answers("NilClass#to_a()"), "Array");
        assert_eq!(answers("NilClass#to_h()"), "(none)");

        // The one-argument members, through the arity partition.
        assert_eq!(called("TrueClass#|()", 1), "TrueClass");
        assert_eq!(called("FalseClass#&()", 1), "FalseClass");
        assert_eq!(called("NilClass#&()", 1), "FalseClass");

        // **The agreement rule still outranks the literal arm, which keeps it safe.** `TrueClass#&`
        // is `(false | nil) -> false | (untyped obj) -> bool`: two one-argument arms naming
        // different things, so the partition disagrees and answers nothing. Reading the literal
        // made the first arm *readable*; it did not make it win.
        assert_eq!(called("TrueClass#&()", 1), "(none)");
        assert_eq!(called("TrueClass#===()", 1), "(none)");
        assert_eq!(called("FalseClass#|()", 1), "(none)");

        // Pinned for `ruby_s_own_signatures_...`'s reason: it moves when `vendor/rbs` is bumped or
        // the policy changes, and both should fail visibly. Without the literal arm, only
        // `NilClass`'s `rationalize`, `to_c`, `to_f` and `to_r` would count.
        // **A partition only a keyword hash reaches still keeps the row**. The hash
        // is not `true`, so `true === (a: 1)` reaches only the `(untyped) -> bool` arm.
        let keyed = |method: &str| {
            types
                .returns(DeclarationId::from(method), Arity::Keyed(0), false)
                .map_or_else(|| "(none)".to_owned(), spelled)
        };
        assert_eq!(keyed("TrueClass#===()"), "bool");
        assert_eq!(keyed("FalseClass#===()"), "bool");

        // Six more since unions are read: `false | nil` is now a readable parameter, so a
        // keyword hash cannot be handed to it, and `true ^ (a: 1)` reaches the `bool` arm alone.
        // One more since a tuple is an `Array`: `NilClass#to_a`.
        assert_eq!(
            types.len(),
            27,
            "methods typed across the boolean pair and nil"
        );
    }

    #[test]
    fn a_chain_completes_against_what_the_signature_says_it_returns() {
        // A chain through signatures, end to end over the wire. Without the return-type table,
        // every line here would answer `(unrecognised)`, the name-based list.
        let (mut harness, uri) = with_types("");
        assert_eq!(class_at(&mut harness, &uri, "\"hi\".upcase.~"), "String");
        // A chain of two: what makes it a chain and not one step.
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".upcase.upcase.~"),
            "String"
        );
        // A link that changes class, then a link on the new class.
        assert_eq!(class_at(&mut harness, &uri, "\"hi\".length.~"), "Integer");
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".length.succ.~"),
            "Integer"
        );
        // A generic: the head is the answer, and nothing here asks the element type.
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".scan(\"a\").~"),
            "Array"
        );
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".scan(\"a\").join.~"),
            "String"
        );
    }

    #[test]
    fn a_mark_does_not_stop_a_chain_and_a_union_does() {
        // The two promises the folds make to a *caller*, as opposed to a reader.
        //
        // - **`guarded` answers `String?`.** Only a margin reads the mark, so a `.` after it offers
        //   `String`'s members as for a plain `String`. The one deliberate inexactness: a call that
        //   returned `nil` has no `upcase`, and the label makes that visible.
        // - **`split` answers `String | Integer`.** No one class can answer a `.`, so completion
        //   falls to the name-based list, as for an untyped receiver. That is what [`Typed::one`]
        //   returning `None` buys. A *call* on the union is the one step that goes on, class by
        //   class (`a_call_on_a_union_is_made_on_each_class_that_has_the_member`).
        //
        // The class is in **another file**, because `Harness::complete` replaces the buffer it
        // completes in, and the body rung reads bodies from buffers.
        let (mut harness, uri) = with_types("");
        harness.write(
            "lib/ledger.rb",
            "\
class Ledger
  def guarded
    return if stamped?
    \"x\"
  end

  def split
    if stamped?
      \"x\"
    else
      1
    end
  end
end
",
        );
        harness.index();

        assert_eq!(
            class_at(&mut harness, &uri, "Ledger.new.guarded.~"),
            "String"
        );
        assert_eq!(
            class_at(&mut harness, &uri, "Ledger.new.split.~"),
            "(everything, which is the name-based list)"
        );
    }
    #[test]
    fn self_in_a_signature_is_the_receiver_and_not_the_class_that_declared_it() {
        // `Kernel#tap` returns `self`. Resolved where the signature is *written*, that would be
        // `Kernel`, with none of the receiver's methods. The lookup goes through rubydex's ancestor
        // walk and resolves `self` against the receiver.
        //
        // Written with a block because rbs declares `tap` with a required one. That pins a second
        // rule: an arm with a required block is not what a blockless call reaches.
        let (mut harness, uri) = with_types("");
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".tap { |s| s }.~"),
            "String"
        );
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".length.tap { |n| n }.~"),
            "Integer"
        );
        let blockless = class_at(&mut harness, &uri, "\"hi\".tap.~");
        assert!(
            blockless.starts_with("(everything"),
            "a required block is not optional: {blockless}"
        );
    }

    #[test]
    fn a_local_assigned_a_chain_is_typed_by_it() {
        // A local's write goes beyond literals and `.new`.
        let (mut harness, uri) = with_types("");
        assert_eq!(
            class_at(&mut harness, &uri, "shouted = \"hi\".upcase\nshouted.~\n"),
            "String"
        );
        assert_eq!(
            class_at(&mut harness, &uri, "n = \"hi\".length\nn.succ.~\n"),
            "Integer"
        );
    }

    #[test]
    fn a_block_at_the_call_site_chooses_which_overload_answered() {
        // The easy one to get wrong. `String#bytes` declares `() -> Array[Integer]` and
        // `() { (Integer) -> void } -> self`. Not a union: whether a block was written picks the
        // arm, and the cursor has already read that. Both answers are exact, and different.
        let (mut harness, uri) = with_types("");
        assert_eq!(class_at(&mut harness, &uri, "\"hi\".bytes.~"), "Array");
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".bytes { |b| b }.~"),
            "String"
        );
        // `&:to_s` and a forwarded `&blk` pass a block too, so they reach the same arm.
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".bytes(&:to_s).~"),
            "String"
        );
    }

    #[test]
    fn how_many_arguments_the_call_wrote_chooses_which_overload_answered() {
        // The arity version of the block rule. `Float#round` declares `(?half: ...) -> Integer`
        // beside `(Integer, ?half: ...) -> (Integer | Float)`. As one partition both are dropped;
        // by arity the zero-argument side agrees with itself, and `3.7.round.` is an `Integer`. A
        // fact, not a guess.
        let (mut harness, uri) = with_types("");
        assert_eq!(class_at(&mut harness, &uri, "3.7.round.~"), "Integer");
        // Keywords are not positional arguments on either side.
        assert_eq!(
            class_at(&mut harness, &uri, "3.7.round(half: :up).~"),
            "Integer"
        );
        // The arm the digit count reaches is a union, which arity does not rescue: the list is
        // both classes', `Integer`'s `succ` beside `Float`'s `round`.
        let digits = harness.declarations_at(&uri, "3.7.round(1).~");
        assert!(
            ["succ", "round"]
                .iter()
                .all(|name| digits.iter().any(|row| row == name))
                && !digits.iter().any(|row| row == "upcase"),
            "{digits:?}"
        );
        // `Array#first`: arity tells `(Integer count) -> Array[E]` from `() -> E`, the receiver's
        // own type argument. A literal carries that argument, so both halves answer, with different
        // classes.
        assert_eq!(class_at(&mut harness, &uri, "[1, 2].first(3).~"), "Array");
        assert_eq!(class_at(&mut harness, &uri, "[1, 2].first.~"), "Integer");
        // A receiver with no known argument: an `Array` of something unknown. `E` stays a question.
        let unheld = class_at(&mut harness, &uri, "[foo, bar].first.~");
        assert!(unheld.starts_with("(everything"), "{unheld}");
    }

    #[test]
    fn a_call_the_signature_cannot_accept_falls_back_rather_than_to_the_nearest_arm() {
        // What keeps this a reading of the signature, and the one place the split takes an answer
        // away: `scan` has one arm that requires a pattern, so `"hi".scan.` reaches no arm rather
        // than answering `Array`. `types.md` has the argument.
        let (mut harness, uri) = with_types("");
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".scan(\"a\").~"),
            "Array"
        );
        let bare = class_at(&mut harness, &uri, "\"hi\".scan.~");
        assert!(bare.starts_with("(everything"), "{bare}");
        let too_many = class_at(&mut harness, &uri, "\"hi\".scan(\"a\", 1).~");
        assert!(too_many.starts_with("(everything"), "{too_many}");
        // A call whose arguments cannot be counted gets what every arm agrees on. That is what
        // makes this a partition.
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".scan(*args).~"),
            "Array"
        );
    }

    #[test]
    fn a_chain_through_something_the_table_dropped_answers_nothing_exact() {
        // The `Unknown` path at this tier, the half a green suite hides: a fallback that silently
        // absorbs a bug. Each of these must reach the name-based list, not a class.
        let (mut harness, uri) = with_types("");
        // `String#sub` declares `(String pattern) -> String` beside `(Integer index) -> Integer`,
        // and the call passes a `String`, so the arms are told apart by the argument rather than
        // dropped. [`pick_by_argument`] is the only reason this line differs from the union below.
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".sub(\"a\").~"),
            "String"
        );
        // The same two arms with an untypable argument: neither can be ruled out, so the
        // disagreement stands and the chain stops.
        let union = class_at(&mut harness, &uri, "\"hi\".sub(whatever).~");
        assert!(union.starts_with("(everything"), "{union}");
        // A method no signature declares.
        let absent = class_at(&mut harness, &uri, "\"hi\".nonesuch.~");
        assert!(absent.starts_with("(everything"), "{absent}");
        // A receiver that was never anything: one `Unknown` ends the chain.
        let nothing = class_at(&mut harness, &uri, "thing.upcase.~");
        assert!(nothing.starts_with("(everything"), "{nothing}");
    }

    #[test]
    fn an_instance_variable_completes_against_what_its_class_assigned_it() {
        // An instance variable end to end, shaped like a Rails controller: assigned in one method,
        // read in the others. The most common receiver in an application, typed with no annotation.
        let (mut harness, uri) = with_types("");
        let controller = "\
class Report
  def initialize
    @title = \"quarterly\"
  end

  def render
    @title.~
  end
end
";
        assert_eq!(class_at(&mut harness, &uri, controller), "String");

        // Through a chain, with the three derived rungs composing: the ivar typed by an assignment,
        // the assignment by a signature, and the cursor one link past both.
        let chained = "\
class Report
  def initialize
    @size = \"quarterly\".length
  end

  def render
    @size.~
  end
end
";
        assert_eq!(class_at(&mut harness, &uri, chained), "Integer");
    }

    #[test]
    fn an_instance_variable_from_another_self_does_not_leak_into_instance_methods() {
        // The `Unknown` path for an instance variable, where a mistake would be a *wrong* answer:
        // `@seed` in `def self.build` belongs to the class object, and joining it to the instance's
        // `@seed` would offer `String`'s methods for something that was never a string.
        let (mut harness, uri) = with_types("");
        let split = "\
class Report
  def self.build
    @seed = \"x\"
  end

  def render
    @seed.~
  end
end
";
        let answered = class_at(&mut harness, &uri, split);
        assert!(answered.starts_with("(everything"), "{answered}");
    }

    /// One file holding every tier an answer can come from, so the cards can be read together.
    ///
    /// - **The trailing comments are the needles.** A hover fixture points at the *start* of what
    ///   it searches for, so each row needs a method name spelled once.
    /// - **`shout` and `tally` have stub bodies** naming something undeclared. An empty body
    ///   answers `NilClass`, and a return type on every card would bury the tier line each row is
    ///   there to show.
    const TIERS: &str = "\
class Report
  def initialize
    @title = \"quarterly\"
    @size = \"quarterly\".length
  end

  def a
    \"hi\".upcase # resolved
  end

  def b
    \"hi\".upcase.length # derived
  end

  def c
    \"hi\".upcase.length.digits # chained
  end

  def d
    @title.upcase # assigned
  end

  def e
    @size.succ # both
  end

  def f
    thing.upcase # guessed
  end

  def g
    person.shout # named
  end

  def h
    GREETING.upcase # constant
  end

  def i
    HOLDER.shout # held
  end

  def j
    HOLDER.tally # missed
  end

  def k
    Person.tally # class object
  end

  def l
    @person.tally # guessed receiver
  end
end

class Person
  def shout
    unknowable
  end
end

module Counting
  def tally
    unknowable
  end
end

class Ledger
  include Counting

  [1].each do
    tally # closure
  end
end

HOLDER = Person.new
";

    #[test]
    fn a_call_with_no_receiver_at_all_has_no_type_to_derive() {
        // `resolve_typed` runs on every call the graph could not resolve, and most have no written
        // receiver: a bare `render` is a call on an implicit `self`, a question about the enclosing
        // class. The new rung has nothing to do, so the answer is the name-based one.
        let mut harness = Harness::new();
        harness.write("app/view.rb", "class View\n  def render\n  end\nend\n");
        let source = "render\n";
        let caller = harness.write("app/main.rb", source);
        harness.index();

        let markdown = card(&mut harness, &caller, source, "render");
        assert!(markdown.contains("View#render"), "{markdown}");
        assert!(
            markdown.contains("Guessed from name alone"),
            "and it is still a guess: {markdown}"
        );
    }

    #[test]
    fn a_self_captured_above_a_block_that_rebinds_it_keeps_the_class_that_captured_it() {
        // **rubydex records a `Class.new(base) do … end` body as an anonymous class**, so `self`
        // inside it is that class, not the instance the enclosing method ran on. Ruby agrees about
        // the block's own `self`, but a local closes over the `self` of the line that *wrote* it.
        //
        // `Receiver::SelfObject` carries that line's offset for this reason. Read at the cursor's
        // scope instead, `held` would be the anonymous class: none of the captured instance's
        // members and no name a card can print, while `completion` listed the anonymous class's
        // members at the same byte.
        let mut harness = Harness::new();
        let source = "class Thing\n  def go\n    held = self\n    Class.new(Object) do\n      \
                      held.frob\n    end\n  end\n\n  def frob\n  end\nend\n";
        let uri = harness.write("app/thing.rb", source);
        harness.index();

        // `find` takes the first `frob`, the one inside the block.
        let markdown = card(&mut harness, &uri, source, "frob");
        assert!(markdown.contains("Thing#frob"), "{markdown}");
        assert!(
            !markdown.contains("Guessed from name alone"),
            "and exactly, not by the name: {markdown}"
        );

        // The other half of the same fact. `method_receiver` is shared so the two surfaces cannot
        // disagree about `held.`.
        let answer = harness.complete(
            &uri,
            "class Thing\n  def go\n    held = self\n    Class.new(Object) do\n      \
             held.~\n    end\n  end\n\n  def frob\n  end\nend\n",
        );
        let (labels, precise) = offered(&answer);
        assert!(
            precise,
            "the receiver is typed, not name-matched: {labels:?}"
        );
        assert!(labels.contains(&"frob".to_owned()), "{labels:?}");
    }

    #[test]
    fn an_index_call_on_a_constant_that_holds_an_object_is_typed() {
        // `ENV["HOME"]`: the `[]` call's reference begins where `ENV` ends, and the constant was
        // looked up there, so the index was untyped while `ENV.fetch("HOME")` was typed.
        let (mut harness, uri) = with_rbs(
            "",
            "class Holder\n  def []: (String name) -> String?\nend\nHELD: Holder\n",
        );
        assert_eq!(class_at(&mut harness, &uri, "HELD[\"x\"].~"), "String");
        assert_eq!(
            class_at(&mut harness, &uri, "home = HELD[\"x\"]\nhome.~"),
            "String"
        );
        assert_eq!(
            class_at(&mut harness, &uri, "KEY = HELD[\"x\"].upcase\nKEY.~"),
            "String"
        );
    }

    #[test]
    fn a_constant_alias_is_the_namespace_it_names() {
        // `YAML = Psych`: rubydex files an alias, which is no namespace, so `YAML.` listed nothing
        // and `YAML.load_file(path)` ended the chain. The same alias `Foo.new` already follows.
        let (mut harness, uri) = with_types("");
        let defined =
            "module Real\n  def self.load_file(path) = \"x\"\nend\nNick = Real\nOther = Nick\n";
        assert!(
            harness
                .declarations_at(&uri, &format!("{defined}Nick.~"))
                .iter()
                .any(|name| name == "load_file")
        );
        assert_eq!(
            class_at(&mut harness, &uri, &format!("{defined}Nick.load_file(1).~")),
            "String"
        );
        // An alias of an alias.
        assert_eq!(
            class_at(
                &mut harness,
                &uri,
                &format!("{defined}Other.load_file(1).~")
            ),
            "String"
        );
    }

    #[test]
    fn a_constant_that_holds_an_object_is_typed_from_the_signature_that_declares_it() {
        // The constant is not the class. `ENV` holds an *instance*, so the graph has a constant
        // with no singleton, which rubydex has promoted to a namespace it invented. Only the
        // signature says what the object is, as plainly as `-> String`.
        let (mut harness, uri) = with_types(
            "\
class Reader
  def a
    GREETING.upcase
  end

  def b
    MYSTERY.upcase
  end
end
",
        );

        let mut card = |needle: &str| {
            harness.hover_at(&uri, HOLDERS, needle)["contents"]["value"]
                .as_str()
                .unwrap_or("null")
                .to_owned()
        };

        let answered = card("upcase\n  end\n\n  def b");
        assert!(answered.contains("String#upcase"), "{answered}");
        assert!(!answered.contains(GUESS_FOOTNOTE), "{answered}");

        // **A class the signature names and nothing declares adds nothing.** An id built from an
        // undeclared name answers nothing, so the rung falls through; here to the name-based list,
        // which has the workspace's only `upcase`.
        let ghostly = card("upcase\n  end\nend");
        assert!(ghostly.contains("String#upcase"), "{ghostly}");
        assert!(
            ghostly.contains(GUESS_FOOTNOTE),
            "and it is a guess rather than a type: {ghostly}"
        );
    }

    /// The two cursors of the test above, as the text that finds them.
    const HOLDERS: &str = "\
class Reader
  def a
    GREETING.upcase
  end

  def b
    MYSTERY.upcase
  end
end
";

    const GUESS_FOOTNOTE: &str = "Guessed from name alone";

    /// The other half of "a constant is not the class it holds": the Ruby that assigns it.
    ///
    /// - **A signature typed `ENV` because Ruby ships one.** No one will ship a signature for an
    ///   application's own config object; the line that builds it says the same thing, in a file
    ///   far from the cursor, which is why the derivation names it.
    /// - **The assignment is written inside its namespace and names its class unqualified**, so the
    ///   nesting matters: `Cabinet` means `Vault::Cabinet` there and nothing where the cursor is.
    #[test]
    fn a_constant_is_typed_from_the_ruby_that_assigns_it() {
        let (mut harness, _uri) = with_types("class Unrelated\nend\n");
        harness.write(
            "lib/vault.rb",
            "\
module Vault
  class Cabinet
    def combination; end
  end
end
",
        );
        harness.write(
            "config/initializers/vault.rb",
            "module Vault\n  CABINET = Cabinet.new\nend\n",
        );
        let main = harness.write("lib/main.rb", ASSIGNED);
        harness.index();

        let mut card = |needle: &str| {
            harness.hover_at(&main, ASSIGNED, needle)["contents"]["value"]
                .as_str()
                .unwrap_or("null")
                .to_owned()
        };

        let answered = card("combination # held");
        assert!(
            answered.contains("Vault::Cabinet#combination"),
            "{answered}"
        );
        assert!(!answered.contains(GUESS_FOOTNOTE), "{answered}");

        // **An assignment naming a class this workspace never declares falls through.**
        // `Missing.new` is a normal shape, but its lookup is a hash of an undeclared name, so the
        // rung answers nothing and the arms below answer; here the name-based list, with the
        // workspace's only `combination`.
        let ghostly = card("combination # ghost");
        assert!(ghostly.contains("Vault::Cabinet#combination"), "{ghostly}");
        assert!(
            ghostly.contains(GUESS_FOOTNOTE),
            "and it is a guess rather than a type: {ghostly}"
        );
    }

    const ASSIGNED: &str = "\
GHOST = Missing.new

def go
  Vault::CABINET.combination # held
  GHOST.combination # ghost
end
";

    /// The rung against a buffer the graph has not caught up with, both ways round.
    ///
    /// The assignment is read from the *buffer* (an initializer being edited types the constant
    /// before it is saved), but the span that finds it came from the graph, so the two agree only
    /// until something is typed. An edit past the name translates and the answer stands. An edit
    /// **over** the name cannot be translated honestly, so the rung declines rather than read
    /// whatever constant is at that offset now.
    #[test]
    fn a_constant_whose_declaration_is_being_edited_declines_rather_than_reads_the_wrong_one() {
        let mut harness = Harness::new();
        harness.write(
            "lib/vault.rb",
            "module Vault\n  class Store\n    def unlock\n    end\n  end\nend\n",
        );
        let wiring = harness.write("lib/wiring.rb", WIRING);
        let main = harness.write("lib/main.rb", "HOLDER.unlock\n");
        harness.index();
        harness.open(&wiring, WIRING);

        fn card(harness: &mut Harness, main: &DocUri) -> String {
            harness.hover_at(main, "HOLDER.unlock\n", "unlock")["contents"]["value"]
                .as_str()
                .unwrap_or("null")
                .to_owned()
        }
        let settled = card(&mut harness, &main);
        assert_eq!(settled, "```ruby\nVault::Store#unlock\n```");

        // **A line typed *below* the assignment leaves it where it was.** The name's span is inside
        // text both copies still agree on, so it translates and the answer is unchanged.
        harness.edit_without_indexing(
            &wiring,
            vec![TextChange {
                range: None,
                text: format!("{WIRING}OTHER = 1\n"),
            }],
        );
        let appended = card(&mut harness, &main);
        assert_eq!(
            appended, settled,
            "an edit the span does not overlap costs nothing"
        );

        // **Renaming the constant takes the span with it.** No offset in the new text means what
        // the graph's does, so this rung answers nothing and the name rung answers, as every rung
        // does when unsure.
        harness.edit_without_indexing(
            &wiring,
            vec![TextChange {
                range: None,
                text: "HOLD = Vault::Store.new\n".to_owned(),
            }],
        );
        let mid_rename = card(&mut harness, &main);
        assert!(
            mid_rename.contains(GUESS_FOOTNOTE),
            "the rung answered from a span the graph no longer holds, not the name rung: \
             {mid_rename}"
        );
    }

    const WIRING: &str = "HOLDER = Vault::Store.new\n";

    /// A constant a **gem** assigns: read from the gem's file, where `where_written` cannot make the
    /// path relative.
    #[test]
    fn a_constant_a_gem_assigns_is_typed_from_the_gem_s_assignment() {
        let (dir, _elsewhere, env) = project_with_gem(
            "\
module Shouty
  class Megaphone
    def blare
    end
  end

  LOUD = Megaphone.new
end
",
        );
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let source = "Shouty::LOUD.blare\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();

        let card = harness.hover_at(&uri, source, "blare")["contents"]["value"]
            .as_str()
            .unwrap_or("null")
            .to_owned();
        assert!(card.contains("Shouty::Megaphone#blare"), "{card}");
        assert!(!card.contains(GUESS_FOOTNOTE), "{card}");
    }

    /// Two constants deep, five constants deep, then two constants in a circle.
    ///
    /// - **Why the hop limit is not one:** `THING = FACTORY.build` over `FACTORY = Builder.new` is
    ///   ordinary wiring, and the card shows both rungs.
    /// - **Why it is not three either:** a library builds constants out of constants several deep
    ///   (addressable's character classes are four), and the limit is a guard, not a depth.
    /// - **Why there is a limit:** this rung can come back to the constant it started from. **The
    ///   assertion is that this test returns.** A stack overflow is not a panic the request
    ///   bulkhead can contain; it kills the process.
    #[test]
    fn a_chain_of_constant_assignments_is_followed_and_a_circle_of_them_stops() {
        let (mut harness, _uri) = with_types("class Unrelated\nend\n");
        harness.write(
            "sig/wiring.rbs",
            "class Builder\n  def build: () -> Vault::Store\n  def again: () -> Builder\nend\n",
        );
        harness.write(
            "lib/vault.rb",
            "\
class Builder
  def build; end
  def again; end
end

module Vault
  class Store
    def unlock; end
  end
end
",
        );
        harness.write(
            "lib/wiring.rb",
            "\
FACTORY = Builder.new
THING = FACTORY.build

ONE = Builder.new
TWO = ONE.again
THREE = TWO.again
FOUR = THREE.again
FIVE = FOUR.build

LEFT = RIGHT.unlock
RIGHT = LEFT.unlock
",
        );
        let main = harness.write("lib/main.rb", CHAINED);
        harness.index();
        harness.index_gems();

        let mut card = |needle: &str| {
            harness.hover_at(&main, CHAINED, needle)["contents"]["value"]
                .as_str()
                .unwrap_or("null")
                .to_owned()
        };

        let chained = card("unlock # chain");
        assert!(chained.contains("Vault::Store#unlock"), "{chained}");
        assert!(!chained.contains(GUESS_FOOTNOTE), "{chained}");

        // Five constants deep is still an answer: the limit guards a circle, not a depth.
        let deep = card("unlock # deep");
        assert!(deep.contains("Vault::Store#unlock"), "{deep}");
        assert!(!deep.contains(GUESS_FOOTNOTE), "{deep}");

        // A circle answers nothing, leaving the name-based list, as every rung does when it cannot
        // be exact.
        let circular = card("unlock # circle");
        assert!(circular.contains("Vault::Store#unlock"), "{circular}");
        assert!(
            circular.contains(GUESS_FOOTNOTE),
            "and it is the guess and not a type read off a constant that has none: {circular}"
        );
    }

    const CHAINED: &str = "\
def go
  THING.unlock # chain
  FIVE.unlock # deep
  LEFT.unlock # circle
end
";

    #[test]
    fn the_two_tiers_a_reader_sees_drawn_side_by_side() {
        // Every way an answer is reached, `GALLERY`-style, and what the card makes of each: the
        // answer, and one line where it rests on a name alone (decided 2026-09-29). A signature, an
        // assignment, a constant's signature or the Ruby assigning it, and a block scope the file
        // does not state all read as the code naming the type; the three name matches and the
        // guessed receiver say they are guesses, and nothing else.
        //
        // The property under test: a reader can tell a guess from everything else without leaving
        // the card, and is told nothing about how ya-lsp got there. `answers.GUESSED` in the audit
        // matches the one line.
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
        let uri = harness.write("app/report.rb", TIERS);
        harness.index();
        harness.index_gems();

        let drawn: String = [
            "upcase # resolved",
            "length # derived",
            "digits # chained",
            "upcase # assigned",
            "succ # both",
            "upcase # guessed",
            "shout # named",
            "upcase # constant",
            "shout # held",
            "tally # missed",
            "tally # class object",
            "tally # guessed receiver",
            "tally # closure",
        ]
        .into_iter()
        .map(|needle| {
            let found = harness.hover_at(&uri, TIERS, needle);
            let card = found["contents"]["value"].as_str().unwrap_or("null");
            let body: String = card
                .lines()
                .map(|line| {
                    if line.is_empty() {
                        "\n".to_owned()
                    } else {
                        format!("  {line}\n")
                    }
                })
                .collect();
            format!("{needle}\n{body}")
        })
        .collect();

        assert_eq!(
            drawn,
            "\
upcase # resolved
  ```ruby
  String#upcase -> String
  ```
length # derived
  ```ruby
  String#length -> Integer
  ```
digits # chained
  ```ruby
  Integer#digits -> Array
  ```
upcase # assigned
  ```ruby
  String#upcase -> String
  ```
succ # both
  ```ruby
  Integer#succ -> Integer
  ```
upcase # guessed
  ```ruby
  String#upcase -> String
  ```

  *Guessed from name alone.*
shout # named
  ```ruby
  Person#shout
  ```

  *Guessed from name alone.*
upcase # constant
  ```ruby
  String#upcase -> String
  ```
shout # held
  ```ruby
  Person#shout
  ```
tally # missed
  ```ruby
  Counting#tally
  ```

  *Guessed from name alone.*
tally # class object
  ```ruby
  Counting#tally
  ```

  *Guessed from name alone.*
tally # guessed receiver
  ```ruby
  Counting#tally
  ```

  *Guessed from name alone.*
tally # closure
  ```ruby
  Counting#tally
  ```
"
        );
    }

    #[test]
    fn a_local_assigned_a_chain_that_does_not_type_falls_through_to_its_own_name() {
        // The fall-through, both directions, because either half alone is a bug.
        //
        // A chain whose *shape* is sound and whose *type* nothing states: `published` is a scope
        // this `Story` does not declare, like any unannotated class method. Not
        // `Story.where(...).first`, because the model's class side types that; this chain fails
        // even with the class side present, which shows the fall-through matters.
        //
        // Without the fall-through, writing the assignment would make the answer *worse* than no
        // assignment (which reaches the name rung and answers `Story`). That is the wrong shape for
        // a system built on ordered rungs.
        let source = "story = Story.published.first\nstory.comments\n";
        let (mut harness, _story, uri) = models_project(source);

        let fell_through = card(&mut harness, &uri, source, "comments");
        assert!(
            fell_through.contains("Story#comments"),
            "the failed chain has to end where a bare `story` ends: {fell_through}"
        );
        // With the label of the rung it reached. The fall-through is a *step*, not a sixth rung, so
        // it must not launder a guess into a derivation.
        assert!(
            fell_through.contains("Guessed from name alone"),
            "{fell_through}"
        );

        // The other direction, which makes the first safe. `comment` would guess `Comment`, which
        // really declares `comments`; if the spelling were asked before the assignment, this card
        // would say `Comment#comments` and be wrong about a chain the code states.
        let resolved = "comment = Story.new.parent_story\ncomment.comments\n";
        let other = harness.write("app/other.rb", resolved);
        harness.watch(&[&other]);
        let card = card(&mut harness, &other, resolved, "comments");
        assert!(card.contains("Story#comments"), "{card}");
        assert!(
            !card.contains("Comment#comments"),
            "a chain that resolves can never be displaced by a guess: {card}"
        );
        assert!(
            !card.contains("Guessed from name alone"),
            "and it keeps the tier it earned: {card}"
        );
    }

    #[test]
    fn the_fall_through_goes_off_with_the_rung_it_belongs_to() {
        // It reaches `types::named`, the same pair of rungs as a bare name, so
        // `[types] guess_from_names = false` turns it off with no switch of its own. Otherwise it
        // would be a guess the user had declined, arriving by an unnamed route.
        let source = "story = Story.where(published: true).first\nstory.title\n";

        let mut on = Harness::new();
        on.write("app/models/story.rb", STORY);
        let uri = on.write("app/main.rb", source);
        on.index();
        let guessed = card(&mut on, &uri, source, "title");
        assert!(guessed.contains("Story#title"), "{guessed}");
        assert!(guessed.contains("Guessed from name alone"), "{guessed}");

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n\n\
             [types]\nguess_from_names = false\n",
        )
        .unwrap();
        let mut off = Harness::at(dir, PositionEncoding::Utf16);
        off.write("app/models/story.rb", STORY);
        let uri = off.write("app/main.rb", source);
        off.index();
        // The receiver is no longer read off its name, so the member is only the name rung's match.
        let silenced = card(&mut off, &uri, source, "title");
        assert!(
            silenced.contains("Guessed from name alone"),
            "with the rung off there is nothing below the failed chain: {silenced}"
        );
    }

    #[test]
    fn a_block_parameter_is_typed_by_what_the_method_says_it_yields() {
        // The block parameter, end to end. `Story::Relation#each` is
        // `() { (Story) -> void } -> Story::Relation`. Reading only the return would drop the block
        // half, leaving `.each do |story|` to a *guess* and `.each do |instance|` (as one
        // application writes) with nothing.
        let source = "Story.where(id: 1).each do |instance|\n  instance.title\nend\n";
        let (mut harness, schema, uri) = rails_project(source);
        let author = harness.write(
            "app/models/author.rb",
            "class Author < ApplicationRecord\n  has_many :stories\nend\n",
        );
        harness.watch(&[&author]);

        // The member is found, the card names the column's file, and the jump lands on the schema
        // line that declared it, as for a column reached any other way.
        let card = card(&mut harness, &uri, source, "title");
        assert!(card.contains("Story#title"), "{card}");
        assert!(
            !card.contains("Guessed from name alone"),
            "still on the name rung: {card}"
        );

        let definition = harness.definition_at(&uri, source, "title");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(schema.as_str())
        );
    }

    #[test]
    fn a_literal_hands_its_block_and_its_caller_what_it_was_written_holding() {
        // The one receiver whose type *argument* is recoverable, end to end. `Array#each` is
        // `() { (E element) -> void }` and `Array#first` is `() -> E`. `E` is the receiver's first
        // argument, and a literal writes it down.
        let (mut harness, uri) = with_types("");
        assert_eq!(
            class_at(&mut harness, &uri, "[1, 2].each { |n| n.~ }"),
            "Integer"
        );
        assert_eq!(class_at(&mut harness, &uri, "[1, 2].first.~"), "Integer");
        assert_eq!(
            class_at(&mut harness, &uri, "%w[a b].each { |word| word.~ }"),
            "String"
        );
        // A name in front of the literal changes nothing: the assignment wraps the literal, and the
        // same holds for an instance variable's write.
        assert_eq!(
            class_at(
                &mut harness,
                &uri,
                "rows = [1, 2]\nrows.each { |row| row.~ }"
            ),
            "Integer"
        );
        assert_eq!(
            class_at(
                &mut harness,
                &uri,
                "class Ledger\n  def fill\n    @rows = [\"a\"]\n  end\n\n  \
                 def walk\n    @rows.each { |row| row.~ }\n  end\nend\n"
            ),
            "String"
        );
        // A literal holding no single class carries no argument, so `E` stays unanswered.
        let unheld = class_at(&mut harness, &uri, "[foo, bar].each { |o| o.~ }");
        assert!(unheld.starts_with("(everything"), "{unheld}");
        // **It survives a step.** `Array#first(1)` returns `Array[E]`, `E` is substituted at the
        // call, and the `Array` the next block is written on still says what it holds.
        assert_eq!(
            class_at(&mut harness, &uri, "[1, 2].first(1).each { |n| n.~ }"),
            "Integer"
        );
        // **A member reached through an ancestor is refused**: the whole safety argument.
        // `Enumerable[E]#walk` is declared over *its own* `E`, and the including class decides what
        // that is. `Array` writes `include Enumerable[E]` and would line up; `Hash` writes
        // `include Enumerable[[K, V]]` and would hand a `map` block the key instead of the pair. A
        // position means something only against its own list.
        let through = class_at(&mut harness, &uri, "[1, 2].walk { |n| n.~ }");
        assert!(through.starts_with("(everything"), "{through}");
    }

    #[test]
    fn what_a_generic_holds_survives_the_step_its_head_takes() {
        // The type argument across calls, end to end. The head and the argument answer two
        // questions, and both must cross a call, or `Array` chains and the block on it gets
        // nothing.
        let (mut harness, uri) = with_types("");
        // A signature naming its element outright: `String#scan` is `-> Array[String]`, with no
        // receiver involved.
        assert_eq!(
            class_at(
                &mut harness,
                &uri,
                "\"a,b\".scan(\",\").each { |part| part.~ }"
            ),
            "String"
        );
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".scan(\"a\").first.~"),
            "String"
        );
        // A signature naming the *receiver's* argument: `Array#first(n)` is `-> Array[E]`, so the
        // position is substituted at the call and the returned `Array` holds what the literal held.
        assert_eq!(
            class_at(&mut harness, &uri, "[1, 2].first(1).first.~"),
            "Integer"
        );
        // **`self` is not a step**, so a call returning the receiver returns its arguments with it.
        assert_eq!(
            class_at(&mut harness, &uri, "[1, 2].each { }.each { |n| n.~ }"),
            "Integer"
        );
        // A hash has two positions, and an index reads the second: `Hash#[]` is `(K key) -> V`.
        // That answers the common `settings[key].each`.
        assert_eq!(
            class_at(&mut harness, &uri, "{ \"a\" => 1 }[\"a\"].~"),
            "Integer"
        );
        assert_eq!(
            class_at(
                &mut harness,
                &uri,
                "{ \"a\" => 1 }.keys.each { |key| key.~ }"
            ),
            "String"
        );
        // A block parameter that is itself a generic carries its element into the block below:
        // `each_slice` hands over an `Array[E]`, not an `Array`.
        assert_eq!(
            class_at(
                &mut harness,
                &uri,
                "[1, 2, 3].each_slice(2) { |pair| pair.each { |n| n.~ } }"
            ),
            "Integer"
        );
        // **A method's own type variable is answered from the other side**, which no receiver can
        // do: `map` is `[U] () { (E) -> U } -> Array[U]`, and the block wrote the `U`. See
        // [`what_a_block_hands_back_is_what_the_call_hands_back`].
        assert_eq!(
            class_at(
                &mut harness,
                &uri,
                "[1, 2].map { |n| n.to_s }.each { |s| s.~ }"
            ),
            "String"
        );
    }

    /// The `U` of `[U] () { (E) -> U } -> Array[U]`: the block's own return type.
    ///
    /// The receiver cannot answer it (`Array[Integer]` and `Array[String]` reach the same
    /// declarations), so each case reads back the block's last expression, with every exit required
    /// to agree.
    #[test]
    fn what_a_block_hands_back_is_what_the_call_hands_back() {
        let (mut harness, uri) = with_types("");
        // The literal tail, which needs nothing typed first: a literal is the one expression this
        // module can always name.
        assert_eq!(
            class_at(
                &mut harness,
                &uri,
                "[1, 2].map { |n| \"x\" }.each { |s| s.~ }"
            ),
            "String"
        );
        // A call on the block parameter, which needs the parameter typed first. It is, because the
        // literal to the left carries its element in.
        assert_eq!(
            class_at(&mut harness, &uri, "[1, 2].map { |n| n.to_s }.first.~"),
            "String"
        );
        // **An empty block returns `nil`**, as Ruby says: `[1, 2].map { }` is `[nil, nil]`. An
        // answer, not a missing one.
        assert_eq!(
            class_at(&mut harness, &uri, "[1, 2].map { }.each { |n| n.~ }"),
            "NilClass"
        );
        // **`sort_by` shows how narrow the rule is.** It declares the same `[U]` and hands the
        // block the same `U`, but returns `Array[E]`, the *receiver's* element. The block says
        // nothing about that and is not asked.
        assert_eq!(
            class_at(
                &mut harness,
                &uri,
                "[1, 2].sort_by { |n| n.to_s }.each { |n| n.~ }"
            ),
            "Integer"
        );
        // **A block's value is one class**: two branches naming two are a union, and a position
        // holds none.
        let split = class_at(
            &mut harness,
            &uri,
            "[1, 2].map { |n| if n then n.to_s else n.succ end }.each { |x| x.~ }",
        );
        assert!(split.starts_with("(everything"), "{split}");
        // A `nil` branch makes the element `String?`: the unwritten `else` returns `nil`. A position
        // holds a bare class, so the element is not claimed at all rather than claimed `String`,
        // which would say no element is `nil` (#18).
        assert!(
            class_at(
                &mut harness,
                &uri,
                "[1, 2].map { |n| n.to_s if n }.each { |x| x.~ }"
            )
            .starts_with("(everything")
        );
        // **A `next` refuses the whole block.** It is a block's `return`, not in tail position, so
        // the walk does not see it; reading only the tail would be a confident half-answer.
        let escaped = class_at(
            &mut harness,
            &uri,
            "[1, 2].map { |n| next 1 if n\n n.to_s }.each { |x| x.~ }",
        );
        assert!(escaped.starts_with("(everything"), "{escaped}");
        // **The same variant read whole, not at a position.** `pair` is
        // `[X] () { ([String, Integer]) -> X } -> X`: the block's value *is* the call's, which is
        // how `File.open(path) { |f| f.read }` is a `String`.
        assert_eq!(
            class_at(&mut harness, &uri, "\"x\".pair { |p| 1 }.~"),
            "Integer"
        );
    }

    /// A block's exits are joined whole, whichever comes first: a later exit's `?`, `bool` and
    /// what its class holds count as much as the first's. Reading only the first's made the answer
    /// depend on branch order, and drew `Array[String]` for a block that can hand back `nil`.
    #[test]
    fn every_exit_of_a_block_counts_whatever_its_order() {
        let source = "\
class Pick
  def forward(c)
    [1, 2].map { |n| c ? \"x\" : maybe }
  end

  def backward(c)
    [1, 2].map { |n| c ? maybe : \"x\" }
  end

  def either(c)
    \"x\".pair { |p| c ? true : flag }
  end

  def held(c)
    \"x\".pair { |p| c ? [1] : [\"a\"] }
  end
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class Pick\n  def maybe: () -> String?\n  def flag: () -> bool\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def forward(c) -> Array
    [1, 2].map { |n: Integer| c ? \"x\" : maybe }
  def backward(c) -> Array
    [1, 2].map { |n: Integer| c ? maybe : \"x\" }
  def either(c) -> bool
  def held(c) -> Array"
        );
    }

    #[test]
    fn a_body_that_hands_back_a_literal_hands_back_what_it_held() {
        // The body rung, where an application's own `def` gets its type. An exit is no more a step
        // than an assignment is, and every exit must agree about the position, as about the class.
        let (mut harness, uri) = with_types("");
        assert_eq!(
            class_at(
                &mut harness,
                &uri,
                "class Ledger\n  def rows\n    [1, 2]\n  end\n\n  \
                 def walk\n    rows.each { |row| row.~ }\n  end\nend\n"
            ),
            "Integer"
        );
        // Two exits holding different things agree on `Array` and nothing inside it, the same rule
        // two signature arms get.
        let split = class_at(
            &mut harness,
            &uri,
            "class Ledger\n  def rows\n    return [1, 2] if @flag\n\n    [\"a\"]\n  end\n\n  \
             def walk\n    rows.each { |row| row.~ }\n  end\nend\n",
        );
        assert!(split.starts_with("(everything"), "{split}");
    }

    #[test]
    fn a_body_that_hands_back_self_hands_back_the_receiver_and_not_the_class_it_is_written_in() {
        // A Ruby body saying `self` means "the receiver's type", as RBS's `self` does. Resolving it
        // in the scope the `def` is written in would make a method written once in a module answer
        // *that module* for every including class: wrong, not missing.
        let (mut harness, uri) = with_types("");
        let mixin = "module Probe\n  def probe\n    self\n  end\n\n                       def maybe_probe\n    self if @on\n  end\nend\n\n                     class Integer\n  include Probe\nend\n\n                     class String\n  include Probe\nend\n\n";

        // The same `def`, two receivers, two answers.
        assert_eq!(
            class_at(&mut harness, &uri, &format!("{mixin}1.probe.~\n")),
            "Integer"
        );
        assert_eq!(
            class_at(&mut harness, &uri, &format!("{mixin}\"x\".probe.~\n")),
            "String"
        );

        // **A `nil` exit does not spoil it, and that is the real target.** ActiveSupport's
        // `presence` is `self if present?`: one `self` exit plus the implicit `nil` of an unwritten
        // branch. The `nil` folds into the mark, one bare `self` is left, and the members offered
        // are still the receiver's.
        assert_eq!(
            class_at(&mut harness, &uri, &format!("{mixin}1.maybe_probe.~\n")),
            "Integer"
        );

        // A body that says anything *else* beside `self` is an ordinary body: two exits, two
        // classes, a union.
        let mixed = class_at(
            &mut harness,
            &uri,
            "module Probe\n  def probe\n    return \"x\" if @flag\n\n    self\n  end\nend\n\n             class Integer\n  include Probe\nend\n\n1.probe.~\n",
        );
        assert!(
            mixed.starts_with("(everything") || mixed == "(nothing)",
            "{mixed}"
        );
    }

    #[test]
    fn a_card_on_an_instance_variable_names_the_assignment_it_was_typed_from() {
        // The other half of the same answer, which is why both requests read one resolution:
        // `definition` jumps to the assignment, and the card says the type came from it. Both are
        // the scope walk winning at a span the graph would answer with `Story#@title`, which names
        // the variable but not its contents.
        let mut harness = Harness::new();
        let source = "class Title\nend\n\nclass Story\n  def initialize\n    \
                      @title = Title.new\n  end\n\n  def show\n    @title\n  end\nend\n";
        let uri = harness.write("lib/story.rb", source);
        harness.index();

        for needle in ["@title = Title", "@title\n  end\nend"] {
            let card = harness.hover_at(&uri, source, needle)["contents"]["value"]
                .as_str()
                .unwrap_or("null")
                .to_owned();
            assert_eq!(card, "```ruby\nStory#@title: Title\n```", "at {needle:?}");
        }
    }

    #[test]
    fn an_instance_variable_assigned_an_active_record_chain_is_not_typed() {
        // The common case, pinned. `Story.where(...)` is a chain whose return nothing declares, so
        // the convention gives `@stories` nothing, and `@stories` does not spell a class, so the
        // guess gives nothing either. The ceiling here is the missing annotations, not the
        // machinery.
        let mut harness = Harness::new();
        rails_app(&harness);
        // The template calls a method the *model* declares, on purpose: an untyped receiver and one
        // typed to the wrong class both answer `Story#title`, and only the footnote tells them
        // apart. An undeclared method would answer `null` either way and pin nothing.
        let source = "<%= @stories.title %>\n";
        let view = harness.write("app/views/stories/index.html.erb", source);
        harness.index();

        let markdown = card(&mut harness, &view, source, "title");
        assert!(markdown.contains("Guessed from name alone"), "{markdown}");
        assert!(!markdown.contains("StoriesController"), "{markdown}");
    }

    #[test]
    fn a_guess_never_displaces_an_answer_the_code_states() {
        // The rung order: what makes the last rung safe to ship. `@user` is assigned a `Draft` in
        // its own class, and a `User` class sits in the graph ready to be guessed. If the rungs
        // were reversed, or tried in parallel, this card would say `User`.
        let mut harness = Harness::new();
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        harness.write("app/models/user.rb", "class User\n  def slug\n  end\nend\n");
        harness.write(
            "app/models/draft.rb",
            "class Draft\n  def slug\n  end\nend\n",
        );
        let source = "\
class Session
  def start
    @user = Draft.new
  end

  def render
    @user.slug
  end
end
";
        let uri = harness.write("app/session.rb", source);
        harness.index();

        let markdown = card(&mut harness, &uri, source, "slug");
        assert!(markdown.contains("Draft#slug"), "{markdown}");
        assert!(!markdown.contains(GUESS_FOOTNOTE), "{markdown}");
        assert!(
            !markdown.contains("Guessed from name alone"),
            "an assignment in the same class is not a guess: {markdown}"
        );
    }

    #[test]
    fn the_guess_can_be_turned_off_and_the_convention_stays() {
        // The setting exists because the guess is the one answer allowed to be wrong, and a user
        // who wants only checkable answers should get them. It must *not* take the derived tier
        // with it: a controller and a line can be checked.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n\n\
             [types]\nguess_from_names = false\n",
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        let view = rails_app(&harness);
        let guessed_source = "<%= @story.title %>\n";
        let elsewhere = harness.write("app/views/comments/show.html.erb", guessed_source);
        harness.index();

        let source = "<h1><%= @story.title %></h1>\n";
        let kept = card(&mut harness, &view, source, "title");
        assert!(kept.contains("Story#title"), "{kept}");
        assert!(!kept.contains(GUESS_FOOTNOTE), "{kept}");

        // The same variable and class, with no controller to reach it through: with the guess off,
        // nothing is left to say.
        let silenced = card(&mut harness, &elsewhere, guessed_source, "title");
        assert!(silenced.contains("Guessed from name alone"), "{silenced}");
    }

    /// Every place a jump answers with, as `file.rb:line:character`, short enough to assert whole.
    /// The file name, not the URI, because the harness's tempdir path changes every run.
    fn jumps(harness: &mut Harness, uri: &DocUri, source: &str, needle: &str) -> Vec<String> {
        let found = harness.definition_at(uri, source, needle);
        found
            .as_array()
            .into_iter()
            .flatten()
            .map(|link| {
                let file = link["targetUri"]
                    .as_str()
                    .unwrap_or_default()
                    .rsplit('/')
                    .next()
                    .unwrap_or_default();
                let at = &link["targetSelectionRange"]["start"];
                format!(
                    "{file}:{}:{}",
                    at["line"].as_u64().unwrap_or_default() + 1,
                    at["character"].as_u64().unwrap_or_default()
                )
            })
            .collect()
    }

    #[test]
    fn a_templates_instance_variable_jumps_to_the_controller_that_assigns_it() {
        // The card cites a controller and a line, and the jump at the same cursor must land there
        // too. A template's writes are always in another file, so searching only the cursor's
        // buffer finds nothing. One convention, asked twice, answers twice.
        let mut harness = Harness::new();
        let view = rails_app(&harness);
        harness.index();

        let source = "<h1><%= @story.title %></h1>\n";
        assert_eq!(
            jumps(&mut harness, &view, source, "@story"),
            ["stories_controller.rb:3:4"]
        );

        // The origin is the variable the reader stands on, not the call after it.
        let found = harness.definition_at(&view, source, "@story");
        assert_eq!(found[0]["originSelectionRange"]["start"]["character"], 8);
        assert_eq!(found[0]["originSelectionRange"]["end"]["character"], 14);
    }

    #[test]
    fn a_write_that_types_nothing_is_still_a_place() {
        // The common real-controller case, which the card cannot answer: nothing declares what
        // `Story.where(...)` returns, so `@stories` has no type. Its assignment line is still what
        // go-to-definition wants, so the jump reads the writes themselves, typed or not.
        let mut harness = Harness::new();
        rails_app(&harness);
        let source = "<%= @stories.count %>\n";
        let view = harness.write("app/views/stories/index.html.erb", source);
        harness.index();

        assert_eq!(
            jumps(&mut harness, &view, source, "@stories"),
            ["stories_controller.rb:7:4"]
        );
    }

    #[test]
    fn a_reflective_write_that_builds_the_name_reaches_every_read_and_is_a_place_to_jump_to() {
        // An admin engine's `ResourceController`: `instance_variable_set("@#{name}", …)` can
        // write any variable, and its file never spells the one read. The walk used to skip such a
        // file, so `@order` was typed from the one write that spells it; the rule says a write
        // nothing types refuses the read. And where nothing spells the name, the jump lands on the
        // call that writes it.
        let mut harness = Harness::new();
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        harness.write("app/models/order.rb", "class Order\nend\n");
        harness.write(
            "app/controllers/resource_controller.rb",
            "class ResourceController\n  def load\n    instance_variable_set(\"@#{name}\", \
             find)\n  end\nend\n",
        );
        harness.write(
            "app/controllers/orders_controller.rb",
            "class OrdersController < ResourceController\n  def show\n    @order = Order.new\n  \
             end\nend\n",
        );
        let source = "<% held = @order %>\n<% @orders %>\n";
        let view = harness.write("app/views/orders/show.html.erb", source);
        harness.index();

        assert_eq!(drawn_hints(source, &harness.hints_in(&view)), "null");
        assert_eq!(
            jumps(&mut harness, &view, source, "@order "),
            ["orders_controller.rb:3:4"]
        );
        assert_eq!(
            jumps(&mut harness, &view, source, "@orders"),
            ["resource_controller.rb:3:4"]
        );
    }

    #[test]
    fn a_write_the_file_does_not_spell_is_lit_where_the_jump_lands() {
        // An admin engine's `ResourcesController` reading `@page`: the file
        // writes it only through a name it builds, so the jump lands on the call, and the
        // highlight lights the call too. An accessor is the same.
        let source = "\
class Pages
  attr_writer :title

  def show
    @page
    @title
  end

  def load
    instance_variable_set(\"@#{name}\", 1)
  end
end
";
        let mut harness = Harness::new();
        let uri = harness.write("app/controllers/pages.rb", source);
        harness.index();

        assert_eq!(
            harness.agreement_map(&uri, &cursor_after(source, "    @pag")),
            "    @page\n    rrrrr\n    instance_variable_set(\"@#{name}\", 1)\n    \
             WWWWWWWWWWWWWWWWWWWWW"
        );
        assert_eq!(
            harness.agreement_map(&uri, &cursor_after(source, "    @tit")),
            "  attr_writer :title\n               WWWWW\n    @title\n    rrrrrr\n    \
             instance_variable_set(\"@#{name}\", 1)\n    WWWWWWWWWWWWWWWWWWWWW"
        );
    }

    #[test]
    fn a_partial_s_jump_leaves_out_a_renderer_whose_ancestry_cannot_be_read() {
        // An admin engine's `_adjustments_table`: one of 71 renderers has an ancestor the graph
        // cannot resolve. The card refuses, never a partial fold; the jump lists the writes of
        // every renderer it can read, on the user's decision.
        let mut harness = Harness::new();
        rails_app(&harness);
        harness.write(
            "app/controllers/broken_controller.rb",
            "class BrokenController < Missing::Base\n  def index\n    @story = Story.new\n  \
             end\nend\n",
        );
        let broken = "<%= @missing %>\n";
        let own = harness.write("app/views/broken/index.html.erb", broken);
        let source = "<% held = @story %>\n";
        let view = harness.write("app/views/shared/_header.html.erb", source);
        harness.index();

        assert_eq!(drawn_hints(source, &harness.hints_in(&view)), "null");
        assert_eq!(
            jumps(&mut harness, &view, source, "@story"),
            ["stories_controller.rb:3:4"]
        );
        // Where no renderer can be read, there is no list.
        assert!(jumps(&mut harness, &own, broken, "@missing").is_empty());
    }

    #[test]
    fn a_helper_s_variable_is_every_renderer_s() {
        // An `I18nHelper` reading `@home_page`: a helper module's methods run on
        // the view, whose variables are its renderer's. Its own writes count too. A
        // `StoriesController` has no view of its own and renders another's by name, which makes
        // it a renderer as surely as a folder would.
        let mut harness = Harness::new();
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        harness.write("app/models/page.rb", "class Page\nend\n");
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def index\n    @home_page = Page.new\n    render \
             template: \"articles/index\"\n  end\nend\n",
        );
        harness.write(
            "app/controllers/feeds_controller.rb",
            "class FeedsController\n  def index\n    @home_page = 1\n  end\nend\n",
        );
        // A render with a receiver renders with that object's variables, not this class's.
        harness.write(
            "app/controllers/reports_controller.rb",
            "class ReportsController\n  def index\n    @home_page = 1\n    \
             ApplicationController.render(template: \"articles/index\")\n  end\nend\n",
        );
        harness.write("app/views/articles/index.html.erb", "x\n");
        let source = "module I18nHelper\n  def home?\n    held = @home_page\n  end\nend\n";
        let helper = harness.write("app/helpers/i18n_helper.rb", source);
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&helper)),
            "  def home? -> Page?\n    held: Page? = @home_page"
        );
        assert_eq!(
            jumps(&mut harness, &helper, source, "@home_page"),
            ["stories_controller.rb:3:4"]
        );
    }

    #[test]
    fn a_braceless_hash_is_no_array_to_a_create_that_asks() {
        // A model's own `def self.create!` whose body asks `attributes.is_a?(Array)`:
        // a braceless hash is not an `Array`, so the call is the record, not `Story | Array`.
        // (an application's `Account.create!(name: …)` read Rails' `def` beside the generated row as a
        // second body; that path needs a bundle this harness does not have, and was checked on
        // the corpus.)
        let caller = "a = Story.create!(title: \"x\")\nb = Story.create!\n";
        let (mut harness, _story, uri) = models_project(caller);
        harness.write(
            "app/models/story_create.rb",
            "class Story\n  def self.create!(attributes = nil)\n    \
             if attributes.is_a?(Array)\n      attributes\n    else\n      new\n    end\n  end\nend\n",
        );
        harness.index();
        assert_eq!(
            drawn_hints(caller, &harness.hints_in(&uri)),
            "a: Story = Story.create!(title: \"x\")\nb: Story = Story.create!"
        );
    }

    #[test]
    fn a_partial_s_local_is_what_its_render_calls_pass() {
        // A bare name in a partial is the union of what every render call that can
        // name the partial passes under it: a local, a collection's element under `as:` with its
        // counter, a partial named without a directory, a strict-locals default. Locals nothing
        // can list refuse, and a name no call passes stays a call.
        let mut harness = Harness::new();
        harness.write(
            "sig/core.rbs",
            "class NilClass\nend\nclass TrueClass\nend\nclass FalseClass\nend\n\
             class Integer\nend\nclass String\nend\nclass Hash\nend\n\
             class Array[E]\n  def each: () { (E) -> void } -> Array[E]\n  \
             def to_ary: () -> Array[E]\nend\n",
        );
        harness.write("app/models/story.rb", "class Story\nend\n");
        harness.write("app/models/comment.rb", "class Comment\nend\n");
        harness.write(
            "sig/comment.rbs",
            "class Comment\n  def self.recent: () -> Array[Comment]\nend\n",
        );
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def show\n    @story = Story.new\n  end\nend\n",
        );
        harness.write(
            "app/views/stories/show.html.erb",
            "<%= render \"stories/row\", story: @story, compact: true %>\n\
             <%= render partial: \"comments/comment\", collection: Comment.recent, as: :note %>\n\
             <%= render \"badge\", story: Story.new %>\n\
             <%= render \"stories/strict\", story: Story.new %>\n\
             <%= render \"stories/refused\", options %>\n",
        );
        let row_source = "<% a = story %>\n<% b = compact %>\n<% c = unknown_local %>\n";
        let row = harness.write("app/views/stories/_row.html.erb", row_source);
        let comment_source = "<% d = note %>\n<% e = note_counter %>\n";
        let comment = harness.write("app/views/comments/_comment.html.erb", comment_source);
        let badge_source = "<% f = story %>\n";
        let badge = harness.write("app/views/shared/_badge.html.erb", badge_source);
        let strict_source =
            "<%# locals: (story:, flag: false) -%>\n<% g = flag %>\n<% h = story %>\n";
        let strict = harness.write("app/views/stories/_strict.html.erb", strict_source);
        let refused_source = "<% i = story %>\n";
        let refused = harness.write("app/views/stories/_refused.html.erb", refused_source);
        harness.index();

        assert_eq!(
            drawn_hints(row_source, &harness.hints_in(&row)),
            "<% a: Story? = story %>\n<% b: true = compact %>"
        );
        assert_eq!(
            drawn_hints(comment_source, &harness.hints_in(&comment)),
            "<% d: Comment = note %>\n<% e: Integer = note_counter %>"
        );
        assert_eq!(
            drawn_hints(badge_source, &harness.hints_in(&badge)),
            "<% f: Story = story %>"
        );
        assert_eq!(
            drawn_hints(strict_source, &harness.hints_in(&strict)),
            "<% g: false = flag %>\n<% h: Story = story %>"
        );
        assert_eq!(
            drawn_hints(refused_source, &harness.hints_in(&refused)),
            "null"
        );
        // The card is a variable's, and the jump lands on each value passed.
        assert_eq!(
            harness.hover_at(&row, row_source, "story")["contents"]["value"],
            "```ruby\nstory: Story?\n```"
        );
        assert_eq!(
            jumps(&mut harness, &row, row_source, "story"),
            ["show.html.erb:1:33"]
        );
        assert_eq!(
            jumps(&mut harness, &comment, comment_source, "note_counter"),
            ["show.html.erb:2:4"]
        );
    }

    #[test]
    fn an_object_renders_its_class_s_partial_and_doubt_refuses() {
        // `render @stories` renders each element's own partial with it under the
        // partial's name, and `render Comment.new` the record's. A partial passing its own local
        // back to itself, a helper answering where a call passes nothing, and a component's
        // `render`, which this cannot place, each refuse.
        let mut harness = Harness::new();
        harness.write(
            "sig/core.rbs",
            "class NilClass\nend\nclass Integer\nend\nclass String\nend\n\
             class Array[E]\n  def each: () { (E) -> void } -> Array[E]\n  \
             def to_ary: () -> Array[E]\nend\n",
        );
        harness.write("app/models/story.rb", "class Story\nend\n");
        harness.write("app/models/comment.rb", "class Comment\nend\n");
        harness.write(
            "sig/story.rbs",
            "class Story\n  def self.recent: () -> Array[Story]\nend\n",
        );
        harness.write(
            "app/helpers/stories_helper.rb",
            "module StoriesHelper\n  def badge_title\n    \"x\"\n  end\nend\n",
        );
        harness.write(
            "app/components/card_component.rb",
            "class CardComponent\n  def call\n    render partial: \"stories/card\", locals: { story: 1 }\n  \
             end\nend\n",
        );
        harness.write(
            "app/views/stories/index.html.erb",
            "<%= render Story.recent %>\n<%= render Comment.new %>\n\
             <%= render \"stories/badge\", badge_title: 1 %>\n<%= render \"stories/badge\" %>\n\
             <%= render \"stories/card\", story: Story.new %>\n\
             <%= render \"stories/loose\", options %>\n",
        );
        let story_source = "<% a = story %>\n<% b = story_counter %>\n";
        let story = harness.write("app/views/stories/_story.html.erb", story_source);
        let comment_source =
            "<% c = comment %>\n<%= render \"comments/comment\", comment: comment %>\n";
        let comment = harness.write("app/views/comments/_comment.html.erb", comment_source);
        let badge_source = "<% d = badge_title %>\n";
        let badge = harness.write("app/views/stories/_badge.html.erb", badge_source);
        let card_source = "<% e = story %>\n";
        let card = harness.write("app/views/stories/_card.html.erb", card_source);
        // Nothing says `badge_title` is a local here, so the call whose locals cannot be read
        // leaves it the helper's.
        let loose_source = "<% f = badge_title %>\n";
        let loose = harness.write("app/views/stories/_loose.html.erb", loose_source);
        harness.index();

        assert_eq!(
            drawn_hints(story_source, &harness.hints_in(&story)),
            "<% a: Story = story %>\n<% b: Integer = story_counter %>"
        );
        assert_eq!(
            drawn_hints(comment_source, &harness.hints_in(&comment)),
            "null"
        );
        assert_eq!(drawn_hints(badge_source, &harness.hints_in(&badge)), "null");
        assert_eq!(drawn_hints(card_source, &harness.hints_in(&card)), "null");
        assert_eq!(
            drawn_hints(loose_source, &harness.hints_in(&loose)),
            "<% f: String = badge_title %>"
        );
    }

    #[test]
    fn a_partial_s_local_reads_every_shape_of_call() {
        // A partial local's edges: a call passing nothing beside one passing the local (no helper of the
        // name, so the local stands), a `**rest` strict-locals comment, a spec's render (fenced
        // out), a partial rendering itself with a value of its own, an object nothing types, one
        // whose class writes `to_partial_path`, a collection whose elements nothing types, and a
        // default whose class the project does not declare.
        let mut harness = Harness::new();
        harness.write(
            "sig/core.rbs",
            "class NilClass\nend\nclass Integer\nend\nclass String\nend\n\
             class Array[E]\n  def each: () { (E) -> void } -> Array[E]\n  \
             def to_ary: () -> Array[E]\nend\n",
        );
        harness.write("app/models/story.rb", "class Story\nend\n");
        harness.write(
            "app/models/card.rb",
            "class Card\n  def to_partial_path\n    \"stories/row\"\n  end\nend\n",
        );
        harness.write(
            "sig/story.rbs",
            "class Story\n  def self.loose: () -> Array[untyped]\nend\n",
        );
        harness.write(
            "app/views/stories/index.html.erb",
            "<%= render \"stories/row\", story: Story.new %>\n<%= render \"stories/row\" %>\n\
             <%= render \"stories/open\", story: Story.new, extra: 1 %>\n\
             <%= render \"stories/float\" %>\n<%= render mystery %>\n\
             <%= render Card.new %>\n<%= render Story.loose %>\n",
        );
        harness.write(
            "spec/views/row_spec.rb",
            "render partial: \"stories/row\", locals: { story: 1 }\n",
        );
        let row_source = "<% a = story %>\n<%= render \"stories/row\", story: Story.new %>\n";
        let row = harness.write("app/views/stories/_row.html.erb", row_source);
        let open_source = "<%# locals: (story:, **rest) -%>\n<% b = story %>\n<% c = extra %>\n";
        let open = harness.write("app/views/stories/_open.html.erb", open_source);
        let float_source = "<%# locals: (ratio: 1.5) -%>\n<% d = ratio %>\n";
        let float = harness.write("app/views/stories/_float.html.erb", float_source);
        let own_source = "<% e = story %>\n<% f = card %>\n";
        let own = harness.write("app/views/stories/_story.html.erb", own_source);
        harness.index();

        assert_eq!(
            drawn_hints(row_source, &harness.hints_in(&row)),
            "<% a: Story = story %>"
        );
        assert_eq!(
            drawn_hints(open_source, &harness.hints_in(&open)),
            "<% b: Story = story %>\n<% c: Integer = extra %>"
        );
        // `Float` is no class this project declares, so the default says nothing it can name.
        assert_eq!(drawn_hints(float_source, &harness.hints_in(&float)), "null");
        // `render mystery` may render any partial with its object under the partial's own name.
        assert_eq!(drawn_hints(own_source, &harness.hints_in(&own)), "null");
    }

    #[test]
    fn a_partial_s_local_refuses_where_a_call_beside_it_cannot_be_read() {
        // A call passing `story` beside one whose locals nobody can list, in the same
        // view: the second may pass anything, so `story` refuses. A strict-locals comment that
        // does not declare a name makes it no local. An object whose class writes its own
        // `to_partial_path` may render this partial under its own name, which refuses.
        let mut harness = Harness::new();
        harness.write("sig/core.rbs", "class NilClass\nend\nclass Integer\nend\n");
        harness.write("app/models/story.rb", "class Story\nend\n");
        harness.write(
            "app/models/note.rb",
            "class Note\n  def to_partial_path\n    \"notes/note\"\n  end\nend\n",
        );
        harness.write(
            "app/views/stories/index.html.erb",
            "<%= render \"stories/row\", story: Story.new %>\n\
             <%= render \"stories/row\", options %>\n\
             <%= render \"stories/strict\", story: Story.new, other: 1 %>\n\
             <%= render Note.new %>\n",
        );
        let row_source = "<% a = story %>\n";
        let row = harness.write("app/views/stories/_row.html.erb", row_source);
        let strict_source = "<%# locals: (story:) -%>\n<% b = other %>\n";
        let strict = harness.write("app/views/stories/_strict.html.erb", strict_source);
        let note_source = "<% c = note %>\n";
        let note = harness.write("app/views/notes/_note.html.erb", note_source);
        harness.index();
        assert_eq!(drawn_hints(row_source, &harness.hints_in(&row)), "null");
        assert_eq!(
            drawn_hints(strict_source, &harness.hints_in(&strict)),
            "null"
        );
        assert_eq!(drawn_hints(note_source, &harness.hints_in(&note)), "null");
    }

    #[test]
    fn a_jbuilder_view_is_a_template() {
        // A jbuilder view reads its controller's variables, `json` is a
        // `JbuilderTemplate`, and `json.partial!` and a key written with `partial:` pass a
        // jbuilder partial its locals. A JSON lookup finds no ERB partial.
        let mut harness = Harness::new();
        harness.write(
            "sig/core.rbs",
            "class NilClass\nend\nclass JbuilderTemplate\nend\n",
        );
        harness.write("app/models/story.rb", "class Story\nend\n");
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def show\n    @story = Story.new\n  end\nend\n",
        );
        let show_source = "a = @story\nb = json\njson.partial! \"stories/story\", story: @story\n\
                           json.author @story, partial: \"users/user\", as: :user\n";
        let show = harness.write("app/views/stories/show.json.jbuilder", show_source);
        let story_source = "c = story\n";
        let story = harness.write("app/views/stories/_story.json.jbuilder", story_source);
        let html_source = "<% d = story %>\n";
        let html = harness.write("app/views/stories/_story.html.erb", html_source);
        let user_source = "e = user\n";
        let user = harness.write("app/views/users/_user.json.jbuilder", user_source);
        harness.index();

        assert_eq!(
            drawn_hints(show_source, &harness.hints_in(&show)),
            "a: Story? = @story\nb: JbuilderTemplate = json"
        );
        assert_eq!(
            drawn_hints(story_source, &harness.hints_in(&story)),
            "c: Story? = story"
        );
        assert_eq!(drawn_hints(html_source, &harness.hints_in(&html)), "null");
        assert_eq!(
            drawn_hints(user_source, &harness.hints_in(&user)),
            "e: Story? = user"
        );
        assert_eq!(
            jumps(&mut harness, &show, show_source, "@story"),
            ["stories_controller.rb:3:4"]
        );
    }

    #[test]
    fn a_json_view_s_controller_renders_no_erb_partial() {
        // An ERB partial runs on the classes whose ERB views may render it: a controller
        // with a jbuilder view alone writes `@story` with something nothing types, and must not make
        // the partial's read refuse.
        let mut harness = Harness::new();
        harness.write("sig/core.rbs", "class NilClass\nend\n");
        harness.write("app/models/story.rb", "class Story\nend\n");
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def show\n    @story = Story.new\n  end\nend\n",
        );
        harness.write("app/views/stories/show.html.erb", "x\n");
        harness.write(
            "app/controllers/feeds_controller.rb",
            "class FeedsController\n  def show\n    @story = anything\n  end\nend\n",
        );
        harness.write("app/views/feeds/show.json.jbuilder", "json.id 1\n");
        let head_source = "<% a = @story %>\n";
        let head = harness.write("app/views/layouts/_head.html.erb", head_source);
        harness.index();
        assert_eq!(
            drawn_hints(head_source, &harness.hints_in(&head)),
            "<% a: Story? = @story %>"
        );
    }

    #[test]
    fn a_partial_s_local_needs_the_view_convention() {
        let mut harness = Harness::configured("[rails]\nviews = false\n");
        harness.write("app/models/story.rb", "class Story\nend\n");
        harness.write(
            "app/views/stories/index.html.erb",
            "<%= render \"stories/row\", story: Story.new %>\n",
        );
        let row_source = "<% a = story %>\n";
        let row = harness.write("app/views/stories/_row.html.erb", row_source);
        harness.index();
        assert_eq!(drawn_hints(row_source, &harness.hints_in(&row)), "null");
    }

    #[test]
    fn with_the_view_convention_off_a_helper_and_a_partial_read_no_renderer() {
        // `[rails] views = false` is the one gate: a helper's variables are its module's own, and a
        // partial is no template the convention reads.
        let mut harness = Harness::configured("[rails]\nviews = false\n");
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        harness.write("app/models/page.rb", "class Page\nend\n");
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def index\n    @home_page = Page.new\n  end\nend\n",
        );
        harness.write("app/views/stories/index.html.erb", "x\n");
        let source = "module I18nHelper\n  def home?\n    held = @home_page\n  end\nend\n";
        let helper = harness.write("app/helpers/i18n_helper.rb", source);
        let partial = "<% held = @home_page %>\n";
        let row = harness.write("app/views/stories/_row.html.erb", partial);
        harness.index();

        assert!(jumps(&mut harness, &helper, source, "@home_page").is_empty());
        assert!(jumps(&mut harness, &row, partial, "@home_page").is_empty());
    }

    #[test]
    fn every_renderer_is_held_until_the_graph_or_the_convention_moves() {
        // Held across requests with the graph (`Indexed::renderers`), not found again per read. A
        // controller indexed later renders too, and a reloaded `[rails] views = false` takes every
        // one away.
        let mut harness = Harness::new();
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        harness.write("app/models/page.rb", "class Page\nend\n");
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def index\n    @home_page = Page.new\n  end\nend\n",
        );
        harness.write("app/views/stories/index.html.erb", "x\n");
        let source = "module I18nHelper\n  def home?\n    held = @home_page\n  end\nend\n";
        let helper = harness.write("app/helpers/i18n_helper.rb", source);
        harness.index();
        assert_eq!(
            jumps(&mut harness, &helper, source, "@home_page"),
            ["stories_controller.rb:3:4"]
        );

        harness.write(
            "app/controllers/feeds_controller.rb",
            "class FeedsController\n  def index\n    @home_page = Page.new\n  end\nend\n",
        );
        harness.write("app/views/feeds/index.html.erb", "x\n");
        harness.index();
        assert_eq!(
            jumps(&mut harness, &helper, source, "@home_page"),
            ["feeds_controller.rb:3:4", "stories_controller.rb:3:4"]
        );

        std::fs::write(
            harness.root.path().join("ya-lsp.toml"),
            "[rails]\nviews = false\n",
        )
        .unwrap();
        harness.run(crate::analysis::Task::ReloadConfig);
        assert!(jumps(&mut harness, &helper, source, "@home_page").is_empty());
    }

    #[test]
    fn a_variable_a_symbol_names_is_read_on_its_object() {
        // `delegate :render, to: :@template` and `obj.instance_variable_get(:@x)` name
        // a variable no read spells. A macro's is its class's instances'; the reflective read's is
        // its receiver's. The card, the jump and the highlight answer as for a read.
        let source = "\
class Template
end

class Builder
  delegate :render, to: :@template
  instance_variable_defined?(:@template)

  def initialize
    @template = Template.new
  end
end

built = Builder.new
built.instance_variable_get(:@template)
unknown.instance_variable_get(:@template) # u
";
        let (mut harness, uri) = with_rbs(
            source,
            "module Kernel\n  def instance_variable_get: (untyped) -> untyped\nend\n",
        );
        let delegated = card(&mut harness, &uri, source, "@template\n");
        assert!(delegated.contains("Template"), "{delegated}");
        assert_eq!(
            jumps(&mut harness, &uri, source, "@template\n"),
            ["widget.rb:9:4"]
        );
        assert_eq!(
            harness.agreement_map(&uri, &cursor_after(source, "to: :@tem")),
            "  delegate :render, to: :@template\n\
             \u{20}                        rrrrrrrrr\n    @template = Template.new\n    WWWWWWWWW"
        );
        let reflected = card(&mut harness, &uri, source, "@template)\nunknown");
        assert!(reflected.contains("Template"), "{reflected}");
        assert_eq!(
            jumps(&mut harness, &uri, source, "@template)\nunknown"),
            ["widget.rb:9:4"]
        );
        // A receiver nothing types names no object.
        let unknown = harness.hover_at(&uri, source, "@template) # u");
        assert!(unknown.is_null(), "{unknown}");
        // A macro's symbol that is not a variable's name is not this question, and neither is a
        // reflective call's: what it reaches decides, and here no signature declares it.
        let named = jumps(&mut harness, &uri, source, "render, to");
        assert!(!named.contains(&"widget.rb:9:4".to_owned()), "{named:?}");
        assert!(jumps(&mut harness, &uri, source, "@template)\n\n").is_empty());
    }

    #[test]
    fn a_controller_reopened_across_files_answers_with_every_write() {
        // Two writes in two files, ordered by URI, not graph order, so two runs agree. Several
        // possible writes in several files is a list, and `definition` can answer a list.
        let mut harness = Harness::new();
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def show\n    @story = Story.new\n  end\nend\n",
        );
        harness.write(
            "app/controllers/stories_controller_extra.rb",
            "class StoriesController\n  def preview\n    @story = Story.new\n  end\nend\n",
        );
        let source = "<%= @story.title %>\n";
        let view = harness.write("app/views/stories/show.html.erb", source);
        harness.index();

        assert_eq!(
            jumps(&mut harness, &view, source, "@story"),
            [
                "stories_controller.rb:3:4",
                "stories_controller_extra.rb:3:4"
            ]
        );
    }

    #[test]
    fn a_layout_reads_the_variables_of_every_class_whose_views_it_wraps() {
        // A layout's path names no class: `layouts/application` spells a `LayoutsController`
        // nobody writes. Its variables are those of every class Rails renders in it, by the class's
        // nearest `layout`, else its own name: the admin controllers' `layout "admin"`, the feeds'
        // own `layouts/feeds`, a mailer's `layout "mailer"`, and `layouts/application` for the
        // rest. A class in another layout never reaches it.
        let mut harness = Harness::new();
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        for model in ["Page", "User", "Notice", "Feed"] {
            harness.write(
                &format!("app/models/{}.rb", model.to_lowercase()),
                &format!("class {model}\nend\n"),
            );
        }
        harness.write(
            "app/controllers/application_controller.rb",
            "class ApplicationController\nend\n",
        );
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController < ApplicationController\n  def show\n    @title = Page.new\n  \
             end\nend\n",
        );
        harness.write("app/views/stories/show.html.erb", "x\n");
        harness.write(
            "app/controllers/admin/base_controller.rb",
            "module Admin\n  class BaseController < ApplicationController\n    layout \"admin\"\n  \
             end\nend\n",
        );
        harness.write(
            "app/controllers/admin/users_controller.rb",
            "module Admin\n  class UsersController < BaseController\n    def index\n      \
             @title = User.new\n    end\n  end\nend\n",
        );
        harness.write("app/views/admin/users/index.html.erb", "x\n");
        harness.write(
            "app/controllers/feeds_controller.rb",
            "class FeedsController < ApplicationController\n  def index\n    @title = Feed.new\n  \
             end\nend\n",
        );
        harness.write("app/views/feeds/index.html.erb", "x\n");
        harness.write("app/views/layouts/feeds.html.erb", "x\n");
        // Renders with no layout, and a `render` whose target only running Ruby knows does not
        // name one either.
        harness.write(
            "app/controllers/reports_controller.rb",
            "class ReportsController < ApplicationController\n  layout false\n\n  def show\n    \
             @title = 1.5\n    render options\n  end\nend\n",
        );
        let application_mailer = harness.write(
            "app/mailers/application_mailer.rb",
            "class ApplicationMailer\n  layout \"mailer\"\nend\n",
        );
        harness.write(
            "app/mailers/user_mailer.rb",
            "class UserMailer < ApplicationMailer\n  def welcome\n    @title = Notice.new\n  \
             end\nend\n",
        );
        harness.write("app/views/user_mailer/welcome.html.erb", "x\n");
        let source = "<% held = @title %>\n";
        let application = harness.write("app/views/layouts/application.html.erb", source);
        let admin = harness.write("app/views/layouts/admin.html.erb", source);
        let mailer = harness.write("app/views/layouts/mailer.html.erb", source);
        // A layout nothing is rendered in has no object to read.
        let unused = harness.write("app/views/layouts/print.html.erb", source);
        harness.index();

        for (layout, class, write) in [
            (&application, "Page", "stories_controller.rb:3:4"),
            (&admin, "User", "users_controller.rb:4:6"),
            (&mailer, "Notice", "user_mailer.rb:3:4"),
        ] {
            assert_eq!(
                drawn_hints(source, &harness.hints_in(layout)),
                format!("<% held: {class}? = @title %>")
            );
            assert_eq!(jumps(&mut harness, layout, source, "@title"), [write]);
        }
        assert_eq!(drawn_hints(source, &harness.hints_in(&unused)), "null");
        assert!(jumps(&mut harness, &unused, source, "@title").is_empty());
        // A `layout` is read, never declared.
        assert_eq!(harness.generated_for(&application_mailer), None);
    }

    #[test]
    fn a_mailer_s_default_template_path_moves_where_its_views_are_read_from() {
        // An `ApplicationMailer` may move every mailer's views under `mailers/`, so
        // `mailers/notify_mailer/` is `NotifyMailer`'s and `notify_mailer/` is nobody's. A mailer
        // that writes its own moves its views again, and the nearest setting is the one Rails
        // reads. The mailer layout then has a mailer rendering in it.
        let mut harness = Harness::new();
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        harness.write("app/models/comment.rb", "class Comment\nend\n");
        harness.write("app/models/summary.rb", "class Summary\nend\n");
        // The chain must reach the framework's class, as a bundle's does: a mailer whose ancestors
        // cannot be read has no nearest setting.
        harness.write(
            "lib/action_mailer/base.rb",
            "module ActionMailer\n  class Base\n  end\nend\n",
        );
        harness.write(
            "app/mailers/application_mailer.rb",
            "class ApplicationMailer < ActionMailer::Base\n  layout \"mailer\"\n  default(\n    \
             template_path: ->(mailer) { \"mailers/#{mailer.class.name.underscore}\" },\n  \
             )\nend\n",
        );
        harness.write(
            "app/mailers/notify_mailer.rb",
            "class NotifyMailer < ApplicationMailer\n  def new_reply_email\n    \
             @comment = Comment.new\n  end\nend\n",
        );
        harness.write(
            "app/mailers/digest_mailer.rb",
            "class DigestMailer < ApplicationMailer\n  default template_path: \"digests\"\n\n  \
             def daily\n    @comment = Summary.new\n  end\nend\n",
        );
        let source = "<% held = @comment %>\n";
        let moved = harness.write(
            "app/views/mailers/notify_mailer/new_reply_email.html.erb",
            source,
        );
        let default = harness.write("app/views/notify_mailer/new_reply_email.html.erb", source);
        let fixed = harness.write("app/views/digests/daily.html.erb", source);
        let bypassed = harness.write("app/views/mailers/digest_mailer/daily.html.erb", source);
        let layout = harness.write("app/views/layouts/mailer.html.erb", source);
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&moved)),
            "<% held: Comment? = @comment %>"
        );
        assert_eq!(
            jumps(&mut harness, &moved, source, "@comment"),
            ["notify_mailer.rb:3:4"]
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&fixed)),
            "<% held: Summary? = @comment %>"
        );
        assert_eq!(
            jumps(&mut harness, &fixed, source, "@comment"),
            ["digest_mailer.rb:5:4"]
        );
        for nobody in [&default, &bypassed] {
            assert_eq!(drawn_hints(source, &harness.hints_in(nobody)), "null");
        }
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&layout)),
            "<% held: Summary? | Comment = @comment %>"
        );
    }

    #[test]
    fn a_partial_jumps_to_every_write_its_card_folds() {
        // `shared/_header.html.erb` names a `SharedController` no file declares, and a
        // partial's path names no class anyway: its variables are every class's a view is
        // rendered by, which the card already folds. The jump lists each of those writes rather
        // than picking one, and a class with no view of its own is not one of them.
        let mut harness = Harness::new();
        rails_app(&harness);
        harness.write(
            "app/controllers/feeds_controller.rb",
            "class FeedsController\n  def index\n    @story = Story.new\n  end\nend\n",
        );
        harness.write(
            "app/views/feeds/index.html.erb",
            "<%= render \"shared/header\" %>\n",
        );
        harness.write(
            "app/controllers/api_controller.rb",
            "class ApiController\n  def show\n    @story = Story.new\n  end\nend\n",
        );
        let source = "<%= @story.title %>\n";
        let view = harness.write("app/views/shared/_header.html.erb", source);
        harness.index();

        assert_eq!(
            jumps(&mut harness, &view, source, "@story"),
            ["feeds_controller.rb:3:4", "stories_controller.rb:3:4"]
        );
        let markdown = card(&mut harness, &view, source, "title");
        assert!(markdown.contains("Story#title"), "{markdown}");
    }

    #[test]
    fn a_partial_whose_directory_names_no_class_reads_the_mailer_whose_view_renders_it() {
        // A digest partial: `user_notifications/digest/_stats.html.erb` spells a
        // class nobody declares, and the mailer's own `digest` view renders the partial. The card
        // and the jump read the mailer's writes, as they would for the view itself. Where two
        // unrelated renderers write a variable nothing types, the card has no one class to name.
        let mut harness = Harness::new();
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        harness.write(
            "app/mailers/application_mailer.rb",
            "class ApplicationMailer\nend\n",
        );
        harness.write(
            "app/mailers/user_notifications.rb",
            "class UserNotifications < ApplicationMailer\n  def digest\n    @counts = Count.new\n    \
             @unread = unknown\n  end\nend\n",
        );
        harness.write(
            "app/mailers/other_mailer.rb",
            "class OtherMailer < ApplicationMailer\n  def notice\n    @unread = unknown\n  \
             end\nend\n",
        );
        harness.write("app/views/other_mailer/notice.html.erb", "x\n");
        harness.write("app/models/count.rb", "class Count\nend\n");
        harness.write(
            "app/views/user_notifications/digest.html.erb",
            "<%= render partial: \"user_notifications/digest/stats\" %>\n",
        );
        let source = "<% held = @counts %>\n<% @unread %>\n";
        let partial = harness.write(
            "app/views/user_notifications/digest/_stats.html.erb",
            source,
        );
        harness.index();

        assert_eq!(
            jumps(&mut harness, &partial, source, "@counts"),
            ["user_notifications.rb:3:4"]
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&partial)),
            "<% held: Count? = @counts %>"
        );
        assert_eq!(
            jumps(&mut harness, &partial, source, "@unread"),
            ["other_mailer.rb:3:4", "user_notifications.rb:4:4"]
        );
        let unread = harness.hover_at(&partial, source, "@unread");
        assert!(unread.is_null(), "{unread}");
    }

    #[test]
    fn a_name_the_controller_never_assigns_answers_nothing() {
        // The convention found the controller, but it never writes this variable. An empty answer,
        // not the class: the reader did not ask for the class.
        let mut harness = Harness::new();
        let view = rails_app(&harness);
        let source = "<%= @missing %>\n";
        harness.write("app/views/stories/show.html.erb", source);
        harness.index();

        let found = harness.definition_at(&view, source, "@missing");
        assert!(found.is_null(), "{found}");
    }

    #[test]
    fn a_mailers_template_is_typed_by_the_mailer_its_path_names() {
        // A mailer's views hang off the mailer's own name. `ActionMailer::Base` derives its view
        // path from its class, so `app/views/user_mailer/welcome.html.erb` is `UserMailer`, and
        // there is no `UserMailerController`. In one corpus every `.erb` file is a mailer view.
        let mut harness = Harness::new();
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        harness.write("app/models/story.rb", STORY);
        harness.write(
            "app/mailers/application_mailer.rb",
            "class ApplicationMailer\nend\n",
        );
        harness.write(
            "app/mailers/user_mailer.rb",
            "class UserMailer < ApplicationMailer\n  def welcome\n    @story = Story.new\n  \
             end\nend\n",
        );
        let source = "<%= @story.title %>\n";
        let view = harness.write("app/views/user_mailer/welcome.html.erb", source);
        harness.index();

        // The card names the class **and the convention**: `UserMailer` is not a controller, and a
        // footnote is read as a claim about its one class.
        let markdown = card(&mut harness, &view, source, "title");
        assert!(markdown.contains("Story#title"), "{markdown}");
        assert!(!markdown.contains(GUESS_FOOTNOTE), "{markdown}");

        // The jump goes through the same function, so the two cannot name different classes.
        assert_eq!(
            jumps(&mut harness, &view, source, "@story"),
            ["user_mailer.rb:3:4"]
        );

        // **A controller of that name wins**, which is Rails' order: an application that writes a
        // `UserMailerController` has said where this template renders from. Nothing here overrules
        // the framework.
        let controller = harness.write(
            "app/controllers/user_mailer_controller.rb",
            "class UserMailerController\n  def welcome\n    @story = Story.new\n  end\nend\n",
        );
        harness.watch(&[&controller]);

        let overruled = card(&mut harness, &view, source, "title");
        assert!(overruled.contains("Story#title"), "{overruled}");
        assert!(!overruled.contains(GUESS_FOOTNOTE), "{overruled}");
        assert_eq!(
            jumps(&mut harness, &view, source, "@story"),
            ["user_mailer_controller.rb:3:4"]
        );
    }

    #[test]
    fn a_controllers_own_instance_variable_never_names_a_line_in_the_template() {
        // **The renderer rung reads a second document, so it can hand the card an offset into a
        // file the card does not have.** `@current = @story` makes the receiver a nested
        // `Receiver::Assigned`, which carries its assignment's offset in the *controller*. `hover`
        // and `hints` draw that offset as a line of the cursor's text (the template), so the
        // footnote would name a line of markup.
        //
        // Every other provenance field that points into another file carries the file and a line
        // for this reason. `Receiver::Assigned` carries an offset because every other rung stays in
        // one document.
        let mut harness = Harness::new();
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        harness.write("app/models/story.rb", STORY);
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def show\n    @story = Story.new\n    @current = \
             @story\n  end\nend\n",
        );
        // Long enough that the controller's offsets are real lines here, so the wrong footnote
        // would look plausible, not absurd.
        let source = "<h1>Story</h1>\n<p>one</p>\n<p>two</p>\n<%= @current.title %>\n";
        let view = harness.write("app/views/stories/show.html.erb", source);
        harness.index();

        let markdown = card(&mut harness, &view, source, "title");
        assert!(markdown.contains("Story#title"), "{markdown}");
        assert!(!markdown.contains(GUESS_FOOTNOTE), "{markdown}");
    }

    /// A class whose own file never assigns `@subject`, above a class that does.
    ///
    /// `@subject`, not `@story`, in every test below: the guess rung camelizes the variable's name,
    /// so `@story` would answer `Story` whether or not the ancestor was read. No file declares
    /// `Subject`, so an answer here came from the chain.
    #[test]
    fn a_body_nobody_wrote_answers_nil_and_one_that_is_only_an_ensure_answers_nothing() {
        // A `def` with no body returns `nil`, as `if x then end` does, and `NilClass` alone is
        // drawn `nil`.
        let (mut harness, _) = with_types(
            "class Story\n  def blank\n  end\n\n  def guarded\n  ensure\n    cleanup\n  end\nend\n",
        );
        let probe = "class Probe\n  def a\n    it.blank\n  end\n\n  def b\n    it.guarded\n  end\n\n                       def it\n    Story.new\n  end\nend\n";
        let uri = harness.write("lib/probe.rb", probe);
        harness.index();

        let empty = card(&mut harness, &uri, probe, "blank");
        assert!(empty.contains("Story#blank -> nil"), "{empty}");
        // An `ensure` runs for its effect and never decides the return, so a body that is *only* an
        // `ensure` has no readable exit. That is the empty list `body_return`'s guard is for
        // (*read, and none found*, not *never read*), and it declines the method.
        let ensured = card(&mut harness, &uri, probe, "guarded");
        assert!(!ensured.contains("->"), "{ensured}");
    }

    /// A parameter typed six ways, and three cases that must stay silent.
    ///
    /// - **The `sig` and the `@param` are one rung, spelled two ways.** `knowledge::annotations`
    ///   reads both and renders RBS, so `types.rs` sees one thing.
    /// - **One `Story` method per case**, so hovering the *call* names the case: a card reading
    ///   `Story#declared_call` proves the receiver beneath it resolved.
    /// - **No parameter is called `story`, on purpose.** The name guess two rungs below would
    ///   answer `Story` for it, so the fixture would pass whether this rung works or not. `held`
    ///   camelizes onto nothing, so every answer below is this rung's or absent.
    const PARAMETERS: &str = "\
class Story
  def declared_call
    \"x\"
  end

  def tagged_call
    \"x\"
  end

  def positioned_call
    \"x\"
  end

  def keyworded_call
    \"x\"
  end

  def defaulted_call
    \"x\"
  end

  def optional_call
    \"x\"
  end

  def shadowed_call
    \"x\"
  end

  def singleton_call
    \"x\"
  end

  def bare_call
    \"x\"
  end
end

class Shelf
  sig { params(held: Story).returns(String) }
  def declared(held)
    held.declared_call
  end

  # @param held [Story] the one to read
  def tagged(held)
    held.tagged_call
  end

  sig { params(first: Integer, second: Story).returns(String) }
  def positioned(first, second)
    second.positioned_call
  end

  sig { params(held: Story, mode: Integer).returns(String) }
  def keyworded(held, mode:)
    held.keyworded_call
  end

  def defaulted(held = Story.new)
    held.defaulted_call
  end

  def optional(held = nil)
    held.optional_call
  end

  sig { params(held: Story).returns(String) }
  def shadowed(held)
    [1].each do |held|
      held.shadowed_call
    end
  end

  def bare(held)
    held.bare_call
  end

  sig { params(held: Story).returns(String) }
  def self.on_the_class(held)
    held.singleton_call
  end

  # @param held [Object] anything at all
  def opaque(held)
    held.bare_call
  end

  # @param held [Class] a class object
  def classy(held)
    held.bare_call
  end

  sig { params(held: Story).returns(String) }
  def partial(held, extra, missing:)
    extra.bare_call
  end

  sig { params(held: Story).returns(String) }
  def unkeyed(held, missing:)
    missing.bare_call
  end

  sig { params(held: Nowhere).returns(String) }
  def unresolvable(held)
    held.bare_call
  end
end
";

    #[test]
    fn a_parameter_of_a_def_inside_an_anonymous_class_is_declined_rather_than_looked_up_elsewhere()
    {
        const ANONYMOUS: &str = "\
class Story
  def anon_call
    1
  end
end

Holder = Class.new do
  # @param held [Story] a story
  def inside(held)
    held.anon_call
  end
end
";
        let (mut harness, uri) = with_types(ANONYMOUS);
        // rubydex records a `Class.new do … end` body as an **anonymous** class, so there is no
        // name to look `inside()` up by and no tag to read. The arm declines, the only honest
        // answer: falling back to some other `inside` would read a tag written above a different
        // method.
        //
        // The shape `Receiver::SelfObject` documents, from the other side, and why this arm
        // resolves the `def`'s own offset, not the cursor's.
        let settled = card(&mut harness, &uri, ANONYMOUS, "anon_call\n  end\nend");
        assert!(settled.contains("Guessed from name alone"), "{settled}");
    }

    #[test]
    fn a_parameter_declared_self_is_the_class_the_def_is_written_in() {
        const SELF_TYPED: &str = "\
class Probe
  def same_call
    1
  end

  def same(held)
    held.same_call
  end

  def widened(held)
    held.same_call
  end
end
";
        let (mut harness, uri) = with_types(SELF_TYPED);
        // **A hand-written `sig/` is a third spelling of the same rung**, and it can say what a
        // Sorbet `sig` and a YARD tag cannot: RBS has `self`, and neither of the others has a word
        // for it.
        harness.write(
            "sig/app.rbs",
            "class Probe\n  def same: (self) -> void\n  def widened: (Object) -> void\nend\n",
        );
        harness.index();

        // `self` in a parameter's type means *the class this `def` is written in*, resolved at
        // lookup for `Return::Same`'s reason, from the argument's end.
        let same = card(
            &mut harness,
            &uri,
            SELF_TYPED,
            "same_call\n  end\n\n  def widened",
        );
        assert!(same.contains("Probe#same_call"), "{same}");
        assert!(same.contains("Probe#same"), "{same}");

        // The refusal every spelling must keep: `Object` names every object, so it must not take
        // the question away from the rungs below.
        let widened = card(&mut harness, &uri, SELF_TYPED, "same_call\n  end\nend");
        assert!(!widened.contains("Probe#widened"), "{widened}");
    }

    #[test]
    fn a_method_parameter_is_typed_by_what_its_def_declares_and_never_by_its_default() {
        let (mut harness, uri) = with_types(PARAMETERS);
        let mut answer = |needle: &str| card(&mut harness, &uri, PARAMETERS, needle);
        let guessed = "Guessed from name alone";

        // **The rung itself.** `held` is bound by nothing this module collects (a parameter is not
        // a write), so without this it would reach `Receiver::Named`, camelize onto no class, and
        // answer nothing. A `sig` says what it is.
        let declared = answer("declared_call\n  end\n\n  # @param");
        assert!(declared.contains("Story#declared_call"), "{declared}");
        assert!(!declared.contains(guessed), "{declared}");
        assert!(declared.contains("-> String"), "{declared}");

        // **The other spelling of the same rung, indistinguishable to `types.rs`.** Both arrive as
        // generated RBS through `Types::harvest`: `types.md`'s "generated RBS feeds the same table
        // by the same route".
        let tagged = answer("tagged_call\n  end");
        assert!(tagged.contains("Story#tagged_call"), "{tagged}");

        // **A positional is matched by position, not name.** `second` is at index one; reading
        // index zero would answer `Integer`.
        let positioned = answer("positioned_call\n  end");
        assert!(positioned.contains("Story#positioned_call"), "{positioned}");

        // **A keyword is matched by name.** `held` is positional and `mode:` a keyword; counting
        // the keyword into the positions would answer `held` with `Integer`. That is why
        // `ParameterSlot` has two spellings.
        let keyworded = answer("keyworded_call\n  end");
        assert!(keyworded.contains("Story#keyworded_call"), "{keyworded}");

        // **A default is not a type.** `Story.new` says what `held` holds when nobody passed
        // anything, and a caller may pass anything. Nothing declares it, so the receiver is
        // unknown and only the name-based list answers, as for `= nil` below.
        let defaulted = answer("defaulted_call\n  end");
        assert!(defaulted.contains("Guessed from name alone"), "{defaulted}");

        // **`= nil` says nothing either.** Asserted on **this rung's fingerprint** (the footnote
        // naming the `def`), not on the absence of an answer: `optional_call` is unique in the
        // graph, so the name-based list still matches it. What must not happen is this arm
        // claiming a receiver.
        let optional = answer("optional_call\n  end");
        assert!(!optional.contains("Shelf#optional"), "{optional}");
        assert!(!optional.contains("NilClass"), "{optional}");
        assert!(optional.contains("Guessed from name alone"), "{optional}");

        // **A block parameter of the same name shadows the method's**, by being asked first, as
        // Ruby shadows. The block's `held` is an `Integer` element, so the `sig` two lines up must
        // not answer for it.
        let shadowed = answer("shadowed_call\n    end");
        assert!(!shadowed.contains("Shelf#shadowed"), "{shadowed}");

        // **A `def self.` is looked up on the singleton.** This arm resolves the `def`'s own
        // offset, where `scope_at(...).caller` answers the class for an instance method and
        // `Shelf::<Shelf>` here: the value `Receiver::SelfObject` resolves to, and the split
        // `from_super` depends on. Using the instance side for both would find neither.
        let singleton = answer("singleton_call\n  end");
        assert!(singleton.contains("Story#singleton_call"), "{singleton}");
        assert!(singleton.contains("-> String"), "{singleton}");

        // **A declared type that names every object is no type.** `[Object]`, `[Class]`, `[Module]`
        // and `[BasicObject]` are true of everything and offer nothing, and answering with one
        // takes the question from the rungs below. The module's `Namespace::Todo` refusal, applied
        // to a parameter.
        let opaque = answer("bare_call\n  end\n\n  # @param held [Class]");
        assert!(!opaque.contains("Shelf#opaque"), "{opaque}");
        let classy = answer("bare_call\n  end\nend");
        assert!(!classy.contains("Shelf#classy"), "{classy}");

        // **A signature that says less than the `def` takes answers only what it said.** A
        // positional past the declared list is `None`, never the last one. A keyword the signature
        // never mentions is `None`, never a position: Ruby binds them differently.
        let partial = answer(
            "bare_call\n  end\n\n  sig { params(held: Story).returns(String) }\n  def unkeyed",
        );
        assert!(!partial.contains("Shelf#partial"), "{partial}");
        let unkeyed = answer("bare_call\n  end\nend");
        assert!(!unkeyed.contains("Shelf#unkeyed"), "{unkeyed}");

        // **A signature naming an undeclared class answers nothing**, which keeps the rung purely
        // additive: the lookup decides, not the name, so a `sig` written against an unbundled gem
        // costs the rungs below nothing.
        let unresolvable = answer("bare_call\n  end\nend");
        assert!(
            !unresolvable.contains("Shelf#unresolvable"),
            "{unresolvable}"
        );

        // **A parameter nothing describes stays where it was.** No `sig`, no tag, no default, so
        // the rung declines and nothing below has anything to add.
        let bare = answer("bare_call\n  end");
        assert!(!bare.contains("Shelf#bare"), "{bare}");
        assert!(bare.contains("Guessed from name alone"), "{bare}");
    }

    /// One `def` per row of the truth table a shortcut is evaluated against, plus the five
    /// methods whose returns the rows are written in terms of.
    ///
    /// Bodies, not signatures, because that is where the shape occurs: nothing declares an
    /// application's predicates, and a `&&` between two of them is the tail this rung is for.
    const SHORTCUTS: &str = "\
class Story
  def text
    \"x\"
  end

  def count
    1
  end

  def missing
    nil
  end

  def no
    false
  end

  def maybe
    text if count
  end

  def predicate
    missing.nil?
  end

  def truthy_and
    text && count
  end

  def truthy_or
    text || count
  end

  def falsy_and
    missing && count
  end

  def falsy_or
    missing || count
  end

  def either_and
    maybe && count
  end

  def either_or
    maybe || text
  end

  def defaulted
    maybe || no
  end

  def both_predicates
    predicate && predicate
  end

  def unreadable
    mystery && count
  end
end
";

    #[test]
    fn a_shortcut_is_executed_against_the_classes_because_only_nil_and_false_are_falsy() {
        let names = [
            "truthy_and",
            "truthy_or",
            "falsy_and",
            "falsy_or",
            "either_and",
            "either_or",
            "defaulted",
            "both_predicates",
            "unreadable",
        ];
        let probe = format!(
            "class Probe\n{}end\n",
            names
                .iter()
                .enumerate()
                .map(|(nth, name)| format!("  def p{nth}\n    Story.new.{name}\n  end\n\n"))
                .collect::<String>()
        );
        let (mut harness, _) = with_types(SHORTCUTS);
        let uri = harness.write("lib/probe.rb", &probe);
        harness.index();
        let mut answer = |name: &str| card(&mut harness, &uri, &probe, name);

        // **The left operand decides the operator, and its class decides the left operand.**
        // `"a" && 1` is `1`: `&&` returns its *right* side when the left is truthy, and a `String`
        // is truthy because it is neither of the two falsy classes.
        assert!(
            answer("truthy_and").contains("Story#truthy_and -> Integer"),
            "{}",
            answer("truthy_and")
        );
        // The same rule with the sides swapped, and the right operand is never resolved at all.
        assert!(
            answer("truthy_or").contains("Story#truthy_or -> String"),
            "{}",
            answer("truthy_or")
        );
        // A left operand that can only be falsy takes the other branch of each, exactly.
        assert!(
            answer("falsy_and").contains("Story#falsy_and -> nil"),
            "{}",
            answer("falsy_and")
        );
        assert!(
            answer("falsy_or").contains("Story#falsy_or -> Integer"),
            "{}",
            answer("falsy_or")
        );
        // **Only a left operand that can be either produces a union**: the taken branch plus the
        // half of the left that reaches the end. `String? && Integer` is an `Integer` where the
        // `String` was, and `nil` where it was not.
        assert!(
            answer("either_and").contains("Story#either_and -> Integer?"),
            "{}",
            answer("either_and")
        );
        // The mark comes **off** with the other operator: `String? || String` cannot return `nil`,
        // because the `||` is exactly what runs when it would have.
        let settled = answer("either_or");
        assert!(settled.contains("Story#either_or -> String"), "{settled}");
        assert!(!settled.contains("String?"), "{settled}");
        // A value or a `false`, spelled as the two it is.
        assert!(
            answer("defaulted").contains("Story#defaulted -> String | false"),
            "{}",
            answer("defaulted")
        );
        // Two predicates joined by `&&` are a predicate, and fold like every `bool` here: `true`
        // and `false` together are what RBS calls one. This is how a policy ends.
        assert!(
            answer("both_predicates").contains("Story#both_predicates -> bool"),
            "{}",
            answer("both_predicates")
        );
        // **A left operand with no type declines the pair.** The operator cannot run without one,
        // and declining is what this module does instead of picking a side.
        let declined = answer("unreadable");
        assert!(!declined.contains("->"), "{declined}");
    }

    /// A base class, three classes for its methods to return, a subclass written as `story`, and
    /// one file of calls into it: the document every `super` test hovers in.
    ///
    /// Three files, so each needle is unique: `title` is a `def` in two of them and a call in the
    /// third, and a hover fixture points at the first spelling it finds.
    fn super_app(harness: &Harness, story: &str) -> DocUri {
        harness.write(
            "app/models/base_story.rb",
            "\
class Label
end

class Built
end

class Near
end

class BaseStory
  def title
    Label.new
  end

  def self.build
    Built.new
  end
end
",
        );
        harness.write("app/models/story.rb", story);
        harness.write("app/models/probe.rb", SUPER_PROBE)
    }

    /// One call to each of the methods [`super_app`]'s subclass overrides.
    const SUPER_PROBE: &str = "\
class Probe
  def a
    Story.new.title
  end

  def b
    Story.build
  end

  def c
    Story.new.nowhere
  end
end
";

    /// Two answers a body can give, and an importer whose private step a subclass overrides.
    const IMPORTERS: &str = "\
class Parsed
end

class CsvRow
end

class Importer
  def run
    parse
  end

  private

  def parse
    Parsed.new
  end
end

class CsvImporter < Importer
  private

  def parse
    CsvRow.new
  end
end

class StrictCsvImporter < CsvImporter
end
";

    #[test]
    fn a_self_call_in_an_inherited_body_is_looked_up_from_the_receiver_s_class() {
        // Ruby looks `parse` up from the object's class, so `run`, written in `Importer`, reaches
        // the override on a `CsvImporter`. Read where the code is written, the call would be
        // `Importer#parse` and the label `Parsed`, which a `CsvImporter` never returns (#22).
        let mut harness = Harness::new();
        harness.write("app/models/importers.rb", IMPORTERS);
        let probe = "\
class Probe
  def a
    CsvImporter.new.run
  end

  def b
    Importer.new.run
  end

  def c
    StrictCsvImporter.new.run
  end
end
";
        let uri = harness.write("app/models/probe.rb", probe);
        harness.index();

        // `b` is the class the body is written in, which keeps its own step. `c` is below the
        // override, which is found on the way up.
        assert_eq!(
            drawn_hints(probe, &harness.hints_in(&uri)),
            "  def a -> CsvRow\n  def b -> Parsed\n  def c -> CsvRow"
        );
    }

    #[test]
    fn a_recursion_is_refused_at_once_rather_than_unrolled() {
        // `a` and `b` call each other and `c` calls itself: legal Ruby that never returns, so none
        // has a label. What this pins is the work: a read stops the first time a body is asked for
        // inside itself. The label reads `a` for no object, and the self-calls inside read for a
        // `Loop`, which is another question, so `a` reads `a`, `b`, `a`, then meets `b` again; `b`
        // reads three the same way and `c` two. Eight, where unrolling each loop to `BODY_HOPS`
        // read that many per `def`.
        let mut harness = Harness::new();
        let probe = "\
class Loop
  def a
    b
  end

  def b
    a
  end

  def c
    c
  end
end
";
        let uri = harness.write("app/models/loop.rb", probe);
        harness.index();
        bodies_read();
        assert_eq!(harness.hints_in(&uri), serde_json::Value::Null);
        assert_eq!(bodies_read(), 8);
    }

    #[test]
    fn a_loop_through_an_instance_variable_is_settled_by_its_reads() {
        // `filters` comes back to itself through `@filters`, a variable read the reads settle in
        // rounds: the first round skips the write still being answered, the second reads it with
        // the first round's answer. The body check stays out of the way there. Refusing the inner
        // `filters` would make `filters.take(3)` a write nothing types, which refuses `@filters`
        // and both labels with it.
        let probe = "\
class Search
  def filters
    @filters ||= []
  end

  def narrow
    @filters = filters.take(3)
  end
end
";
        let (mut harness, uri) = with_types(probe);
        harness.write(
            "sig/take.rbs",
            "class Array[E]\n  def take: (Integer) -> Array[E]\nend\n",
        );
        harness.index();
        assert_eq!(
            drawn_hints(probe, &harness.hints_in(&uri)),
            "  def filters -> Array\n  def narrow -> Array"
        );
    }

    #[test]
    fn a_chain_of_distinct_bodies_is_read_to_the_bound() {
        // `m0` calls `m1`, and so on to a `String`: no recursion, just depth. One short of the
        // bound is read to the end; past it the first link answers nothing. Reading that deep on a
        // test thread's stack is also what shows the bound fits in one.
        fn chain(links: usize) -> String {
            let mut source = "class Chain\n".to_owned();
            for at in 0..links {
                source.push_str(&format!("  def m{at}\n    m{}\n  end\n\n", at + 1));
            }
            source.push_str(&format!("  def m{links}\n    \"x\"\n  end\nend\n"));
            source
        }
        let first = |links: usize| {
            let probe = chain(links);
            let (mut harness, uri) = with_types(&probe);
            drawn_hints(&probe, &harness.hints_in(&uri))
                .lines()
                .next()
                .unwrap_or("")
                .to_owned()
        };
        let hops = usize::from(BODY_HOPS);
        // On a thread with the analysis thread's stack: a test thread's default is smaller than
        // the server ever runs on.
        let (fits, past) = std::thread::Builder::new()
            .stack_size(super::super::ANALYSIS_STACK)
            .spawn(move || (first(hops - 1), first(hops)))
            .expect("thread")
            .join()
            .expect("no overflow");
        assert_eq!(fits, "  def m0 -> String");
        assert_eq!(past, "  def m1 -> String");
    }

    #[test]
    fn a_class_method_s_self_call_is_looked_up_from_the_class_it_was_called_on() {
        // The service-object entry point: `self.call` builds `new` and calls `call` on it. Read on
        // the base, `new` is the base and its `call` raises, so nothing is known; read on the
        // action, both are the action's.
        let mut harness = Harness::new();
        harness.write(
            "app/services/actions.rb",
            "\
class Done
end

class ActionBase
  def self.call
    new.call
  end

  def call
    raise NotImplementedError
  end
end

class FinishAction < ActionBase
  def call
    Done.new
  end
end
",
        );
        let probe = "class Probe\n  def a\n    FinishAction.call\n  end\nend\n";
        let uri = harness.write("app/models/probe.rb", probe);
        harness.index();

        assert_eq!(
            drawn_hints(probe, &harness.hints_in(&uri)),
            "  def a -> Done"
        );
    }

    #[test]
    fn a_module_s_body_read_for_an_includer_calls_the_includer_s_methods() {
        // `self` in a module's method is whichever object includes it. Read for a `Report`, the
        // step the module leaves to its includer is `Report#parse`; read on the module alone there
        // is no `parse` at all.
        let mut harness = Harness::new();
        harness.write(
            "app/models/report.rb",
            "\
class Parsed
end

module Runs
  def run
    parse
  end
end

class Report
  include Runs

  def parse
    Parsed.new
  end
end
",
        );
        let probe = "class Probe\n  def a\n    Report.new.run\n  end\nend\n";
        let uri = harness.write("app/models/probe.rb", probe);
        harness.index();

        assert_eq!(
            drawn_hints(probe, &harness.hints_in(&uri)),
            "  def a -> Parsed"
        );
    }

    #[test]
    fn a_self_written_in_another_class_s_body_is_that_class_s() {
        // `DEFAULT = make` runs in `Factory`'s body, where `self` is `Factory`, whatever object's
        // method later reads the constant. Taking the reader's object there would look `make` up on
        // `Sub`, which has none.
        let mut harness = Harness::new();
        harness.write(
            "app/models/factory.rb",
            "\
class Made
end

class Factory
  def self.make
    Made.new
  end

  DEFAULT = make
end

class Base
  def run
    Factory::DEFAULT
  end
end

class Sub < Base
end
",
        );
        let probe = "class Probe\n  def a\n    Sub.new.run\n  end\nend\n";
        let uri = harness.write("app/models/probe.rb", probe);
        harness.index();

        assert_eq!(
            drawn_hints(probe, &harness.hints_in(&uri)),
            "  def a -> Made"
        );
    }

    /// [`with_types`], plus `Kernel#class` as core RBS declares it, the classes a `bool` needs, and a
    /// method typed as a module (which includes `Kernel`, or it would have no `class` to find). Each
    /// carries a singleton method, so a guard is what keeps its singleton class from being the
    /// answer.
    fn with_class_signature(source: &str) -> (Harness, DocUri) {
        with_rbs(
            source,
            "module Kernel\n  def class: () -> Class\nend\n\n\
             class Class\nend\n\n\
             class TrueClass\n  def self.allocate: () -> TrueClass\nend\n\n\
             class FalseClass\n  def self.allocate: () -> FalseClass\nend\n\n\
             module Named\n  include Kernel\n  def self.helper: () -> String\nend\n\n\
             class Picker\n  def self.pick: () -> Named\nend\n",
        )
    }

    /// A workspace whose signatures are [`TYPED_RBS`] plus `rbs`, holding one Ruby file.
    fn with_rbs(source: &str, rbs: &str) -> (Harness, DocUri) {
        let mut harness = signed(&[("core/core.rbs", &format!("{TYPED_RBS}\n{rbs}"))], "");
        let uri = harness.write("app/models/widget.rb", source);
        harness.index();
        harness.index_gems();
        (harness, uri)
    }

    /// A call returning a tuple returns an `Array`, so `Array(x)`, whose three arms are `[]`,
    /// `Array[T]` and `[T]`, is one whatever `x` is. A destructure still reads the positions, and a
    /// block's tuple parameter is still the value Ruby unpacks, never one `Array`.
    #[test]
    fn a_tuple_return_is_an_array() {
        let mut harness = signed(
            &[(
                "core/core.rbs",
                &format!(
                    "{TYPED_RBS}\nmodule Kernel\n  \
                     def self?.Array: (nil) -> [] | [T] (Array[T]) -> Array[T] | [T] (T) -> [T]\n\
                     end\n\nclass Pipe\n  def self.open: () -> [Integer, String]\n  \
                     def self.maybe: () -> [Integer, String]?\n  \
                     def self.pairs: () {{ ([String, Integer]) -> void }} -> void\nend\n"
                ),
            )],
            "",
        );
        let source = "\
def listed(x)
  Array(x)
end
ends = Pipe.open
maybe = Pipe.maybe
number, name = Pipe.open
Pipe.pairs { |pair| pair }
Pipe.pairs { |key, count| key }
";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "\
def listed(x) -> Array
ends: Array = Pipe.open
maybe: Array? = Pipe.maybe
number: Integer, name = Pipe.open
number, name: String = Pipe.open"
        );
    }

    /// A method's own `[X]` that one argument binds whole is that argument's type, where the
    /// argument has one: `ENV.fetch("PORT", 3000)` is `String | Integer`.
    ///
    /// Nothing where the argument is untyped or only guessed, a variable two parameters share
    /// (`clamp`), or one written inside another type (`Array[X]`). A splat binds nothing: its
    /// `listed` is an `Array`, holding what no position says, and `pad(*args, 1)` may hand `1` to
    /// the optional `width`, not to `X`.
    #[test]
    fn a_methods_own_type_variable_is_the_argument_that_binds_it() {
        let mut harness = signed(
            &[(
                "core/core.rbs",
                &format!(
                    "{TYPED_RBS}\nclass Env\n  \
                     def fetch: (String name) -> String | [X] (String name, X default) -> (String | X)\n  \
                     def clamp: [T] (T low, T high) -> T\n  \
                     def unwrap: [X] (Array[X] list) -> X\n  \
                     def gather: [U] (U memo) {{ (String, U) -> untyped }} -> U\n  \
                     def listed: [U] (U memo) -> Array[U]\n  \
                     def pad: [X] (String name, X value, ?Integer width) -> X\n  \
                     def number: () -> Integer?\nend\n\nclass Widget\nend\n"
                ),
            )],
            "",
        );
        let source = "\
env = Env.new
plain = env.fetch(\"A\")
defaulted = env.fetch(\"A\", \"b\")
numbered = env.fetch(\"A\", 1)
nothing = env.fetch(\"A\", nil)
optional = env.fetch(\"A\", env.number)
gathered = env.gather({}) { |name, memo| memo }
listed = env.listed(1)
guessed = env.fetch(\"A\", widget)
clamped = env.clamp(1, 2)
unwrapped = env.unwrap([1])
args = [\"A\", \"b\"]
splatted = env.listed(*args)
padded = env.pad(*args, 1)
";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "\
plain: String = env.fetch(\"A\")
defaulted: String = env.fetch(\"A\", \"b\")
numbered: String | Integer = env.fetch(\"A\", 1)
nothing: String? = env.fetch(\"A\", nil)
optional: String? | Integer = env.fetch(\"A\", env.number)
gathered: Hash = env.gather({}) { |name, memo| memo }
gathered = env.gather({}) { |name: String, memo| memo }
listed: Array[Integer] = env.listed(1)
splatted: Array = env.listed(*args)"
        );
    }

    /// An arm written as the symbols a call passes answers it, tried in written order, as RBS
    /// tries an overload set.
    ///
    /// - `pick(:b)` skips `(:a)`, which it does not fit, and stops at `(:b)`; two symbols are
    ///   matched position by position.
    /// - A name no arm writes reaches only the catch-all, and a variable or a string could be
    ///   any symbol: nothing.
    /// - `early` takes any `Symbol` first, so that arm runs and the literal one never does:
    ///   nothing rather than the wrong arm.
    /// - `split`'s arms come from two documents, which have no order between them: nothing.
    /// - An `untyped` position takes whatever is written there, a count or a variable, so
    ///   `counted` and `listed` still answer by their symbol (`create_list(:user, 3)`). An
    ///   `Integer` one might not take it: `typed` answers nothing.
    #[test]
    fn an_arm_written_as_the_symbols_a_call_passes_answers_it() {
        let mut harness = signed(
            &[
                (
                    "core/core.rbs",
                    &format!(
                        "{TYPED_RBS}\nclass Picker\n  \
                         def pick: (:a) -> String | (:b) -> Integer | (*untyped) -> untyped\n  \
                         def two: (:a, :b) -> String | (:a, :c) -> Integer | (*untyped) -> untyped\n  \
                         def early: (Symbol) -> Float | (:a) -> String | (*untyped) -> untyped\n  \
                         def quoted: (:\"a\") -> String | (*untyped) -> untyped\n  \
                         def split: (:a) -> String | (*untyped) -> untyped\n  \
                         def counted: (:a, untyped) -> String | (:b, untyped) -> Integer \
                         | (*untyped) -> untyped\n  \
                         def listed: (:a, untyped amount, *untyped) -> String \
                         | (untyped, untyped, *untyped) -> untyped\n  \
                         def typed: (:a, Integer) -> String | (*untyped) -> untyped\nend\n"
                    ),
                ),
                (
                    "core/picker.rbs",
                    "class Picker\n  def split: (:b) -> Integer\nend\n",
                ),
            ],
            "",
        );
        let source = "\
picker = Picker.new
a = picker.pick(:a)
b = picker.pick(:b)
c = picker.pick(:c)
name = :a
held = picker.pick(name)
spelled = picker.pick(\"a\")
ac = picker.two(:a, :c)
ab = picker.two(:a, :b)
early = picker.early(:a)
quoted = picker.quoted(:a)
split = picker.split(:a)
counted = picker.counted(:b, 3)
held_count = picker.counted(:a, name)
listed = picker.listed(:a, 3, :x)
typed = picker.typed(:a, 3)
";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "\
a: String = picker.pick(:a)
b: Integer = picker.pick(:b)
ac: Integer = picker.two(:a, :c)
ab: String = picker.two(:a, :b)
counted: Integer = picker.counted(:b, 3)
held_count: String = picker.counted(:a, name)
listed: String = picker.listed(:a, 3, :x)"
        );
    }

    /// A private method answers a call written on `self`, and nothing on any other receiver, where
    /// Ruby raises. A relation's `load` reached `Kernel#load`, private, and a patch of it in a gem
    /// typed `rel.load` as `bool?`.
    #[test]
    fn a_private_method_answers_only_a_call_on_self() {
        let source = "\
class Vault
  def open
    secret
  end

  def peek
    self.secret
  end

  def pry
    Vault.new.secret
  end

  private

  def secret
    \"x\"
  end
end
";
        let (mut harness, uri) = with_rbs(source, "");
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def open -> String
  def peek -> String
  def secret -> String"
        );
    }

    /// Every call rung reaches its member through one lookup ([`reach`]): a private method hands
    /// a block nothing on a written receiver either, and a `def` written only inside a block is
    /// not a member of every object, as navigation already refused.
    #[test]
    fn a_call_reaches_the_member_navigation_would_jump_to() {
        let source = "\
String.class_eval do
  def self.configure
    \"x\"
  end
end

class Vault
  def open
    secret_each { |item| item }
  end

  def peek
    Vault.new.secret_each { |thing| thing }
  end

  def run
    Vault.configure
  end
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class Vault\n  private\n  def secret_each: () { (String) -> void } -> void\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def self.configure -> String
    secret_each { |item: String| item }"
        );
    }

    /// A signature two Ruby bodies dispute is the union a chain reads, in the margin too
    /// ([`method_return`]): the margin stated the signature where every call was a union.
    #[test]
    fn a_disputed_signature_is_the_same_union_in_the_margin_and_a_chain() {
        let source = "\
class Twice
  def label
    \"x\"
  end
end

class Twice
  def label
    \"y\"
  end

  def use
    label
  end
end
";
        let (mut harness, uri) = with_rbs(source, "class Twice\n  def label: () -> Integer\nend\n");
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def label -> Integer | String
  def label -> Integer | String
  def use -> Integer | String"
        );
    }

    /// `Maker.new` written literally and `new` reached through `self` answer from one rule: a
    /// class's own `self.new` keeps its signature either way.
    #[test]
    fn a_class_s_own_new_keeps_its_signature_however_new_is_spelled() {
        let source = "\
class Maker
  def self.build
    new
  end

  def self.literal
    Maker.new
  end
end

class Plain
  def self.literal
    Plain.new
  end
end

class Kid < Base
  def self.literal
    Kid.new
  end

  def self.build
    new
  end
end
";
        // `Base.new` names `Base`, as stdlib's `Tempfile.new` and `Net::HTTP.new` do: Ruby's own
        // rule spelled out, which a subclass inherits and Ruby answers with the subclass.
        let (mut harness, uri) = with_rbs(
            source,
            "class Maker\n  def self.new: () -> String\nend\n\n\
             class Base\n  def self.new: () -> Base\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def self.build -> String
  def self.literal -> String
  def self.literal -> Plain
  def self.literal -> Kid
  def self.build -> Kid"
        );
    }

    #[test]
    fn x_class_is_the_class_object_of_the_receiver() {
        // `self.class.new` is another object of the receiver's class, and `self.class.kind` is that
        // class's own method. With the signature's `Class`, both stop. On a `Gadget` the class is
        // `Gadget`, since `self` there is the receiver. The class object itself is drawn
        // `Widget:class` (`render::typed`).
        let source = "\
class Kind
end

class Widget
  def self.kind
    Kind.new
  end

  def copy
    self.class.new
  end

  def kind_of_widget
    self.class.kind
  end

  def own_class
    self.class
  end
end

class Gadget < Widget
end

class Probe
  def a
    Gadget.new.copy
  end

  def b
    Gadget.new.class
  end
end
";
        let (mut harness, uri) = with_class_signature(source);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def self.kind -> Kind\n  def copy -> Widget\n  def kind_of_widget -> Kind\n  def own_class -> Widget:class\n  def a -> Gadget\n  def b -> Gadget:class"
        );
    }

    #[test]
    fn x_class_keeps_the_signature_where_it_cannot_name_one_class() {
        // A class object's class is `Class`, which the signature already says; a `bool` is two
        // classes; a module is no object's class. A class that defines its own `class` is asked,
        // not overruled: `Proxy#class` answers `Widget`, whose `kind` is a `Kind`.
        let source = "\
class Kind
end

class Widget
  def self.kind
    Kind.new
  end
end

class Proxy
  def class
    Widget
  end
end

class Probe
  def a
    Widget.class
  end

  def b
    ready?.class
  end

  def c
    Proxy.new.class.kind
  end

  def d
    Picker.pick.class
  end

  def ready?
    if Widget.kind
      true
    else
      false
    end
  end
end
";
        let (mut harness, uri) = with_class_signature(source);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def self.kind -> Kind\n  def class -> Widget:class\n  def a -> Class\n  def b -> Class\n  def c -> Kind\n  def d -> Class\n  def ready? -> bool"
        );
    }

    #[test]
    fn a_call_is_typed_by_its_method_s_body_with_this_call_s_arguments() {
        // `bar` returns what it was passed, so it has no type of its own, and each call
        // has the type of its argument. One request answers both calls: the binding is part of the
        // read's memo key, so `bar(1)`'s answer is not reused for `bar("x")`.
        let source = "\
class Foo
  def self.bar(baz)
    baz
  end
end

class Probe
  def one
    Foo.bar(1)
  end

  def two
    Foo.bar(\"x\")
  end

  def three
    n = Foo.bar(1)
    n
  end

  def four
    Foo.bar(Foo.bar(1))
  end
end
";
        let (mut harness, uri) = with_types(source);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def one -> Integer\n  def two -> String\n  def three -> Integer\n    n: Integer = Foo.bar(1)\n  def four -> Integer"
        );
    }

    #[test]
    fn keywords_bind_by_name_and_a_splat_binds_none() {
        // A keyword binds its parameter by name, whatever the order. `**opts` could pass any
        // keyword, so it binds nothing; and a method with no keyword parameters takes a braceless
        // hash as one more positional, a `Hash` (Ruby 3's rule).
        let source = "\
class Foo
  def self.kw(name:, count: 1)
    name
  end

  def self.mix(a, b: nil)
    b
  end

  def self.plain(a)
    a
  end
end

class Probe
  def one
    Foo.kw(name: \"x\")
  end

  def two
    Foo.mix(1, b: 2)
  end

  def three(opts)
    Foo.kw(**opts)
  end

  def four
    Foo.plain(x: 1)
  end

  def five
    Foo.kw(count: 2, name: 3)
  end
end
";
        let (mut harness, uri) = with_types(source);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def one -> String\n  def two -> Integer\n  def four -> Hash\n  def five -> Integer"
        );
    }

    #[test]
    fn a_left_out_argument_holds_its_default_at_that_call() {
        // At a call that passed nothing, the parameter holds its default, read in the method's own
        // scope with the call's binding (`b = a`). A braceless hash to a method with no keywords
        // fills the next positional, a `Hash` and not left out; after `**h` which keywords were
        // passed is unknown.
        let source = "\
class Foo
  def self.d(a = 10)
    a
  end

  def self.dk(name: \"x\")
    name
  end

  def self.chain(a, b = a)
    b
  end

  def self.hashy(a, b = nil)
    b
  end
end

class Probe
  def one
    Foo.d
  end

  def two
    Foo.dk
  end

  def three
    Foo.chain(1)
  end

  def four
    Foo.hashy(1, x: 2)
  end

  def five(h)
    Foo.dk(**h)
  end

  def six
    Foo.d(\"s\")
  end
end
";
        let (mut harness, uri) = with_types(source);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def one -> Integer\n  def two -> String\n  def three -> Integer\n  def four -> Hash\n  \
             def six -> String"
        );
    }

    #[test]
    fn a_call_binds_nothing_it_cannot_place() {
        // A trailing required parameter moves which argument lands where by the count: `post(1)`
        // gives `b` the 1 and `a` its default, so binding by index would call it an `Integer`. A
        // count Ruby rejects has no binding; an argument typed only by its name binds nothing; and
        // two definitions that disagree about their parameters have no one shape to bind.
        let source = "\
class Foo
  def self.post(a = \"s\", b)
    a
  end

  def self.bar(baz)
    baz
  end

  def self.two(a)
    a
  end
end

class Foo
  def self.two(a, b)
    a
  end
end

class Probe
  def one
    Foo.post(1)
  end

  def few
    Foo.bar
  end

  def many
    Foo.bar(1, 2)
  end

  def guessed
    Foo.bar(user)
  end

  def split
    Foo.two(1)
  end
end
";
        let (mut harness, uri) = with_types(source);
        assert_eq!(drawn_hints(source, &harness.hints_in(&uri)), "null");
    }

    #[test]
    fn a_binding_is_only_ever_the_called_method_s() {
        // `take(1)` reads `@v`, whose one write is `put`'s parameter. That parameter sits at the same
        // position as `take`'s, and is another method's: this call says nothing about it.
        let source = "\
class Box
  def put(v)
    @v = v
  end

  def take(x)
    @v
  end
end

class Probe
  def one
    Box.new.take(1)
  end
end
";
        let (mut harness, uri) = with_types(source);
        assert_eq!(drawn_hints(source, &harness.hints_in(&uri)), "null");
    }

    #[test]
    fn a_call_s_card_shows_what_this_call_returns() {
        // The method's card at its `def` has no type; at a call it has the call's, beside the name
        // the reader hovered.
        let source = "\
class Foo
  def self.bar(baz)
    baz
  end
end

class Probe
  def one
    Foo.bar(1)
  end
end
";
        let (mut harness, uri) = with_types(source);
        let at_call = harness.hover_at(&uri, source, "bar(1)")["contents"]["value"]
            .as_str()
            .unwrap_or("null")
            .to_owned();
        assert!(at_call.contains("Foo.bar(baz) -> Integer"), "{at_call}");
        let at_def = harness.hover_at(&uri, source, "bar(baz)")["contents"]["value"]
            .as_str()
            .unwrap_or("null")
            .to_owned();
        assert!(!at_def.contains("->"), "{at_def}");
    }

    #[test]
    fn an_object_holds_what_new_passed_it() {
        // `@user = user` has no type while `user` has none, so `Service#call` stays
        // unlabelled. The object `Service.new("x")` built holds a `String` there, and a body read for
        // it says so: through a local, a self-call, a left-out argument's default, `new` in the
        // class's own `self.run`, a body answering `self`, and an object passed to another's `new`.
        // One request answers several objects: what built each is part of the read's memo key, and
        // part of the key of any binding it is passed in. The local names its class, so it gets no
        // label of its own.
        let source = "\
class Service
  def initialize(user, limit = 10)
    @user = user
    @limit = limit
  end

  def call
    @user
  end

  def limit
    @limit
  end

  def via_self
    call
  end

  def itself_again
    self
  end

  def self.run(user)
    new(user).call
  end
end

class Wrapper
  def initialize(inner)
    @inner = inner
  end

  def inner_call
    @inner.call
  end
end

class Probe
  def one
    Service.new(\"x\").call
  end

  def two
    service = Service.new(1)
    service.call
  end

  def three
    Service.new(1).limit
  end

  def four
    Service.new(1).via_self
  end

  def five
    Service.run(\"x\")
  end

  def six
    Service.new(\"x\").itself_again.call
  end

  def seven
    Wrapper.new(Service.new(1)).inner_call
  end

  def eight
    Wrapper.new(Service.new(\"x\")).inner_call
  end
end
";
        let (mut harness, uri) = with_types(source);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def itself_again -> Service\n  def one -> String\n  def two -> Integer\n  def three -> Integer\n  def four -> Integer\n  def five -> String\n  def six -> String\n  def seven -> Integer\n  def eight -> String"
        );
    }

    #[test]
    fn only_initialize_binds_and_only_through_class_new() {
        // - **A variable another method writes with something untyped** still refuses: `reset`'s
        //   `x` is no parameter of `initialize`.
        // - **A class's own `self.new`** may hand `initialize` something else (`super(amount.to_s)`),
        //   so nothing binds, and `Money#amount` stays unknown.
        // - **A parent's `initialize` runs where the class has none**, and binds.
        // - **`Class#new` as core declares it** binds, as a `new` nothing declares does.
        let source = "\
class Other
  def initialize(user)
    @user = user
  end

  def reset(x)
    @user = x
  end

  def user
    @user
  end
end

class Money
  def self.new(amount)
    super(amount.to_s)
  end

  def initialize(amount)
    @amount = amount
  end

  def amount
    @amount
  end
end

class Base
  def initialize(name)
    @name = name
  end
end

class Child < Base
  def name
    @name
  end
end

class Probe
  def one
    Other.new(1).user
  end

  def two
    Money.new(1).amount
  end

  def three
    Child.new(\"x\").name
  end
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class Class\n  def new: (*untyped) -> untyped\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def three -> String"
        );
    }

    #[test]
    fn a_reader_returns_its_variable_on_the_object() {
        // A reader has no body, so it read nothing before. It returns its variable, read as
        // a method of its class reads it: on the object a body is read for (`Presenter`'s `topic` is
        // what `new` passed), on the class object in `class << self` (`nil` joins, since a class
        // object runs no `initialize`), and behind a setter with what its calls pass: none here
        //. A `def` of the same name is the override, read as before.
        let source = "\
class Topic
  def title
    \"x\"
  end
end

class Fixed
  attr_reader :topic

  def initialize
    @topic = Topic.new
  end
end

class Presenter
  attr_reader :topic

  def initialize(topic)
    @topic = topic
  end

  def title
    topic.title
  end
end

class Settings
  class << self
    attr_reader :config
  end

  def self.setup
    @config = \"x\"
  end
end

class Open
  attr_accessor :topic

  def initialize
    @topic = Topic.new
  end
end

class Lazy
  attr_reader :topic

  def topic
    1
  end
end

class Probe
  def one
    Fixed.new.topic
  end

  def two
    Presenter.new(Topic.new).title
  end

  def three
    Settings.config
  end

  def four
    Open.new.topic
  end

  def five
    Lazy.new.topic
  end
end
";
        let (mut harness, uri) = with_types(source);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def title -> String\n  def self.setup -> String\n  def topic -> Integer\n  def one -> Topic\n  def two -> String\n  def three -> String?\n  def four -> Topic\n  def five -> Integer"
        );
        let markdown = card(&mut harness, &uri, source, "topic\n  end\n\n  def two");
        assert!(markdown.contains("Fixed#topic -> Topic"), "{markdown}");
    }

    /// The reading method's last `@x = value`, on every path, with nothing between
    /// that can run code, is the value: `other`'s `Integer` stays out. A call between (`touch`), a
    /// read in a block, another write between (in a branch), an `||=`, a write inside a call's
    /// arguments, and a call in a modifier's condition (which runs before the body it is written
    /// after) leave every write of the object in. The read's own chain and a call it is an argument
    /// of run after it, and so does a read in the condition itself.
    #[test]
    fn the_last_write_in_the_reading_method_decides_where_nothing_runs_between() {
        let source = "\
class Box
  def other
    @label = 1
  end

  def plain
    @label = \"x\"
    @label
  end

  def chained
    @label = \"x\"
    @label.upcase
  end

  def argument(flag)
    @label = \"x\"
    touch(@label)
  end

  def called
    @label = \"x\"
    touch
    @label
  end

  def looped
    @label = \"x\"
    [1].each { @label }
  end

  def branched(flag)
    @label = \"x\"
    @label = 1 if flag
    @label
  end

  def shortcut
    @label ||= \"x\"
    @label
  end

  def nested
    touch(@label = \"x\")
    @label
  end

  def guarded
    @label = \"x\"
    @label.upcase if touch
  end

  def plainly_guarded(flag)
    @label = \"x\"
    @label.upcase if flag
  end

  def in_condition
    @label = \"x\"
    touch if @label.upcase
  end

  def touch(value = nil)
  end
end
";
        let (mut harness, uri) = with_types(source);
        let drawn = drawn_hints(source, &harness.hints_in(&uri));
        let returns: Vec<&str> = drawn.lines().filter(|line| line.contains(" -> ")).collect();
        assert_eq!(
            returns,
            [
                "  def other -> Integer",
                "  def plain -> String",
                "  def chained -> String",
                "  def argument(flag) -> nil",
                "  def called -> Integer | String",
                "  def looped -> Array[Integer]",
                "  def branched(flag) -> Integer | String",
                "  def shortcut -> Integer | String",
                "  def nested -> Integer | String",
                "  def guarded -> String?",
                "  def plainly_guarded(flag) -> String?",
                "  def in_condition -> nil",
                "  def touch(value = nil) -> nil",
            ],
            "{drawn}"
        );
        for (needle, said) in [
            ("@label.upcase", "String"),
            ("@label)\n  end\n\n  def called", "String"),
            ("@label }", "Integer | String"),
            ("@label.upcase if touch", "Integer | String"),
            ("@label.upcase if flag", "String"),
            ("@label.upcase\n  end\n\n  def touch", "String"),
        ] {
            let markdown = card(&mut harness, &uri, source, needle);
            assert!(
                markdown.contains(&format!("@label: {said}\n")),
                "{needle}: {markdown}"
            );
        }
    }

    /// An empty local its own method fills holds what it was given, where no read of
    /// it escapes and every value is one sure class.
    #[test]
    fn an_empty_container_holds_what_its_method_puts_in_it() {
        let source = "\
class Foo
end

class Bar
end

class Builder
  def list(items)
    r = []
    items.each { |i| r << Foo.new }
    r
  end

  def pushed
    r = Array.new
    r.push(Foo.new, Foo.new)
    r.first
  end

  def keyed
    h = {}
    h[\"a\"] = Foo.new
    h
  end

  def stored
    h = Hash.new
    h.store(\"a\", Foo.new)
    return h
  end

  def indexed
    r = []
    r[0] = Foo.new
    r
  end

  def read_only
    r = []
    r << Foo.new
    r.each { |f| f }
    r.map { |f| f }
  end

  def mixed
    r = []
    r << Foo.new
    r << Bar.new
    r
  end

  def escaped
    r = []
    r << Foo.new
    keep(r)
    r
  end

  def aliased
    r = []
    r << Foo.new
    other = r
    r
  end

  def chained
    r = []
    r << Foo.new << Bar.new
    r
  end

  def untyped(value)
    r = []
    r << value
    r
  end

  def itself
    r = []
    r << r.size
    r
  end

  def arrow
    h = {}
    h << Foo.new
    h
  end

  def nothing
    r = []
    r.size
    r
  end

  def keep(value)
  end
end
";
        let (mut harness, uri) = with_types(source);
        let drawn = drawn_hints(source, &harness.hints_in(&uri));
        let returns: Vec<&str> = drawn.lines().filter(|line| line.contains(" -> ")).collect();
        assert_eq!(
            returns,
            [
                "  def list(items) -> Array[Foo]",
                "  def pushed -> Foo",
                "  def keyed -> Hash[String, Foo]",
                "  def stored -> Hash[String, Foo]",
                "  def indexed -> Array[Foo]",
                "  def read_only -> Array[Foo]",
                "  def mixed -> Array",
                "  def escaped -> Array",
                "  def aliased -> Array",
                "  def chained -> Array",
                "  def untyped(value) -> Array",
                "  def itself -> Array",
                "  def arrow -> Hash",
                "  def nothing -> Array",
                "  def keep(value) -> nil",
            ],
            "{drawn}"
        );
    }

    /// A current attribute's readers, on the class object and on its instance, are
    /// what its writer and `set(name: value)` are given on either, with `nil`; a literal default
    /// joins instead of `nil`. A value nothing types, `set(**options)`, a writer sent by name and
    /// a writer the class `def`s itself refuse; a subclass keeps its own store.
    #[test]
    fn a_current_attribute_is_what_its_writer_and_set_are_given() {
        let mut harness = signed(
            &[
                ("core/core.rbs", TYPED_RBS),
                (
                    "core/current.rbs",
                    "module ActiveSupport\n  class CurrentAttributes\n    \
                     def self.instance: () -> instance\n    \
                     def self.set: (**untyped) { () -> untyped } -> untyped\n  end\nend\n",
                ),
            ],
            "",
        );
        harness.write("app/models/account.rb", "class Account\nend\n");
        harness.write("app/models/user.rb", "class User\nend\n");
        let current = "\
class Current < ActiveSupport::CurrentAttributes
  attribute :account, :user
  attribute :locale, default: \"en\"
  attribute :request_id, :picked, :sent, :lane

  def lane=(value)
    super(value.to_s)
  end

  def check
    account
  end
end

class Sub < Current
end

class Other < ActiveSupport::CurrentAttributes
  attribute :spread
end

class Cache < ActiveSupport::CurrentAttributes
  attribute :store

  def self.fetch
    self.store ||= {}
  end
end
";
        let current_uri = harness.write("app/models/current.rb", current);
        harness.write(
            "app/services/app.rb",
            "\
class App
  def call(id, options)
    Current.account = Account.new
    Current.set(user: User.new) { 1 }
    Current.instance.account = nil
    Current.request_id = id
    Current.sent = 1
    Current.send(:sent=, 1)
    Current.lane = \"x\"
    Sub.user = Account.new
    Other.spread = 1
    Other.set(**options) { 1 }
  end
end
",
        );
        let source = "\
account = Current.account
user = Current.user
locale = Current.locale
request_id = Current.request_id
picked = Current.picked
sent = Current.sent
lane = Current.lane
sub = Sub.user
inherited = Sub.account
spread = Other.spread
stored = Cache.store
";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "\
account: Account? = Current.account
user: User? = Current.user
locale: String = Current.locale
sub: Account? = Sub.user
stored: Hash? = Cache.store"
        );
        // The instance's reader, called from the class's own method.
        assert!(
            drawn_hints(current, &harness.hints_in(&current_uri))
                .contains("  def check -> Account?"),
            "{}",
            drawn_hints(current, &harness.hints_in(&current_uri))
        );
    }

    #[test]
    fn a_call_on_self_has_nothing_or_self_written_before_its_name() {
        for (source, expected) in [
            ("  show", true),
            ("x = show", true),
            ("self.show", true),
            ("self&.show", true),
            ("self .show", true),
            ("post.show", false),
            ("Foo::show", false),
            ("myself.show", false),
            ("my_self.show", false),
            ("@self.show", false),
            (":self.show", false),
            ("x = self.show", true),
            ("(self).show", false),
        ] {
            let at = source.rfind("show").unwrap();
            assert_eq!(on_self(source, at), expected, "{source}");
        }
    }

    /// A `before_action` that writes a variable as a statement of its own body, before
    /// any `return`, keeps `nil` out of the actions it surely runs before.
    ///
    /// - `show` and the inherited `set_locale`'s `@locale` lose it; `index` is not in `only:`;
    ///   `edit` runs on an `Admin::PostsController` too, which skips `set_post` for it.
    /// - `feed`'s callback has `if:`; `listed`'s returns early; `called` is also called by `reuse`,
    ///   so it runs inside another action; `helper_read` is private; `load` is itself a callback.
    /// - `Plain`'s `initialize` returns early, so its variable keeps `nil` too.
    #[test]
    fn a_before_action_that_always_writes_keeps_nil_out_of_its_actions() {
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
        harness.write("app/models/post.rb", "class Post\nend\n");
        harness.write(
            "app/controllers/application_controller.rb",
            "\
class ApplicationController
  before_action :set_locale

  private

  def set_locale
    @locale = \"en\"
  end
end
",
        );
        let posts = "\
class PostsController < ApplicationController
  before_action :set_post, only: %i[show edit called]
  before_action :load_draft, if: :draft?
  before_action :set_list
  before_action :load

  def show
    @post
  end

  def locale
    @locale
  end

  def edit
    @post
  end

  def index
    @post
  end

  def feed
    @draft
  end

  def listed
    @list
  end

  def called
    @post
  end

  def reuse
    Post.new.show
    called
  end

  def load
    @locale
  end

  private

  def set_post
    @post = Post.new
  end

  def load_draft
    @draft = Post.new
  end

  def set_list
    return if params
    @list = Post.new
  end

  def helper_read
    @locale
  end
end

class Plain
  def initialize(skip)
    return if skip
    @seed = 1
  end

  def seed
    @seed
  end
end

module Loading
  def show
    @loaded
  end

  def load
    @loaded = 1
  end
end
";
        let uri = harness.write("app/controllers/posts_controller.rb", posts);
        harness.write(
            "app/controllers/admin/posts_controller.rb",
            "class Admin::PostsController < PostsController\n  skip_before_action :set_post, only: :edit\nend\n",
        );
        harness.index();
        let drawn = drawn_hints(posts, &harness.hints_in(&uri));
        let returns: Vec<&str> = drawn.lines().filter(|line| line.contains(" -> ")).collect();
        assert_eq!(
            returns,
            [
                "  def show -> Post",
                "  def locale -> String",
                "  def edit -> Post?",
                "  def index -> Post?",
                "  def feed -> Post?",
                "  def listed -> Post?",
                "  def called -> Post?",
                "  def reuse -> Post?",
                "  def load -> String?",
                "  def set_post -> Post",
                "  def load_draft -> Post",
                "  def set_list -> Post?",
                "  def helper_read -> String?",
                "  def seed -> Integer?",
                "  def show -> Integer?",
                "  def load -> Integer",
            ],
            "{drawn}"
        );
    }

    /// An `attr_writer` writes what its calls pass, where their receiver can be the
    /// object.
    ///
    /// - `self.label ||= "y"` in `Box` and `Box.new.label = 1` elsewhere pass a `String` and an
    ///   `Integer`; a `Crate`, a subclass, passes `nil`.
    /// - `Tag`'s writer of the same name is another object's, and a spec's call is the suite's.
    /// - With no call at all, the setter adds nothing: `Still`'s variable is what `initialize` wrote.
    #[test]
    fn an_attr_writer_holds_what_its_calls_pass() {
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
        let boxed = "\
class Box
  attr_accessor :label

  def shown
    @label
  end

  def rename
    self.label ||= \"y\"
  end
end
";
        let box_uri = harness.write("app/models/box.rb", boxed);
        harness.write("app/models/crate.rb", "class Crate < Box\nend\n");
        harness.write(
            "app/models/tag.rb",
            "class Tag\n  attr_writer :label\nend\n",
        );
        harness.write(
            "app/services/packer.rb",
            "\
class Packer
  def pack
    Box.new.label = 1
    Crate.new.label = nil
    Tag.new.label = 1.5
  end
end
",
        );
        harness.write("spec/models/box_spec.rb", "Box.new.label = 1.5\n");
        let still = "\
class Still
  attr_writer :count

  def initialize
    @count = 1
  end

  def shown
    @count
  end
end
";
        let still_uri = harness.write("app/models/still.rb", still);
        harness.index();
        assert_eq!(
            drawn_hints(boxed, &harness.hints_in(&box_uri)),
            "  def shown -> String? | Integer"
        );
        assert_eq!(
            drawn_hints(still, &harness.hints_in(&still_uri)),
            "  def shown -> Integer"
        );
    }

    /// Where one call cannot be placed, or a writer Ruby did not make may run, the read
    /// says nothing: a receiver nothing types (`thing`) or only its name guesses (`guessed`), the
    /// writer sent by name to the object or on `self`, a subclass's `def e=`, and a value reading
    /// the variable again. Each but `Looped` has a typed call beside it. `Control` is the one each
    /// rule leaves alone: another name sent, a reader sent, and a writer sent to another class.
    #[test]
    fn an_attr_writer_refuses_where_a_call_may_write_anything() {
        let source = "\
class Loose
  attr_writer :a

  def shown
    @a
  end
end

class Sent
  attr_writer :b

  def shown
    @b
  end
end

class Built
  attr_writer :c

  def shown
    @c
  end

  def assign(key, value)
    public_send(\"#{key}=\", value)
  end
end

class Guessed
  attr_writer :d

  def shown
    @d
  end
end

class Overridden
  attr_writer :e

  def shown
    @e
  end
end

class Special < Overridden
  def e=(value)
    @e = value.to_s
  end
end

class Looped
  attr_accessor :n

  def initialize
    @n = 1
  end

  def shown
    @n
  end

  def bump
    self.n = n.succ
  end
end

class Other
  attr_writer :f
end

class Control
  attr_writer :f

  def shown
    @f
  end
end

class Caller
  def call(thing, guessed)
    thing.a = 1
    Sent.new.b = 1
    Sent.new.send(:b=, 1)
    Built.new.c = 1
    guessed.d = 1
    Overridden.new.e = 1
    Control.new.f = 1
    Control.new.send(:g=, 1)
    Control.new.public_send(:f)
    Other.new.send(:f=, 1)
  end
end
";
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
        let uri = harness.write("app/models/all.rb", source);
        harness.index();
        let drawn = drawn_hints(source, &harness.hints_in(&uri));
        let shown: Vec<&str> = drawn
            .lines()
            .filter(|line| line.contains("def shown"))
            .collect();
        assert_eq!(shown, ["  def shown -> Integer?"], "{drawn}");
    }

    /// A class object's setter is read on the class, and a gem module of the object's
    /// that builds a writer's name on `self` (ActiveModel's `assign_attributes`) may write anything;
    /// one that only forwards what its caller sends does not.
    #[test]
    fn an_attr_writer_on_a_class_or_beside_a_gem_s_assignment() {
        let (dir, _elsewhere, env) = project_with_gem(
            "\
module Shouty
  module Assign
    def assign(attributes)
      attributes.each do |key, value|
        setter = :\"#{key}=\"
        public_send(setter, value)
      end
    end
  end

  module Forward
    def forward(*args)
      public_send(*args)
    end
  end
end
",
        );
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!("[rbs]\npath = {:?}\n", signatures.display().to_string()),
        )
        .unwrap();
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let source = "\
class Token
end

class Settings
  class << self
    attr_accessor :config
  end

  def self.shown
    @config
  end
end

class Form
  include Shouty::Assign
  attr_accessor :name

  def shown
    @name
  end
end

class Relay
  include Shouty::Forward
  attr_accessor :name

  def shown
    @name
  end
end

class Caller
  def call
    Settings.config = Token.new
    Form.new.name = Token.new
    Relay.new.name = Token.new
  end
end
";
        let uri = harness.write("app/models/all.rb", source);
        harness.index();
        harness.index_gems();
        let drawn = drawn_hints(source, &harness.hints_in(&uri));
        let shown: Vec<&str> = drawn
            .lines()
            .filter(|line| line.contains("def shown") || line.contains("def self.shown"))
            .collect();
        assert_eq!(
            shown,
            ["  def self.shown -> Token?", "  def shown -> Token?"],
            "{drawn}"
        );
    }

    #[test]
    fn a_signature_s_reader_returns_its_declared_type() {
        // RBS declares readers the Ruby cannot type: an accessor's variable (its setter
        // writes anything), and a class-side one nothing in the Ruby writes. Each is a `def` with no
        // arguments, so the table answers first, before any Ruby reader is read. A writer declares
        // no reader.
        let source = "\
class Book
  attr_reader :author
  attr_accessor :pages
  attr_writer :note

  class << self
    attr_reader :shelf
  end

  def initialize(author)
    @author = author
  end
end

class Probe
  def one
    Book.new(\"x\").author
  end

  def two
    Book.new(\"x\").pages
  end

  def three
    Book.shelf
  end
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class Book\n  attr_reader author: String\n  attr_accessor pages: Integer\n  \
             attr_writer note: String\n  attr_reader self.shelf: Array[String]?\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def one -> String\n  def two -> Integer\n  def three -> Array[String]?"
        );
    }

    #[test]
    fn a_method_that_only_calls_super_is_typed_from_the_one_it_overrides() {
        // Ruby takes `super`'s name from the enclosing method and starts the lookup one place above
        // the class the method was found on: a walk of the linearization an instance variable's
        // writers come from, entered one step further along.
        let mut harness = Harness::new();
        let uri = super_app(
            &harness,
            "class Story < BaseStory\n  def title\n    super\n  end\nend\n",
        );
        harness.index();

        let markdown = card(&mut harness, &uri, SUPER_PROBE, "title");
        assert!(markdown.contains("Story#title -> Label"), "{markdown}");
        assert!(!markdown.contains(GUESS_FOOTNOTE), "{markdown}");
    }

    #[test]
    fn super_lands_on_the_nearest_ancestor_above_and_not_on_the_superclass() {
        // The two ways this rung could go wrong, in one fixture.
        //
        // 1. **The ordinary lookup starts at `self`**, and `Story#title` is declared there, by the
        //    `def` the keyword is inside. Not skipping it would read a method's own body as its
        //    return.
        // 2. **An `include`d module sits between the class and its superclass** in Ruby's
        //    linearization, so `super` reaches `Nearer`, not `BaseStory`. The order is rubydex's
        //    `ancestors()`, not a rule written here.
        let mut harness = Harness::new();
        let uri = super_app(
            &harness,
            "module Nearer\n  def title\n    Near.new\n  end\nend\n\n\
             class Story < BaseStory\n  include Nearer\n\n  def title\n    super\n  end\nend\n",
        );
        harness.index();

        let markdown = card(&mut harness, &uri, SUPER_PROBE, "title");
        assert!(markdown.contains("Story#title -> Near"), "{markdown}");
    }

    #[test]
    fn super_past_an_included_module_reaches_the_superclass_not_object() {
        // A module in the walk is one step of the class's linearization: one that lacks the
        // method passes on to the superclass. Asking it as a module's own `self` is asked would
        // fall back to `Object`, whose `with` ActiveSupport writes, and answer that.
        let mut harness = Harness::new();
        let uri = super_app(
            &harness,
            "class Object\n  def title(**)\n    Near.new\n  end\nend\n\n\
             module Plain\nend\n\n\
             class Story < BaseStory\n  include Plain\n\n  def title\n    super\n  end\nend\n",
        );
        harness.index();

        let markdown = card(&mut harness, &uri, SUPER_PROBE, "title");
        assert!(markdown.contains("Story#title -> Label"), "{markdown}");
    }

    #[test]
    fn super_inside_a_module_is_refused_because_the_including_class_decides() {
        // A `super` inside a `module` looks up through the *including* class's ancestry, which the
        // module does not have. There is no answer to give, and this gives none.
        let mut harness = Harness::new();
        let uri = super_app(
            &harness,
            "module Titled\n  def title\n    super\n  end\nend\n\n\
             class Story < BaseStory\n  include Titled\nend\n",
        );
        harness.index();

        let markdown = card(&mut harness, &uri, SUPER_PROBE, "title");
        assert!(!markdown.contains("->"), "{markdown}");
    }

    #[test]
    fn super_on_the_singleton_side_climbs_the_singleton_and_not_the_instance() {
        // `def self.build` is a member of the class object, and its `super` walks the class
        // object's ancestry, which `scope.caller` already answers. No second rule needed.
        let mut harness = Harness::new();
        let uri = super_app(
            &harness,
            "class Story < BaseStory\n  def self.build\n    super\n  end\nend\n",
        );
        harness.index();

        let markdown = card(&mut harness, &uri, SUPER_PROBE, "build");
        assert!(markdown.contains("Story.build -> Built"), "{markdown}");
    }

    #[test]
    fn a_name_nothing_above_declares_is_no_answer_rather_than_a_wrong_one() {
        // `super` in a method nothing above defines is a `NoMethodError` at runtime. The walk
        // reaches the end of the ancestry and stops: the loop's other exit.
        let mut harness = Harness::new();
        let uri = super_app(
            &harness,
            "class Story < BaseStory\n  def nowhere\n    super\n  end\nend\n",
        );
        harness.index();

        let markdown = card(&mut harness, &uri, SUPER_PROBE, "nowhere");
        assert!(!markdown.contains("->"), "{markdown}");
    }

    fn ancestor_app(harness: &Harness, body: &str) -> DocUri {
        harness.write("app/models/story.rb", STORY);
        harness.write(
            "app/controllers/application_controller.rb",
            "class ApplicationController\n  def load\n    @subject = Story.new\n  end\nend\n",
        );
        harness.write("app/controllers/stories_controller.rb", body)
    }

    #[test]
    fn an_instance_variable_this_file_never_assigns_is_typed_from_the_class_above_it() {
        // Many instance-variable reads are of a name their own file never writes, and the writing
        // class is usually one step up the receiver's ancestry. Every class of the object is read
        // ([`instance_read`]), by the code's own `<` and `include`, not a path.
        let mut harness = Harness::new();
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        let source = "\
class StoriesController < ApplicationController
  def show
    @subject.title
  end
end
";
        let uri = ancestor_app(&harness, source);
        harness.index();

        let markdown = card(&mut harness, &uri, source, "title");
        assert!(markdown.contains("Story#title"), "{markdown}");
        assert!(!markdown.contains(GUESS_FOOTNOTE), "{markdown}");
        // Derived, not guessed, which is what matters: a guess is the one tier a margin never draws
        // and `implementation` declines.
        assert!(!markdown.contains("Guessed from name alone"), "{markdown}");
    }

    #[test]
    fn a_concern_that_assigns_it_answers_exactly_as_a_superclass_does() {
        // The commonest real shape: one application writes `@account` in `AccountOwnedConcern` and
        // reads it in a dozen controllers; another writes `@user` in `Authenticatable`. An `include` and
        // a `<` are treated the same, because rubydex has already linearized both into one chain in
        // Ruby's order. Re-deriving that order would drift from what the type hierarchy answers.
        let mut harness = Harness::new();
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        harness.write("app/models/story.rb", STORY);
        harness.write(
            "app/controllers/concerns/subject_owned.rb",
            "module SubjectOwned\n  def load\n    @subject = Story.new\n  end\nend\n",
        );
        let source = "\
class StoriesController
  include SubjectOwned

  def show
    @subject.title
  end
end
";
        let uri = harness.write("app/controllers/stories_controller.rb", source);
        harness.index();

        let markdown = card(&mut harness, &uri, source, "title");
        assert!(markdown.contains("Story#title"), "{markdown}");
        assert!(!markdown.contains("Guessed from name alone"), "{markdown}");
    }

    #[test]
    fn the_singleton_side_is_refused_rather_than_answered_from_an_instances_writes() {
        // **The one way this rung could be wrong, not just absent.** `scopes::writes_to` answers
        // for an *instance* (`owner.level == 0`), and `@subject` inside `def self.build` is a
        // different variable with the same spelling. Giving it the instance's writes would be a
        // confident, checkable-looking wrong answer. So the singleton side is declined and falls to
        // the name rung.
        let mut harness = Harness::new();
        let source = "\
class StoriesController < ApplicationController
  def self.build
    @subject.title
  end
end
";
        let uri = ancestor_app(&harness, source);
        harness.index();

        let markdown = card(&mut harness, &uri, source, "title");
        assert!(
            !markdown.contains("Type taken from `ApplicationController`"),
            "{markdown}"
        );
    }

    #[test]
    fn an_ancestor_whose_own_answer_is_a_guess_is_not_worth_the_file_it_is_read_from() {
        // The rung below guesses `@story` is a `Story` from its letters, and an ancestor's file
        // would guess the same way. Taking it would spend a cross-document read to reach what the
        // free rung already has, and dress the guess in a file and line that look like evidence.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        harness.write(
            "app/controllers/application_controller.rb",
            "class ApplicationController\n  def load\n    @story = story\n  end\nend\n",
        );
        let source = "\
class StoriesController < ApplicationController
  def show
    @story.title
  end
end
";
        let uri = harness.write("app/controllers/stories_controller.rb", source);
        harness.index();

        let markdown = card(&mut harness, &uri, source, "title");
        assert!(markdown.contains("Guessed from name alone"), "{markdown}");
        assert!(
            !markdown.contains("Type taken from `ApplicationController`"),
            "{markdown}"
        );
    }

    #[test]
    fn a_name_no_ancestor_assigns_falls_through_to_the_rung_below() {
        // The chain was walked, and it never writes this variable. The same refusal a template
        // gets when its controller assigns nothing by that name: a class is not an answer to *what
        // is this variable*.
        let mut harness = Harness::new();
        let source = "\
class StoriesController < ApplicationController
  def show
    @missing.title
  end
end
";
        let uri = ancestor_app(&harness, source);
        harness.index();

        let markdown = card(&mut harness, &uri, source, "title");
        assert!(
            !markdown.contains("which is above this class in its ancestry"),
            "{markdown}"
        );
    }

    /// `StoriesController < Step0 < … < Step(n-1) < Deepest`, where only `Deepest` writes
    /// `@subject`, so the rung must read exactly `steps` documents before one answers.
    fn a_chain_of(harness: &Harness, steps: usize) -> (DocUri, String) {
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        harness.write("app/models/story.rb", STORY);
        for step in 0..steps {
            let parent = if step + 1 == steps {
                "Deepest".to_owned()
            } else {
                format!("Step{}", step + 1)
            };
            harness.write(
                &format!("app/controllers/step{step}_controller.rb"),
                &format!("class Step{step} < {parent}\nend\n"),
            );
        }
        harness.write(
            "app/controllers/deepest_controller.rb",
            "class Deepest\n  def load\n    @subject = Story.new\n  end\nend\n",
        );
        let source = "\
class StoriesController < Step0
  def show
    @subject.title
  end
end
"
        .to_owned();
        let uri = harness.write("app/controllers/stories_controller.rb", &source);
        (uri, source)
    }

    /// `StoriesController < Relay0 < … < Relay(n-1) < Source`, where each relay passes the variable
    /// on to the next and only `Source` names a class.
    ///
    /// **`relays` relays cost `relays + 1` reads.** `@step0` is read here, found in `Relay0` as
    /// `@step1` (one), in `Relay1` as `@step2` (two), and so on, then in `Source` as a class (one
    /// more). Each is answered once per request ([`Reads`]), so the chain costs its length.
    fn a_relay_of(harness: &Harness, relays: usize) -> (DocUri, String) {
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        harness.write("app/models/story.rb", STORY);
        for step in 0..relays {
            let parent = if step + 1 == relays {
                "Source".to_owned()
            } else {
                format!("Relay{}", step + 1)
            };
            harness.write(
                &format!("app/controllers/relay{step}_controller.rb"),
                &format!(
                    "class Relay{step} < {parent}\n  def load\n    @step{step} = @step{}\n  \
                     end\nend\n",
                    step + 1
                ),
            );
        }
        harness.write(
            "app/controllers/source_controller.rb",
            &format!("class Source\n  def load\n    @step{relays} = Story.new\n  end\nend\n"),
        );
        let source = "\
class StoriesController < Relay0
  def show
    @step0.title
  end
end
"
        .to_owned();
        let uri = harness.write("app/controllers/stories_controller.rb", &source);
        (uri, source)
    }

    #[test]
    fn a_relay_through_many_classes_is_followed_to_the_class_that_names_one() {
        // `@a = @b` in a superclass whose file never writes `@b` is a real shape. Each step is a
        // read of its own, answered once per request, so the depth costs one read per class and
        // there is no hop limit to fall off.
        let mut harness = Harness::new();
        let (uri, source) = a_relay_of(&harness, 4);
        harness.index();

        let markdown = card(&mut harness, &uri, source.as_str(), "title");
        assert!(markdown.contains("Story#title"), "{markdown}");
        assert!(!markdown.contains(GUESS_FOOTNOTE), "{markdown}");
    }

    #[test]
    fn two_classes_that_hand_a_variable_back_and_forth_settle_and_return() {
        // **The assertion is that this test returns.** `@a = @b` above and `@b = @a` below is a
        // cycle through two files; the reads' memo answers it in rounds like any other, instead of
        // a stack overflow the request bulkhead cannot contain.
        let mut harness = Harness::new();
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        harness.write("app/models/story.rb", STORY);
        harness.write(
            "app/controllers/relay_controller.rb",
            "class Relay\n  def load\n    @a = @b\n    @a = Story.new\n  end\nend\n",
        );
        let source = "\
class StoriesController < Relay
  def swap
    @b = @a
  end

  def show
    @a.title
  end
end
";
        let uri = harness.write("app/controllers/stories_controller.rb", source);
        harness.index();

        let markdown = card(&mut harness, &uri, source, "title");
        assert!(markdown.contains("Story#title"), "{markdown}");
    }

    #[test]
    fn a_chain_of_ancestors_is_walked_to_the_class_that_writes_the_variable() {
        // The writing class nine steps up: every ancestor of the object's class is read, with no
        // depth bound, because stopping early would answer from the writes that happened to be
        // near.
        let mut harness = Harness::new();
        let (uri, source) = a_chain_of(&harness, 9);
        harness.index();

        let markdown = card(&mut harness, &uri, source.as_str(), "title");
        assert!(markdown.contains("Story#title"), "{markdown}");
        assert!(!markdown.contains("Guessed from name alone"), "{markdown}");
    }

    #[test]
    fn a_view_directory_that_names_no_mailer_renders_nothing() {
        // The gate, and why the mailer half is not the controller half minus a suffix.
        // `rails::controller_of` produces a name only a controller has; `rails::mailer_of` produces
        // whatever the directory spells, so `app/views/report/` spells `Report`. An application
        // with a `Report` class that writes `@story` would get a card and a jump invented from a
        // directory name. The gate is the application's superclass table, the same list
        // `analysis::views` gates the view context on.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        harness.write(
            "app/models/report.rb",
            "class Report\n  def build\n    @story = Story.new\n  end\nend\n",
        );
        let source = "<%= @story.title %>\n";
        let view = harness.write("app/views/report/show.html.erb", source);
        harness.index();

        // `@story` still spells a class, so the guess answers, and the card says so. That is the
        // difference from the convention answering.
        let markdown = card(&mut harness, &view, source, "title");
        assert!(markdown.contains("Guessed from name alone"), "{markdown}");
        assert!(
            !markdown.contains("renders this template from"),
            "{markdown}"
        );

        let found = harness.definition_at(&view, source, "@story");
        assert!(found.is_null(), "{found}");
    }

    #[test]
    fn the_views_switch_takes_the_jump_with_it() {
        // One gate for both halves of the rung. A project that turned the convention off because
        // its `app/views/` is not Rails' must not get a jump the card refuses.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n\n[rails]\nviews = false\n",
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let view = rails_app(&harness);
        harness.index();

        let source = "<h1><%= @story.title %></h1>\n";
        let found = harness.definition_at(&view, source, "@story");
        assert!(found.is_null(), "{found}");
    }

    #[test]
    fn a_document_s_foreign_names_are_held_while_its_version_is_the_same() {
        let held = HeldExits::new();
        let document = UriId::from("file:///poke.rb");
        let reads = std::cell::Cell::new(0);
        let read = |name: &str| {
            reads.set(reads.get() + 1);
            Some(ForeignNames::from(vec![scopes::Spelled::Exactly(
                name.to_owned(),
            )]))
        };
        let first = held.foreign(document, 1, || read("@a"));
        let same = held.foreign(document, 1, || read("@b"));
        assert_eq!(first, same);
        assert_eq!(reads.get(), 1, "the held version was read again");
        // Another version is read, and replaces the one held.
        let changed = held.foreign(document, 2, || read("@b"));
        assert_eq!(reads.get(), 2);
        assert_ne!(first, changed);
        // A version that cannot be read holds nothing.
        assert!(held.foreign(document, 3, || None).is_none());
    }

    #[test]
    fn the_memo_reads_a_document_once_and_answers_what_reading_it_again_would() {
        let source = "class Story\n  def title\n    \"x\"\n  end\nend\n";
        let reads = std::cell::Cell::new(0);
        let read = |_: &str| {
            reads.set(reads.get() + 1);
            Some((Rc::from(source), Rebase::identity(source.len() as u32)))
        };

        let held = ReadBodies::default();
        let first = held.of("file:///story.rb", &read).expect("a document");
        let again = held.of("file:///story.rb", &read).expect("a document");
        assert_eq!(reads.get(), 1, "the second ask re-read the document");
        assert!(Rc::ptr_eq(&first, &again));

        // The memoized answer equals the unmemoized one, so a memo changes cost, never an answer.
        let fresh = ReadBodies::read("file:///story.rb", &read).expect("a document");
        assert_eq!(reads.get(), 2);
        let uri = "file:///story.rb";
        assert_eq!(
            first.shapes(uri, &HeldExits::new()),
            fresh.shapes(uri, &HeldExits::new())
        );
        assert_eq!(first.source, fresh.source);
        assert_eq!(first.shapes(uri, &HeldExits::new()).returns.len(), 1);

        // A different document is a different read: the memo is keyed, not a single slot.
        assert!(held.of("file:///other.rb", &read).is_some());
        assert_eq!(reads.get(), 3);
    }

    #[test]
    fn a_document_that_could_not_be_read_is_remembered_rather_than_retried() {
        // The reasons a read fails (no such URI, an unsettled edit) do not change within a request,
        // and a file of fifty `def`s would otherwise ask fifty times.
        let reads = std::cell::Cell::new(0);
        let read = |_: &str| {
            reads.set(reads.get() + 1);
            None
        };
        let held = ReadBodies::default();
        assert!(held.of("file:///gone.rb", &read).is_none());
        assert!(held.of("file:///gone.rb", &read).is_none());
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn the_held_walk_is_reused_while_the_text_is_the_same_and_redone_when_it_is_not() {
        // The part of a read that outlives the request. What `returns_in` reads depends only on the
        // string, so the same string gives the same answer whatever happened to the graph in
        // between.
        let before = "class Story\n  def title\n    \"x\"\n  end\nend\n";
        let after = "class Story\n  def title\n    1\n  end\nend\n";
        let held = HeldExits::new();
        assert!(held.is_empty());

        let first = held.of("file:///story.rb", before);
        let again = held.of("file:///story.rb", before);
        assert!(Rc::ptr_eq(&first, &again), "the same text was walked twice");
        assert_eq!(held.len(), 1);

        let edited = held.of("file:///story.rb", after);
        assert!(!Rc::ptr_eq(&first, &edited), "an edit kept the old walk");
        assert_eq!(held.len(), 1, "the edit left the old text behind");

        // What it returns is what the walk returns: the one property the cache must have and could
        // break.
        assert_eq!(*edited, cursor::shapes(after));
    }

    #[test]
    fn the_held_escapes_are_walked_once_per_text_and_again_for_another() {
        // `locator::Modifiers`' half of the cache, keyed like the walk above: by the text, so an
        // open buffer typed into is walked again (`locator`'s
        // `an_edit_that_leaves_the_def_where_it_was_is_walked_again_…` holds it end to end).
        let held = HeldExits::new();
        let walks = std::cell::Cell::new(0);
        let walk = |found: u32| {
            walks.set(walks.get() + 1);
            vec![found]
        };
        let first = held.escapes("file:///fields.rb", "before", || walk(1));
        let again = held.escapes("file:///fields.rb", "before", || walk(2));
        assert!(Rc::ptr_eq(&first, &again), "the same text was walked twice");
        assert_eq!(walks.get(), 1);

        let edited = held.escapes("file:///fields.rb", "after", || walk(3));
        assert_eq!(*edited, [3], "an edit kept the old walk");
        // One slot per document: the text before the edit is walked again, not remembered.
        held.escapes("file:///fields.rb", "before", || walk(4));
        assert_eq!(walks.get(), 3);
        // Another document is its own entry.
        assert_eq!(*held.escapes("file:///other.rb", "before", || walk(5)), [5]);
    }

    #[test]
    fn the_held_walk_lets_go_of_everything_once_it_is_full() {
        // A session is unbounded and its cache must not be. Dropping everything rather than the
        // oldest is deliberate (see `HELD_DOCUMENTS`). What matters is that the bound exists and
        // the cache keeps answering after it fires.
        let source = "def title\n  \"x\"\nend\n";
        let held = HeldExits::new();
        for n in 0..HELD_DOCUMENTS {
            held.of(&format!("file:///{n}.rb"), source);
        }
        assert_eq!(held.len(), HELD_DOCUMENTS);

        let over = held.of("file:///over.rb", source);
        assert_eq!(held.len(), 1, "the cache grew past its bound");
        assert_eq!(*over, cursor::shapes(source));
    }

    #[test]
    fn a_held_text_answers_only_its_own_version_and_lets_go_once_full() {
        let held = HeldExits::new();
        let rebase = Rebase::identity(3);
        held.keep_text("file:///a.rb", 1, &Rc::from("old"), rebase);
        assert_eq!(
            held.text("file:///a.rb", 1),
            Some((Rc::from("old"), rebase))
        );
        assert_eq!(
            held.text("file:///a.rb", 2),
            None,
            "another version is not this text"
        );
        assert_eq!(held.text("file:///b.rb", 1), None);

        // Kept again at a new version: the old one is gone, and so are its bytes.
        held.keep_text("file:///a.rb", 2, &Rc::from("new"), rebase);
        assert_eq!(held.text("file:///a.rb", 1), None);
        assert_eq!(held.texts.borrow().bytes, 3);

        // Past the bound everything goes, and what was just kept is held.
        let big: Rc<str> = Rc::from("x".repeat(HELD_TEXT_BYTES - 1));
        held.keep_text("file:///big.rb", 3, &big, rebase);
        assert_eq!(
            held.text("file:///a.rb", 2),
            None,
            "the texts grew past their bound"
        );
        assert!(held.text("file:///big.rb", 3).is_some());

        held.forget_texts();
        assert_eq!(held.text("file:///big.rb", 3), None);
    }

    #[test]
    fn a_document_is_walked_once_however_many_offsets_are_placed_in_it() {
        // `Scope::at` walks every definition in a document, and the body rung asks it once per
        // `def` it reads: the quadratic `Scope::bodies` avoids, one rung down. Keyed by `UriId`, so
        // another document's walk can never be returned.
        let source = "class Story\n  def title\n    \"x\"\n  end\nend\n";
        let (harness, uri) = with_types(source);
        let graph = &harness.analysis.graph;
        let uri_id = UriId::from(uri.as_str());

        let walked = Walked::default();
        let first = walked.of(graph, uri_id);
        let again = walked.of(graph, uri_id);
        assert!(Rc::ptr_eq(&first, &again), "one document, two walks");

        // The memoised placement equals the unmemoised one: `Sources::scope_at` picks between them
        // on cost alone, so a mismatch would be a difference between surfaces, not speed.
        let inside = source.find("\"x\"").expect("a body") as u32;
        let fresh = Scope::at(graph, uri_id, inside);
        assert_eq!(first.at(inside).nesting, fresh.nesting);
        assert_eq!(first.at(inside).self_id, fresh.self_id);
    }

    #[test]
    fn a_second_request_reads_no_document_the_first_one_already_walked() {
        // Why the cache lives on `Analysis`, not the request: every surface that types a receiver
        // reads the bodies behind it, and without the cache that read dominates a whole-file
        // `inlayHint`, repeated on every keystroke over unchanged files.
        let source =
            "class Story\n  def title\n    \"x\"\n  end\n\n  def shown\n    title\n  end\nend\n";
        let (mut harness, uri) = with_types(source);
        assert!(harness.analysis.exits.is_empty());

        harness.hints_in(&uri);
        let after_one = harness.analysis.exits.len();
        assert!(after_one > 0, "the rung read nothing at all");

        harness.hints_in(&uri);
        assert_eq!(
            harness.analysis.exits.len(),
            after_one,
            "the second request walked a document the first one had already walked"
        );
    }

    #[test]
    fn a_call_on_a_union_is_made_on_each_class_that_has_the_member() {
        // `split` answers `String | Integer`. A call on it runs on whichever class the value is,
        // and where that class has no such method Ruby raises, so it adds nothing to the value:
        //
        // - `narrowed` is `String`: `Integer` has no `upcase`.
        // - `both` is the union: `tap` is on both halves and answers `self` on each.
        // - `nobody` answers nothing: no class of the union has the member, the name rung's case.
        // - `haunted` refuses, because a class *might* answer: `Ghost` writes its own
        //   `method_missing`.
        // - `guarded` is `String`: `Keeper#upcase` is private, and on a written receiver Ruby raises
        //   there as for a missing member.
        let source = "\
class Ledger
  def split
    if stamped?
      \"x\"
    else
      1
    end
  end

  def haunting
    stamped? ? \"x\" : Ghost.new
  end

  def keeping
    stamped? ? \"x\" : Keeper.new
  end

  def narrowed
    split.upcase
  end

  def both
    split.tap { |held| held }
  end

  def nobody
    split.frobnicate
  end

  def haunted
    haunting.upcase
  end

  def guarded
    keeping.upcase
  end
end

class Ghost
  def method_missing(name, *arguments)
    arguments
  end
end

class Keeper
  private

  def upcase
    \"x\"
  end
end

class Shouter
  def upcase
    1
  end
end
";
        let (mut harness, uri) = with_types(source);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def split -> String | Integer
  def haunting -> String | Ghost
  def keeping -> String | Keeper
  def narrowed -> String
  def both -> String | Integer
  def guarded -> String
  def method_missing(name, *arguments) -> Array
  def upcase -> String
  def upcase -> Integer"
        );
        // The jump agrees with the type: `upcase` is `String`'s, the one class of the union that
        // has it, and not `Shouter`'s, which the name alone would add.
        assert_eq!(
            jumps(&mut harness, &uri, source, "upcase\n  end\n\n  def both"),
            ["core.rbs:10:6"]
        );
    }

    #[test]
    fn a_call_on_a_union_is_each_definition_its_classes_reach() {
        // `URI.parse` is ten classes that all inherit `URI::Generic#host`: one place, so the jump
        // and the card name it, as the margin already types the call. Where each class answers from
        // its own definition, the answer is each of them, sure, as a concern's `self` is: the card
        // counts them and the jump lists them. An unrelated `Other#host` is never among them.
        let source = "\
class Base
  def host = \"h\"
  def name = \"b\"
end

class First < Base
  def name = \"f\"
end

class Second < Base
end

class Other
  def host = 1
end

class Use
  def pick = ok? ? First.new : Second.new

  def shared
    pick.host
  end

  def split
    pick.name
  end
end
";
        let (mut harness, uri) = with_types(source);
        let shared = "host\n  end\n\n  def split";
        assert_eq!(jumps(&mut harness, &uri, source, shared), ["main.rb:2:6"]);
        let card = harness.hover_at(&uri, source, shared)["contents"]["value"]
            .as_str()
            .unwrap_or("null")
            .to_owned();
        assert!(
            card.contains("Base#host") && !card.contains(GUESS_FOOTNOTE),
            "{card}"
        );
        let split = "name\n  end\nend";
        let card = harness.hover_at(&uri, source, split)["contents"]["value"]
            .as_str()
            .unwrap_or("null")
            .to_owned();
        assert!(
            card.contains("2 definitions") && !card.contains(GUESS_FOOTNOTE),
            "{card}"
        );
        // In the union's order: `First`'s own, then the one `Second` inherits.
        assert_eq!(
            jumps(&mut harness, &uri, source, split),
            ["main.rb:7:6", "main.rb:3:6"]
        );
    }

    #[test]
    fn an_argument_nothing_types_is_answered_by_every_arm_it_could_reach() {
        // `pick` has two arms of one arity, told apart only by the argument. An argument nothing
        // types picks neither, but one of them runs, so the call is their union. `lookup` has an
        // arm whose return the table refuses (`untyped`), and that arm could return anything: the
        // union would rest on the readable arm alone, so it answers nothing.
        let source = "\
class Report
  def picked(raw)
    Widget.new.pick(raw)
  end

  def looked_up(raw)
    Widget.new.lookup(raw)
  end
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class Widget\n  def pick: (Integer) -> String\n          | (String) -> Array[String]\n  \
             def lookup: (Integer) -> String\n            | (String) -> untyped\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // A type argument survives a join only where every class agrees on it (`Join`), and
            // `String` holds none.
            "  def picked(raw) -> String | Array"
        );
    }

    #[test]
    fn a_keyword_call_reaches_the_arms_its_names_can_run() {
        // `read` agrees only for a call with keywords, where one arm takes the hash as
        // keywords and the other cannot take a second positional; the row was dropped and the
        // body answered. A required keyword rules its arm out of a call that does not write it,
        // and a name no arm takes rules them all out (`sep:`, which `read`'s keyword arm has no
        // `**rest` for), which leaves the body, as a count past every arm does. `load` reaches both arms with `headers:` (the other takes the hash as its
        // options), so the call is either. `**opts` may be empty and pass no keywords at all, so it
        // also reaches the arms of its plain count; a hash with a pair in it never is, whatever its
        // keys, so `where(\"a.b\" => 1)` is not a bare `where`.
        let source = "\
class Loader
  def self.read(path, **options) = [path]
  def self.load(path, options = {}) = [path]
end

class Report
  def run(opts)
    keyed = Loader.read(\"p\", headers: true)
    plain = Loader.read(\"p\")
    unnamed = Loader.read(\"p\", sep: 1)
    extra = Loader.read(\"p\", headers: true, sep: 1)
    either = Loader.load(\"p\", headers: true)
    other = Loader.load(\"p\", sep: 1)
    none = Loader.load(\"p\")
    braced = Loader.load(\"p\", { headers: true })
    splat = Loader.read(\"p\", **opts)
    stringy = Loader.where(\"a.b\" => 1)
    spread = Loader.where(**opts)
    keyword = Loader.where(a: 1)
  end
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class Table\nend\nclass Loader\n  def self.read: (String path, headers: true) -> Table\n  \
             | (String path) -> Array[String]\n  \
             def self.load: (String path, headers: true | Symbol, **untyped options) -> Table\n  \
             | (String path, ?Hash[Symbol, untyped] options) -> Array[Array[String]]\n  \
             def self.where: () -> untyped | (untyped, *untyped) -> Table | (**untyped) -> Table\n\
             end\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def self.read(path, **options) -> Array = [path]
  def self.load(path, options = {}) -> Array = [path]
  def run(opts) -> Table
    keyed: Table = Loader.read(\"p\", headers: true)
    plain: Array[String] = Loader.read(\"p\")
    unnamed: Array = Loader.read(\"p\", sep: 1)
    extra: Array = Loader.read(\"p\", headers: true, sep: 1)
    either: Table | Array = Loader.load(\"p\", headers: true)
    other: Array[Array] = Loader.load(\"p\", sep: 1)
    none: Array[Array] = Loader.load(\"p\")
    braced: Array[Array] = Loader.load(\"p\", { headers: true })
    splat: Table | Array = Loader.read(\"p\", **opts)
    stringy: Table = Loader.where(\"a.b\" => 1)
    keyword: Table = Loader.where(a: 1)"
        );
    }

    #[test]
    fn a_union_a_signature_declares_is_answered_member_by_member() {
        // `Return::Union`: each member resolves and the answers join, `nil` riding as
        // the mark. A call on the union runs on each class that has the member, and a member that
        // names no class (`_Nothing`) refuses the whole union, so `vague` and the `run` it ends
        // draw nothing. A union whose members all inherit one of them is drawn as that one.
        let source = "\
class Report
  def run
    either = Shelf.new.pick
    spelled = Shelf.new.pick.label
    paired = Shelf.new.pair.label
    lonely = Shelf.new.pick.size
    flagged = Shelf.new.flag
    parsed = Shelf.new.parse
    anything = Shelf.new.loose
    truthy = Shelf.new.truthy
    unknown = Shelf.new.vague
  end
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class Book\n  def label: () -> String\n  def size: () -> Integer\nend\n\
             class Map\n  def label: () -> Integer\nend\n\
             class Novel < Book\nend\n\
             class Shelf\n  def pick: () -> (Book | Map | nil)\n  def pair: () -> (Book | Map)\n  def flag: () -> (Book | false)\n  \
             def parse: () -> (Novel | Book | nil)\n  def loose: () -> (Book | Object)\n  def truthy: () -> (Book | true)\n  \
             def vague: () -> (Book | _Nothing)\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "    either: Book? | Map = Shelf.new.pick
    spelled: String | Integer = Shelf.new.pick.label
    paired: String | Integer = Shelf.new.pair.label
    lonely: Integer = Shelf.new.pick.size
    flagged: Book | false = Shelf.new.flag
    parsed: Book? = Shelf.new.parse
    anything: Book | Object = Shelf.new.loose
    truthy: Book | true = Shelf.new.truthy"
        );
    }

    #[test]
    fn a_conditional_read_as_a_value_is_whichever_branch_ran() {
        // A method's own exits already split a tail-position conditional; a conditional *assigned*
        // is one value, whichever branch ran (`Receiver::Either`), joined like exits:
        //
        // - both branches name a class: the union, and a call on it drops what lacks the member;
        // - a branch nobody wrote is `nil`, and a `case … in` without `else` raises instead;
        // - a branch that raises or jumps away hands nothing back;
        // - `a rescue b` is either side, and so is `begin … rescue … end`.
        let source = "\
class Report
  def picked(flag)
    word = flag ? \"a\" : 1
    word
  end

  def chained(flag)
    word = if flag then \"a\" else \"b\" end
    word.upcase
  end

  def unwritten(flag)
    word = (\"a\" if flag)
    word
  end

  def matched(value)
    word = case value
           when 1 then \"a\"
           when 2 then \"b\"
           end
    word
  end

  def patterned(value)
    word = case value
           in Integer then \"a\"
           in String then \"b\"
           end
    word
  end

  def jumped(flag)
    word = flag ? \"a\" : raise(\"no\")
    word
  end

  def modified
    size = (\"a\".length rescue \"none\")
    size
  end

  def begun
    size = begin
      \"a\".length
    rescue StandardError
      \"none\"
    end
    size
  end
end
";
        let (mut harness, uri) = with_types(source);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def picked(flag) -> String | Integer
  def chained(flag) -> String
  def unwritten(flag) -> String?
  def matched(value) -> String?
  def patterned(value) -> String
  def jumped(flag) -> String!
  def modified -> Integer | String
    size: Integer | String = (\"a\".length rescue \"none\")
  def begun -> Integer | String
    size: Integer | String = begin"
        );
    }

    #[test]
    fn a_rescued_exception_is_an_instance_of_the_class_the_rescue_names() {
        // `rescue A => e` binds an instance of `A` (or a subclass, which has every member `A` has),
        // and a bare `rescue => e` binds a `StandardError`, Ruby's default. A class list is their
        // union, which a call on it narrows. A splat, a module and a constant nothing defines name
        // no class, so the read refuses as before, and the method with it: each body below hands
        // back a `String` where nothing is raised, so its label stands or falls with the `rescue`.
        let source = "\
module Retriable
end

class Report
  def named
    \"ok\"
  rescue ArgumentError => error
    error.message
  end

  def bare
    \"ok\"
  rescue => error
    error.message
  end

  def listed
    \"ok\"
  rescue ArgumentError, KeyError => error
    error.message
  end

  def held
    \"ok\"
  rescue => @error
    @error.message
  end

  def splatted
    \"ok\"
  rescue *ERRORS => error
    error.message
  end

  def moduled
    \"ok\"
  rescue Retriable => error
    error.message
  end

  def undefined
    \"ok\"
  rescue Missing => error
    error.message
  end
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class StandardError\n  def message: () -> String\nend\n\n\
             class ArgumentError < StandardError\nend\n\n\
             class KeyError < StandardError\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def named -> String
  def bare -> String
  def listed -> String
  def held -> String"
        );
    }

    #[test]
    fn to_s_is_a_string_whatever_the_receiver_is() {
        // Nothing says what `value` is, but every object answers `to_s` (`Kernel#to_s`), and Ruby
        // raises where it converts with one that returns anything but a `String`. `nil.to_s` is
        // `""`, so only `&.` adds `nil`, and only where the receiver may be `nil`.
        //
        // What the code says comes first: `Money#to_s` returns an `Integer`, and its body says so.
        // `Label#to_s` says nothing, so the rule answers there. A known receiver without the member
        // answers nothing (`Plain` has none only because this fixture's `Kernel` lacks `to_s`).
        // The other conversions are not on `Object`, and overrides returning `nil` or another class
        // were found for them: no label.
        let source = "\
class Money
  def to_s = 42
end

class Label
  def to_s = missing
end

class Plain
end

class Report
  def label(value) = value.to_s
  def maybe(value) = value&.to_s
  def sure = Label.new&.to_s
  def shouted(value) = value.to_s.upcase
  def money = Money.new.to_s
  def labelled = Label.new.to_s
  def plain = Plain.new.to_s
  def count(value) = value.to_i
  def ratio(value) = value.to_f
  def rows(value) = value.to_a
  def text(value) = value.to_str
  def options(value) = value.to_hash
  def symbol(value) = value.to_sym
  def pairs(value) = value.to_h
end
";
        let (mut harness, uri) = with_rbs(source, "");
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def to_s -> Integer = 42
  def label(value) -> String = value.to_s
  def maybe(value) -> String? = value&.to_s
  def sure -> String = Label.new&.to_s
  def shouted(value) -> String = value.to_s.upcase
  def money -> Integer = Money.new.to_s
  def labelled -> String = Label.new.to_s"
        );
        // The call is a `String` by the rule, where the receiver is known and its method says
        // nothing, and so is the next link's receiver. Derived, never a guess.
        let on_the_call = card(&mut harness, &uri, source, "to_s\n  def plain");
        assert!(
            on_the_call.contains("Label#to_s -> String"),
            "{on_the_call}"
        );
        let next_link = card(&mut harness, &uri, source, "upcase\n");
        assert!(next_link.contains("String#upcase"), "{next_link}");
        assert!(!next_link.contains(GUESS_FOOTNOTE), "{next_link}");
    }

    #[test]
    fn what_ruby_s_syntax_fixes_is_typed_whatever_was_passed() {
        // `*rest`, `**rest` and `&block` hold an `Array`, a `Hash` and a `Proc` or `nil`; a
        // `*rest` target an `Array`; `defined?` a `String` or `nil`; `obj&.x = v` the value or `nil`.
        // None of it depends on what the caller passed.
        let source = "\
class Report
  def all(*items) = items
  def options(**settings) = settings
  def callback(&block) = block
  def rest(pair)
    head, *tail = pair
    tail
  end
  def known = defined?(@cache)
  def renamed(story) = (story&.name = \"x\")
  def counted(*items) = items.size
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class Proc\nend\n\nclass Array[E]\n  def size: () -> Integer\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def all(*items) -> Array = items
  def options(**settings) -> Hash = settings
  def callback(&block) -> Proc? = block
  def rest(pair) -> Array
  def known -> String? = defined?(@cache)
  def renamed(story) -> String? = (story&.name = \"x\")
  def counted(*items) -> Integer = items.size"
        );
    }

    #[test]
    fn a_method_that_hands_back_its_block_s_value_answers_it_at_each_call() {
        // `yield`, and a call of the method's own `&block`, hand back the value of the block the
        // call wrote, typed where the call is: its tail and every `next`. A `break` ends the call
        // itself with its value, so the call is its return or that value. `&:name` calls `name` on
        // what the `yield` hands over. No block, or one whose value is unknown, answers nothing,
        // and so does the `def`'s own label, which has no call.
        let source = "\
class Vault
  def with_lock
    yield
  end

  def guarded(&block) = block.call
  def handed(&block) = block.(1)

  def around(value)
    result = yield value
    result
  end
end

class Report
  def locked = Vault.new.with_lock { 42 }
  def called = Vault.new.guarded { \"x\" }
  def shorthand = Vault.new.handed { |n| \"x\" }
  def nexted(flag) = Vault.new.with_lock { next 1 if flag; \"x\" }
  def broke(flag) = [1].each { |n| break \"x\" if flag }
  def symbol = Vault.new.around(1, &:to_s)
  def nothing = Vault.new.with_lock
  def unknown = Vault.new.with_lock { missing }
end
";
        let (mut harness, uri) = with_rbs(source, "");
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def locked -> Integer = Vault.new.with_lock { 42 }
  def called -> String = Vault.new.guarded { \"x\" }
  def shorthand -> String = Vault.new.handed { |n| \"x\" }
  def shorthand = Vault.new.handed { |n: Integer| \"x\" }
  def nexted(flag) -> String | Integer = Vault.new.with_lock { next 1 if flag; \"x\" }
  def broke(flag) -> Array | String = [1].each { |n| break \"x\" if flag }
  def broke(flag) = [1].each { |n: Integer| break \"x\" if flag }
  def symbol -> String = Vault.new.around(1, &:to_s)"
        );
    }

    #[test]
    fn a_block_parameter_is_what_the_method_s_own_yields_hand_it() {
        // Where no signature says what a method yields, each block parameter is every `yield`'s
        // value at its position, read with the call's arguments bound: `nil` where a `yield` hands
        // fewer, and through `yield`s inside the method's own blocks. One value handed to a block
        // Ruby unpacks, a splat, and a method that never yields answer nothing.
        let source = "\
class Rows
  def each_row
    yield \"a\", 1
    yield \"b\", 2
  end

  def each_pair(prefix)
    yield prefix, nil
  end

  def spread
    yield [\"a\", 1]
  end

  def splatted(*values)
    yield(*values)
  end

  def never; end

  def through_each
    [\"x\"].each { |item| yield item }
  end
end

class Report
  def run
    Rows.new.each_row { |name, count, extra| name.upcase }
    Rows.new.each_pair(\"p\") { |prefix, rest| prefix }
    Rows.new.spread { |name, count| name }
    Rows.new.splatted(1) { |one| one }
    Rows.new.never { |nothing| nothing }
    Rows.new.through_each { |item| item }
    Rows.new.each_pair(\"p\") { |prefix, rest = 0, more = \"m\"| more }
  end
end
";
        let (mut harness, uri) = with_rbs(source, "");
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def never -> nil; end
  def through_each -> Array[String]
    [\"x\"].each { |item: String| yield item }
  def run -> String
    Rows.new.each_row { |name: String, count, extra| name.upcase }
    Rows.new.each_row { |name, count: Integer, extra| name.upcase }
    Rows.new.each_row { |name, count, extra: nil| name.upcase }
    Rows.new.each_pair(\"p\") { |prefix: String, rest| prefix }
    Rows.new.each_pair(\"p\") { |prefix, rest: nil| prefix }
    Rows.new.through_each { |item: String| item }
    Rows.new.each_pair(\"p\") { |prefix: String, rest = 0, more = \"m\"| more }
    Rows.new.each_pair(\"p\") { |prefix, rest: nil = 0, more = \"m\"| more }
    Rows.new.each_pair(\"p\") { |prefix, rest = 0, more: String = \"m\"| more }"
        );
        // The card is the call the `yield`s typed, and not a guess.
        let answer = card(&mut harness, &uri, source, "upcase }");
        assert!(answer.contains("String#upcase"), "{answer}");
        assert!(!answer.contains("Guessed from name alone"), "{answer}");
    }

    #[test]
    fn a_proc_literal_in_reach_is_read_for_each_call_of_it() {
        // Where every write of a local, an instance variable or a constant is a proc or lambda
        // literal, a call of it reads the literal with the call's values bound, and so does a
        // block it is passed as (`&fmt`), with what that call hands its block. A lambda refuses a
        // count it does not take; a proc fills a missing value with `nil` and cannot be read if it
        // `return`s. Two literals are either one; any other write is no literal.
        let source = "\
class Vault
  def with_lock
    yield
  end
end

class Formats
  UPPER = ->(text) { text.upcase }

  def initialize
    @count = ->(text) { text.length }
  end

  def counted = @count.call(\"abc\")
  def constant = UPPER.call(\"a\")

  def run(flag)
    upper = ->(text) { text.upcase }
    either = flag ? ->(x) { 1 } : ->(x) { \"a\" }
    loose = proc { |a, b, c| c }
    leaving = proc { |a| return 1 }
    built = make
    called = upper.call(\"a\")
    short = upper.(\"a\")
    indexed = upper[\"a\"]
    counted_wrong = upper.call(\"a\", \"b\")
    either_one = either.call(1)
    missing = loose.call(\"x\", \"y\")
    left = leaving.call(1)
    unknown = built.call(1)
    mapped = [\"a\"].map(&upper)
    locked = Vault.new.with_lock(&-> { 1 })
  end
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class Proc\nend\n\n\
             module Kernel\n  def self?.proc: () { (?) -> untyped } -> Proc\n  \
             def self?.lambda: () { () -> untyped } -> Proc\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def counted -> Integer = @count.call(\"abc\")
  def constant -> String = UPPER.call(\"a\")
  def run(flag) -> Integer
    loose: Proc = proc { |a, b, c| c }
    leaving: Proc = proc { |a| return 1 }
    called: String = upper.call(\"a\")
    short: String = upper.(\"a\")
    indexed: String = upper[\"a\"]
    either_one: Integer | String = either.call(1)
    missing: nil = loose.call(\"x\", \"y\")
    mapped: Array[String] = [\"a\"].map(&upper)
    locked: Integer = Vault.new.with_lock(&-> { 1 })"
        );
    }

    #[test]
    fn a_body_read_for_a_call_leaves_out_the_side_of_block_given_it_cannot_reach() {
        // A method that asks `block_given?`, or reads its own `&block` in a condition, hands back
        // one side to a call with a block and the other to a call without. The sides are
        // a guard's `return` and what follows it, a conditional's branches and a missing `else`,
        // and a local's conditional value. `&nil` passes no block, a lambda passes one, and a
        // `def` left with no reachable exit answers nothing.
        let source = "\
class Source
  def guarded
    return \"none\" unless block_given?
    yield 1
  end

  def tail(value)
    if block_given?
      yield value
    else
      value
    end
  end

  def ternary
    block_given? ? yield : 1.0
  end

  def by_parameter(&block)
    return 1.0 unless block
    block.call
  end

  def modifier
    yield 1 if block_given?
  end

  def raising
    raise ArgumentError unless block_given?
    yield
  end

  def local
    found = !block_given? ? \"x\" : yield
    found
  end

  def both(flag)
    return [] unless flag && block_given?
    yield
  end

  def twice
    raise ArgumentError unless block_given?
    yield
  end

  def later(flag)
    return \"none\" unless block_given?
    return 1 if flag
    yield
  end

  def pick
    block_given? ? 1 : \"s\"
  end

  def self.each_one
    yield new
  end

  def empty
    if block_given?
    end
    unless block_given?
      nil
    else
      return 1
    end
    \"s\"
  end

  def either(flag)
    return [] unless block_given? || flag
    1
  end

  def paren
    (block_given?) ? 1 : \"s\"
  end

  def local_parameter(&block)
    found = block ? block.call : \"x\"
    found
  end

  def mapped
    [1].map { block_given? ? 1 : \"s\" }
  end

  def lam
    check = -> { block_given? ? 1 : \"s\" }
    check.call
  end
end

class Source
  def twice = 1
end

class Use
  def run(flag)
    with = Source.new.guarded { |x| x }
    without = Source.new.guarded
    tailed = Source.new.tail(1) { \"s\" }
    untailed = Source.new.tail(1)
    nothing = Source.new.tail(1, &nil)
    fmt = ->(v) { 1.0 }
    lambda_block = Source.new.tail(1, &fmt)
    ternary = Source.new.ternary { \"s\" }
    plain = Source.new.ternary
    parameter = Source.new.by_parameter { \"s\" }
    no_parameter = Source.new.by_parameter
    modifier = Source.new.modifier { \"s\" }
    no_modifier = Source.new.modifier
    raised = Source.new.raising { 1 }
    unraised = Source.new.raising
    local = Source.new.local { 1 }
    no_local = Source.new.local
    both = Source.new.both(flag) { 1 }
    neither = Source.new.both(flag)
    once = Source.new.twice
    later = Source.new.later(flag) { 1.0 }
    no_later = Source.new.later(flag)
    picked = Source.each_one(&:pick)
    emptied = Source.new.empty { }
    unemptied = Source.new.empty
    either = Source.new.either(flag) { }
    neither_either = Source.new.either(flag)
    paren = Source.new.paren { }
    no_paren = Source.new.paren
    local_parameter = Source.new.local_parameter { 1 }
    no_local_parameter = Source.new.local_parameter
    mapped = Source.new.mapped { }
    unmapped = Source.new.mapped
    lam = Source.new.lam { }
    no_lam = Source.new.lam
  end
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class Proc\nend\nmodule Kernel\n  def self?.block_given?: () -> bool\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def pick -> Integer | String
  def empty -> String | Integer
  def either(flag) -> Integer | Array
  def paren -> Integer | String
  def mapped -> Array
  def lam -> Integer | String
  def run(flag) -> String
    with: Integer = Source.new.guarded { |x| x }
    with = Source.new.guarded { |x: Integer| x }
    without: String = Source.new.guarded
    tailed: String = Source.new.tail(1) { \"s\" }
    untailed: Integer = Source.new.tail(1)
    nothing: Integer = Source.new.tail(1, &nil)
    lambda_block: Float = Source.new.tail(1, &fmt)
    ternary: String = Source.new.ternary { \"s\" }
    plain: Float = Source.new.ternary
    parameter: String = Source.new.by_parameter { \"s\" }
    no_parameter: Float = Source.new.by_parameter
    modifier: String = Source.new.modifier { \"s\" }
    no_modifier: nil = Source.new.modifier
    raised: Integer = Source.new.raising { 1 }
    local: Integer = Source.new.local { 1 }
    no_local: String = Source.new.local
    both: Integer | Array = Source.new.both(flag) { 1 }
    neither: Array = Source.new.both(flag)
    later: Float | Integer = Source.new.later(flag) { 1.0 }
    no_later: String = Source.new.later(flag)
    picked: String = Source.each_one(&:pick)
    emptied: Integer = Source.new.empty { }
    unemptied: String = Source.new.empty
    either: Integer = Source.new.either(flag) { }
    neither_either: Integer | Array = Source.new.either(flag)
    paren: Integer = Source.new.paren { }
    no_paren: String = Source.new.paren
    local_parameter: Integer = Source.new.local_parameter { 1 }
    no_local_parameter: String = Source.new.local_parameter
    mapped: Array[Integer] = Source.new.mapped { }
    unmapped: Array[String] = Source.new.mapped
    lam: Integer = Source.new.lam { }
    no_lam: String = Source.new.lam"
        );
    }

    #[test]
    fn a_condition_that_is_not_the_method_s_own_block_leaves_nothing_out() {
        // What is left out rests on Ruby's rules, so every other spelling leaves both sides in: a
        // `&block` written again, a block's own parameter of that name, a `block_given?` sent to
        // another object, an anonymous `&` or `...` passing on whatever the caller had, and a
        // class's own `block_given?` anywhere in the graph.
        let source = "\
class Source
  def reset(&block)
    block = nil
    return 1 unless block
    \"s\"
  end

  def shadowed(&block)
    [nil].each { |block| return 1 unless block }
    \"s\"
  end

  def asked(other)
    return 1 unless other.block_given?
    \"s\"
  end

  def pick
    block_given? ? 1 : \"s\"
  end

  def forwarded(&) = pick(&)

  def spread(...) = pick(...)

  def make
    @check = -> { block_given? ? 1 : \"s\" }
  end

  def use
    return 1.0 unless block_given?
    @check.call
  end

  def shadow_value(&block)
    return [] unless block
    [nil].map { |block| found = block ? 1 : \"s\"; found }
  end

  def passed_on(&block) = pick(&block)

  def guessed = pick(&story)
end

class Story
end

class Use
  def run
    reset = Source.new.reset { }
    shadowed = Source.new.shadowed { }
    asked = Source.new.asked(self) { }
    used = Source.new.use { }
    shadow_value = Source.new.shadow_value { }
    passed_on = Source.new.passed_on { }
    guessed = Source.new.guessed
    forwarded = Source.new.forwarded
    spread = Source.new.spread
  end
end
";
        let (mut harness, uri) = with_rbs(source, "class Proc\nend\n");
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def reset(&block) -> String | Integer
  def shadowed(&block) -> String | Integer
    [nil].each { |block: nil| return 1 unless block }
  def asked(other) -> String | Integer
  def pick -> Integer | String
  def forwarded(&) -> Integer | String = pick(&)
  def spread(...) -> Integer | String = pick(...)
  def make -> Proc
  def use -> Integer | String | Float
  def shadow_value(&block) -> Array
    [nil].map { |block: nil| found = block ? 1 : \"s\"; found }
  def passed_on(&block) -> Integer | String = pick(&block)
  def guessed -> Integer | String = pick(&story)
  def run -> Integer | String
    reset: String | Integer = Source.new.reset { }
    shadowed: String | Integer = Source.new.shadowed { }
    asked: String | Integer = Source.new.asked(self) { }
    used: Integer | String = Source.new.use { }
    shadow_value: Array = Source.new.shadow_value { }
    passed_on: Integer | String = Source.new.passed_on { }
    guessed: Integer | String = Source.new.guessed
    forwarded: Integer | String = Source.new.forwarded
    spread: Integer | String = Source.new.spread"
        );
        let owned = "\
class Source
  def pick
    block_given? ? 1 : \"s\"
  end
end

class Other
  def block_given? = true
end

class Use
  def run
    picked = Source.new.pick
  end
end
";
        let (mut harness, uri) = with_rbs(owned, "class Proc\nend\n");
        assert!(
            drawn_hints(owned, &harness.hints_in(&uri))
                .contains("picked: Integer | String = Source.new.pick"),
            "{}",
            drawn_hints(owned, &harness.hints_in(&uri))
        );
    }

    #[test]
    fn a_yield_or_a_block_call_that_cannot_be_read_answers_nothing() {
        // What a `yield` hands is unreadable where it splats or passes keywords, or where a block
        // call passes a block of its own. A block call is the call's block only on the `&block`
        // itself: a copy is an ordinary `Proc?`, and a rewritten one is what was written there.
        // Two values to a `&:name`, or one that may be `nil`, answer nothing, and a forwarded
        // anonymous `&` hands no block this text can read.
        let source = "\
class Source
  def splatted(*args)
    yield(*args)
  end

  def keyed
    yield(a: 1)
  end

  def handing(&block)
    block.call(1, &block)
  end

  def renamed(&block)
    other = block
    other.call(1)
  end

  def rewritten(&block)
    block = -> { 2 }
    block.call
  end

  def pair
    yield 1, \"a\"
  end

  def maybe(flag)
    yield(flag ? \"a\" : nil)
  end

  def forwarded(&) = pair(&)
end

class Use
  def run(flag)
    Source.new.splatted(1) { |x| x }
    Source.new.keyed { |x| x }
    Source.new.handing { |x| x }
    renamed = Source.new.renamed { 1 }
    rewritten = Source.new.rewritten { 1 }
    paired = Source.new.pair(&:to_s)
    maybe = Source.new.maybe(flag, &:upcase)
    forwarded = Source.new.forwarded { |a, b| b }
  end
end
";
        let (mut harness, uri) = with_rbs(source, "class Proc\nend\n");
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "    other: Proc? = block
  def rewritten(&block) -> Integer
    rewritten: Integer = Source.new.rewritten { 1 }"
        );
    }

    #[test]
    fn a_proc_literal_call_it_cannot_bind_answers_nothing() {
        // A call of a literal binds only a count and shape it can read: a splat, keywords, one
        // value to a proc that unpacks, a lambda with a post or keyword parameter answer nothing;
        // `&.` on a literal that may be `nil` adds `nil`. An empty lambda is `nil`, a tail `return`
        // its value, and a literal inside another reads the outer one's parameters.
        let source = "\
class Kind
  def run(flag)
    many = ->(a, b) { a }
    spread = proc { |a, b| a }
    post = ->(a, *rest, b) { b }
    keyed = ->(a, k:) { a }
    empty = -> {}
    tailed = -> { return 1 }
    splat = many.call(*[1, 2])
    words = many.call(1, k: 2)
    maybe = flag ? many : nil
    safe = maybe&.call(1, 2)
    one = spread.call([1, 2])
    posted = post.call(1, 2)
    worded = keyed.call(1, k: 2)
    nothing = empty.call
    returned = tailed.call
    outer = ->(x) { inner = ->(y) { x }; inner.call(\"a\") }
    nested = outer.call(1)
  end
end

class Signed
  def run
    own = proc { 1 }
    value = own.call
  end
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class Proc\nend\n\n\
             module Kernel\n  def self?.proc: () { (?) -> untyped } -> Proc\n  \
             def self?.lambda: () { () -> untyped } -> Proc\nend\n\n\
             class Signed\n  def proc: () { () -> untyped } -> Proc\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def run(flag) -> Integer
    spread: Proc = proc { |a, b| a }
    maybe: Proc? = flag ? many : nil
    safe: Integer? = maybe&.call(1, 2)
    nothing: nil = empty.call
    returned: Integer = tailed.call
    nested: Integer = outer.call(1)
    own: Proc = proc { 1 }"
        );
    }

    #[test]
    fn a_lambda_or_proc_call_is_the_literal_only_where_ruby_s_method_made_it() {
        // `lambda { }` and `proc { }` are calls: typed with `Kernel`'s signature and its footnote,
        // not as syntax, and a class that defines its own `proc` answers for itself, even with a
        // `Proc`: `own.call` is not `proc { 1 }`'s `Integer` (and a lambda a method returns is not
        // read, so it is nothing). A namespace's own `Proc.new { }` is not a literal either. A
        // literal's
        // `break` leaves the lambda, so it is a value of calling the lambda, never of the call
        // that made it.
        let source = "\
class Kind
  def run
    made = lambda { |x| x }
    made.arity
    counted = made.call(1)
    leaving = lambda { break 1 }
    leaving.arity
    left = leaving.call
  end
end

class Own
  def proc
    \"mine\"
  end

  def run
    own = proc { 1 }
    value = own.call
    shout = own.upcase
  end
end

class Wrapped
  def proc
    lambda { :mine }
  end

  def run
    own = proc { 1 }
    value = own.call
  end
end

class Shop
  class Proc
  end

  def run
    made = Proc.new { 1 }
    value = made.call
  end
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class Proc\n  def arity: () -> Integer\nend\n\n\
             module Kernel\n  def self?.proc: () { (?) -> untyped } -> Proc\n  \
             def self?.lambda: () { () -> untyped } -> Proc\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def run -> Integer
    made: Proc = lambda { |x| x }
    counted: Integer = made.call(1)
    leaving: Proc = lambda { break 1 }
    left: Integer = leaving.call
  def proc -> String
  def run -> String
    own: String = proc { 1 }
    shout: String = own.upcase
  def proc -> Proc
    own: Proc = proc { 1 }"
        );
        let made = card(&mut harness, &uri, source, "arity\n    counted");
        assert!(made.contains("Proc#arity"), "{made}");
        assert!(made.contains("Proc#arity -> Integer"), "{made}");
        let leaving = card(&mut harness, &uri, source, "arity\n    left");
        assert!(leaving.contains("Proc#arity"), "{leaving}");
        assert!(!leaving.contains("Integer#"), "{leaving}");
    }

    #[test]
    fn a_block_a_signature_rebinds_runs_against_what_the_signature_says() {
        // RBS writes a DSL's `self` as `[self: T]`, on a block or on a proc-typed argument. A
        // receiverless call inside such a block asks `T`, not the class body the block sits in:
        //
        // - `hook do` runs against an instance of the class it is written in (`instance`, read
        //   against the receiver, so `Widget` and not `App`);
        // - `scope`'s lambda, `guard`'s `if:` and any keyword `opts` takes bind by argument;
        // - `configure` names a class outright;
        // - a plain block inside a rebound one (`each`) changes nothing, so the outer answer
        //   stands;
        // - a block passed to a call nothing declares keeps the body's `self`, as before: the
        //   class object's own `shout`;
        // - `instance` on a receiver that is not a class object names nothing, so `self` in
        //   `each_later`'s block is refused rather than read off the file's top level;
        // - a receiver typed only by its name decides nothing: `app.configure`'s block keeps the
        //   top level's `self`, which has no `name`.
        //
        // The class object has a `shout` too, answering something else, so an answer read off the
        // body's `self` cannot pass for the right one, in the type or in the jump.
        let source = "\
class Widget < App
  def shout
    \"x\"
  end

  def self.shout
    1
  end

  hook do
    hooked = shout.upcase
    [1].each { |n| nested = shout }
  end

  scope :loud, -> { scoped = upcase }
  guard if: -> { guarded = shout }
  opts anything: -> { keyed = shout }
  unknown_dsl do
    unknown = shout
  end
end

App.new.configure do
  configured = name
end

App.new.each_later do
  later = name
end

app.configure do
  guessed = name
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class Settings\n  def name: () -> String\nend\n\n\
             class App\n  \
             def configure: () { () [self: Settings] -> void } -> void\n  \
             def self.hook: () ?{ () [self: instance] -> void } -> void\n  \
             def self.scope: (Symbol, ^() [self: String] -> untyped) -> void\n  \
             def self.guard: (?if: ^() [self: instance] -> untyped) -> void\n  \
             def self.opts: (**^() [self: instance] -> untyped) -> void\n  \
             def each_later: () { () [self: instance] -> void } -> void\n\
             end\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def shout -> String
  def self.shout -> Integer
    hooked: String = shout.upcase
    [1].each { |n: Integer| nested = shout }
    [1].each { |n| nested: String = shout }
  scope :loud, -> { scoped: String = upcase }
  guard if: -> { guarded: String = shout }
  opts anything: -> { keyed: String = shout }
    unknown: Integer = shout
  configured: String = name"
        );
        // The jump agrees: `shout` in `hook do` is the instance's.
        assert_eq!(
            jumps(&mut harness, &uri, source, "shout.upcase"),
            ["widget.rb:2:6"]
        );
    }

    /// Ruby's `send`, `__send__` and `public_send` as the signatures declare them: `send` is an
    /// alias in `Kernel` of `BasicObject`'s `__send__`.
    const SENDERS_RBS: &str = "\
class BasicObject
  def __send__: (Symbol name, *untyped, **untyped) ?{ (?) -> untyped } -> untyped
end

class Object < BasicObject
end

module Kernel
  def public_send: (Symbol name, *untyped, **untyped) ?{ (?) -> untyped } -> untyped
  alias send __send__
end
";

    #[test]
    fn a_call_sent_by_a_symbol_is_the_call_it_names() {
        // `send(:shout, 2)` is `shout(2)` on the same receiver: the same member, arity and rules.
        //
        // - `send` and `__send__` reach a private method, as a call on `self` does; `public_send`
        //   does not, so its answer is refused rather than read off a method Ruby would not call;
        // - a name only running Ruby knows (a variable), and a name nothing declares, answer
        //   nothing;
        // - on a class object the named call is the class's, `new` included;
        // - a class's own `send` is not Ruby's, and keeps its own answer.
        let source = "\
class Widget
  def shout(times)
    \"x\"
  end

  def self.build
    1
  end

  def private_one
    secret
  end

  private

  def secret
    \"s\"
  end
end

class Socket
  def send(message, flags)
    1
  end
end

widget = Widget.new
sent = widget.send(:shout, 2)
spoken = widget.__send__(:shout, 2)
said = widget.public_send(:shout, 2)
hidden = widget.send(:secret)
refused = widget.public_send(:secret)
named = widget.send(name)
missing = widget.send(:nothing)
built = Widget.send(:build)
made = Widget.send(:new)
own = Socket.new.send(:shout, 0)
";
        let (mut harness, uri) = with_rbs(source, SENDERS_RBS);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def shout(times) -> String
  def self.build -> Integer
  def private_one -> String
  def secret -> String
  def send(message, flags) -> Integer
sent: String = widget.send(:shout, 2)
spoken: String = widget.__send__(:shout, 2)
said: String = widget.public_send(:shout, 2)
hidden: String = widget.send(:secret)
built: Integer = Widget.send(:build)
made: Widget = Widget.send(:new)
own: Integer = Socket.new.send(:shout, 0)"
        );
        // The card is `send`'s, typed by the call it names, with the parameters of `__send__`,
        // which it is an alias of.
        let sent = card(&mut harness, &uri, source, "send(:shout");
        assert!(
            sent.contains("Kernel#send(name, *args, **kwargs, &block) -> String"),
            "{sent}"
        );
    }

    #[test]
    fn a_symbol_a_method_takes_as_a_name_jumps_to_that_method() {
        // `send(:shout)` names `shout` on the object the call is sent to, found as the call itself
        // was: on an instance in a `def`, on the class object straight in a class body, on the
        // instances of a class for `instance_method`. Privacy is the member's: `send` and `method`
        // reach a private method, `public_send` does not.
        let source = "\
class Widget
  def shout(times)
    \"x\"
  end

  def self.shout(times)
    1
  end

  def run
    send(:secret)
    public_send(:secret)
    method(:shout)
  end

  send(:shout, 1)

  private

  def secret
    \"s\"
  end
end

Widget.new.send(:shout, 2)
Widget.instance_method(:shout)
";
        let (mut harness, uri) = with_rbs(
            source,
            &format!(
                "{SENDERS_RBS}\nmodule Kernel\n  def method: (Symbol name) -> Method\nend\n\n\
                 class Module\n  def instance_method: (Symbol name) -> UnboundMethod\nend\n\n\
                 class Class < Module\nend\n"
            ),
        );
        let at = |harness: &mut Harness, needle: &str| jumps(harness, &uri, source, needle);
        assert_eq!(at(&mut harness, "secret)\n    public"), ["widget.rb:20:6"]);
        assert!(at(&mut harness, "secret)\n    method").is_empty());
        assert_eq!(at(&mut harness, "shout)\n  end"), ["widget.rb:2:6"]);
        assert_eq!(at(&mut harness, "shout, 1)"), ["widget.rb:6:11"]);
        assert_eq!(at(&mut harness, "shout, 2)"), ["widget.rb:2:6"]);
        assert_eq!(at(&mut harness, "shout)\n"), ["widget.rb:2:6"]);
    }

    #[test]
    fn a_method_define_method_makes_answers_what_its_block_does() {
        // `define_method(:loud) { … }` makes `Widget#loud`, which rubydex never sees: the member is
        // generated (`workspace::defines`), answers its block's value read against an instance,
        // and a call of it jumps to the name in the call.
        //
        // - `define_singleton_method` makes a class method, whose block runs on the class object;
        // - a block parameter is a parameter nothing types, so returning it answers nothing;
        // - a method made in a `private` section is private, so a written receiver reaches nothing.
        let source = "\
class Widget
  def title
    \"x\"
  end

  define_method(:loud) { title.upcase }
  define_method(:count) { |times| times }
  define_singleton_method(:build) { new }

  private

  define_method(:secret) { 1 }
end

a = Widget.new.loud
b = Widget.build
c = Widget.new.count(2)
d = Widget.new.secret
";
        let (mut harness, uri) = with_rbs(
            source,
            "class Module\n  \
             def define_method: (Symbol) { () [self: top] -> untyped } -> Symbol\n\
             end\n\n\
             class Class < Module\nend\n\n\
             module Kernel\n  \
             def define_singleton_method: (Symbol) { () [self: self] -> untyped } -> Symbol\n\
             end\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def title -> String
a: String = Widget.new.loud
b: Widget = Widget.build"
        );
        assert_eq!(
            jumps(&mut harness, &uri, source, "loud\nb"),
            ["widget.rb:6:17"]
        );
        assert_eq!(
            jumps(&mut harness, &uri, source, "build\nc"),
            ["widget.rb:8:27"]
        );
        // The symbol in the call is the method's own place, and answers it.
        let made = card(&mut harness, &uri, source, "loud) {");
        assert!(made.contains("Widget#loud"), "{made}");
    }

    #[test]
    fn a_method_object_is_the_method_it_is_bound_to() {
        // `method(:shout)` is a `Method` bound to `Widget#shout`, drawn `Method[Widget#shout]`.
        // Calling it, in any of Ruby's spellings, straight or through a local, is calling `shout`
        // on the same object, and so is passing it as a block.
        //
        // - `method` reaches a private method; `public_method` does not, and binds nothing;
        // - a name the receiver lacks binds nothing, so its call answers what `Method#call` says;
        // - two different methods in one local are no one method.
        let source = "\
class Widget
  def shout(times)
    \"x\"
  end

  def run(flag)
    bound = method(:shout)
    direct = method(:shout).call(2)
    held = bound.call(2)
    bracket = bound[2]
    dotted = bound.(2)
    mapped = [1].map(&method(:shout))
    passed = [1].map(&bound)
    hidden = method(:secret).call
    open = public_method(:shout)
    opened = open.call(2)
    refused = public_method(:secret)
    missing = method(:nothing).call
    either = flag ? method(:shout) : method(:secret)
  end

  private

  def secret
    1
  end
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "module Kernel\n  \
             def method: (Symbol name) -> Method\n  \
             def public_method: (Symbol name) -> Method\n\
             end\n\n\
             class Method\n  \
             def call: (*untyped) ?{ (?) -> untyped } -> untyped\n  \
             def []: (*untyped) -> untyped\n  \
             def arity: () -> Integer\n\
             end\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def shout(times) -> String
  def run(flag) -> Method
    bound: Method[Widget#shout] = method(:shout)
    direct: String = method(:shout).call(2)
    held: String = bound.call(2)
    bracket: String = bound[2]
    dotted: String = bound.(2)
    mapped: Array[String] = [1].map(&method(:shout))
    passed: Array[String] = [1].map(&bound)
    hidden: Integer = method(:secret).call
    open: Method[Widget#shout] = public_method(:shout)
    opened: String = open.call(2)
    refused: Method = public_method(:secret)
    either: Method = flag ? method(:shout) : method(:secret)
  def secret -> Integer"
        );
        let called = card(&mut harness, &uri, source, "call(2)\n    bracket");
        assert!(called.contains("-> String"), "{called}");
    }

    #[test]
    fn a_define_method_block_runs_against_an_instance_of_its_receiver() {
        // `define_method` makes its block or proc the body of a method of the receiver, so `self`
        // there is an instance, which RBS can only write as `[self: top]`. Read as nothing, the
        // block kept the class body's `self`, and `shout` answered the class object's `Integer`.
        //
        // - the block and a proc argument both run against the instance, written bare or on the
        //   class;
        // - `define_singleton_method`'s `[self: self]` is the receiver itself: the class object;
        // - any other `[self: T]` this cannot read refuses `self` rather than keep the body's.
        let source = "\
class Widget < App
  def shout
    \"x\"
  end

  def self.shout
    1
  end

  define_method(:loud) do
    made = shout
  end

  define_method(:quiet, -> { procd = shout })

  define_singleton_method(:louder) do
    singly = shout
  end

  odd do
    unread = shout
  end
end

Widget.define_method(:outside) do
  outside = shout
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class Module\n  \
             def define_method: (Symbol) { () [self: top] -> untyped } -> Symbol\n                   \
             | (Symbol, ^() [self: top] -> untyped | Method | UnboundMethod) -> Symbol\n\
             end\n\n\
             class Class < Module\nend\n\n\
             module Kernel\n  \
             def define_singleton_method: (Symbol) { () [self: self] -> untyped } -> Symbol\n\
             end\n\n\
             class App\n  def self.odd: () { () [self: top] -> void } -> void\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def shout -> String
  def self.shout -> Integer
    made: String = shout
  define_method(:quiet, -> { procd: String = shout })
    singly: Integer = shout
  outside: String = shout"
        );
        assert_eq!(
            jumps(
                &mut harness,
                &uri,
                source,
                "shout\n  end\n\n  define_method(:quiet"
            ),
            ["widget.rb:2:6"]
        );
    }

    #[test]
    fn rails_blocks_run_against_the_record_or_the_relation_their_macro_names() {
        // The Rails rows (`workspace/rails/blocks.rs`) through the whole pipeline: generated RBS,
        // harvested like any signature.
        //
        // - A `scope` lambda runs against the relation, and a class method the relation lacks is
        //   the model's, which the relation hands it to (`delegated`), so `digest` still answers.
        // - A callback's block and a validation's `if:` lambda run against the record.
        let (mut harness, _, _) = models_project("");
        harness.write("app/models/application_record.rb", CONCERNS);
        let source = "\
class Ledger < ApplicationRecord
  def self.digest(value)
    \"x\"
  end

  def active?
    true
  end

  scope :matching, ->(value) { hashed = digest(value) }
  validates :name, if: -> { flagged = active? }
  before_save do
    saved = active?
  end
end
";
        let uri = harness.write("app/models/ledger.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def self.digest(value) -> String
  def active? -> true
  scope :matching, ->(value) { hashed: String = digest(value) }
  validates :name, if: -> { flagged: true = active? }
    saved: true = active?"
        );
    }

    #[test]
    fn a_concerns_included_block_runs_against_each_including_class() {
        // `self` in `included do` is every class that includes the concern, so the
        // Rails rows above apply through it: a callback block runs against each one's record. Two
        // includers, one answer each, joined.
        //
        // The `scope` lambda is each one's relation too, but a class method reached *through* a
        // relation (`delegated`) is asked of one relation, so over two it answers nothing: a
        // narrower answer, never a wrong one.
        let (mut harness, _, _) = models_project("");
        harness.write("app/models/application_record.rb", CONCERNS);
        let concern = "\
module Stamped
  included do
    scope :stamped, -> { hashed = digest }
    before_save do
      saved = active?
    end
  end
end
";
        let uri = harness.write("app/models/concerns/stamped.rb", concern);
        for (file, class) in [("ledger", "Ledger"), ("account", "Account")] {
            harness.write(
                &format!("app/models/{file}.rb"),
                &format!(
                    "class {class} < ApplicationRecord\n  include Stamped\n\n  def self.digest\n    \
                     \"x\"\n  end\n\n  def active?\n    true\n  end\nend\n"
                ),
            );
        }
        harness.index();
        assert_eq!(
            drawn_hints(concern, &harness.hints_in(&uri)),
            "      saved: true = active?"
        );
        let card = card(&mut harness, &uri, concern, "active?");
        assert_eq!(card, "**2 definitions**");
        assert_eq!(
            harness.candidates_at(&uri, concern, "active?"),
            ["Account#active?", "Ledger#active?"]
        );
    }

    #[test]
    fn a_read_in_a_block_a_signature_rebinds_is_that_object_s_variable() {
        // A block or lambda straight in a class body may run on either side, so its
        // reads refused. Where the block's call says what it runs against, the read is that
        // object's variable, written by its methods: a controller's `after_action …, if: -> { @payload }`
        // and a mailer's `default to: -> { @me.email }`. A block nothing says rebinds still
        // refuses.
        let source = "\
class Widget < App
  def load
    @payload = \"x\"
  end

  guard if: -> { guarded = @payload }
  opts to: -> { sent = @payload }
  unknown_dsl do
    unknown = @payload
  end
end
";
        let (mut harness, uri) = with_rbs(
            source,
            "class App\n  \
             def self.guard: (?if: ^() [self: instance] -> untyped) -> void\n  \
             def self.opts: (**^() [self: instance] -> untyped) -> void\n\
             end\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def load -> String\n  guard if: -> { guarded: String? = @payload }\n  opts to: -> { \
             sent: String? = @payload }"
        );
        assert_eq!(
            jumps(&mut harness, &uri, source, "@payload }"),
            ["widget.rb:3:4"]
        );
        assert_eq!(
            jumps(&mut harness, &uri, source, "@payload\n  end\nend"),
            Vec::<String>::new()
        );
        // The highlight groups the lambdas' `@payload` with the instance's, as the jump does, from
        // either end; the unknown block's stays with the class body's, alone.
        let lambda = harness.agreement_map(&uri, &cursor_after(source, "guarded = @pay"));
        let write = harness.agreement_map(&uri, &cursor_after(source, "    @pay"));
        assert_eq!(lambda.replace('W', "w"), write.replace('W', "w"));
        assert_eq!(
            lambda,
            "    @payload = \"x\"\n    WWWWWWWW\n  guard if: -> { guarded = @payload }\n\
             \u{20}                          rrrrrrrr\n  opts to: -> { sent = @payload }\n\
             \u{20}                      rrrrrrrr"
        );
        assert_eq!(
            harness.agreement_map(&uri, &cursor_after(source, "unknown = @pay")),
            "    unknown = @payload\n              rrrrrrrr"
        );
    }

    #[test]
    fn a_read_in_a_concern_s_included_block_is_its_one_includer_s_class_variable() {
        // `included do` runs on the including class, so a read there is that class
        // object's variable, which its `def self.` writes. Over two includers there is no one
        // object to read.
        let (mut harness, _, _) = models_project("");
        harness.write("app/models/application_record.rb", CONCERNS);
        let concern = "\
module Registry
  included do
    held = @registry
  end
end
";
        let uri = harness.write("app/models/concerns/registry.rb", concern);
        harness.write(
            "app/models/ledger.rb",
            "class Ledger < ApplicationRecord\n  include Registry\n\n  def self.load\n    \
             @registry = \"x\"\n  end\nend\n",
        );
        harness.index();
        assert_eq!(
            drawn_hints(concern, &harness.hints_in(&uri)),
            "    held: String? = @registry"
        );
        assert_eq!(
            jumps(&mut harness, &uri, concern, "@registry"),
            ["ledger.rb:5:4"]
        );

        harness.write(
            "app/models/account.rb",
            "class Account < ApplicationRecord\n  include Registry\n\n  def self.load\n    \
             @registry = \"x\"\n  end\nend\n",
        );
        harness.index();
        assert_eq!(drawn_hints(concern, &harness.hints_in(&uri)), "null");
    }

    #[test]
    fn what_a_signature_says_its_blocks_run_against() {
        // The `[self: T]` table, read off the signature: the block's, a proc-typed positional's,
        // a keyword's and a `**rest`'s. An arm that binds nothing there does not vote. Arms that
        // disagree, and a `self` that may be `nil`, are unread: `self` is refused there.
        let mut types = Types::new();
        types.harvest(
            "file:///sig/one.rbs",
            "\
class Widget
  def agreed: () { () [self: instance] -> void } -> void
            | (Integer) { () [self: instance] -> void } -> void
            | (String) -> void
  def split: () { () [self: String] -> void } -> void
           | (Integer) { () [self: Integer] -> void } -> void
  def args: (Symbol, ^() [self: self] -> untyped, ?if: ^() [self: Widget] -> untyped, **^() [self: instance] -> untyped) -> void
  def maybe: () { () [self: String?] -> void } -> void
  def plain: () { () -> void } -> void
  alias renamed agreed
end
",
        );
        let slots = |method: &str| {
            let key = DeclarationId::from(format!("Widget#{method}()").as_str());
            types.selves.get(&key).map(|held| {
                held.iter()
                    .map(|(slot, rebound)| match rebound {
                        Rebound::Instance => format!("{slot:?}=instance"),
                        Rebound::Returned(returned) => format!("{slot:?}={}", spelled(returned)),
                        Rebound::Unread => format!("{slot:?}=unread"),
                    })
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(slots("agreed"), Some(vec!["Block=instance".to_owned()]));
        // An alias runs its blocks against the same `self`.
        assert_eq!(slots("renamed"), slots("agreed"));
        assert_eq!(slots("split"), Some(vec!["Block=unread".to_owned()]));
        assert_eq!(
            slots("args"),
            Some(vec![
                "Positional(1)=self".to_owned(),
                "Keyword(\"if\")=Widget".to_owned(),
                "AnyKeyword=instance".to_owned(),
            ])
        );
        assert_eq!(slots("maybe"), Some(vec!["Block=unread".to_owned()]));
        assert_eq!(slots("plain"), None);
        let key = DeclarationId::from("Widget#args()");
        assert!(matches!(
            types.rebound(key, &cursor::BlockSlot::Keyword("anything".into())),
            Some(Rebound::Instance)
        ));
        assert_eq!(types.rebound(key, &cursor::BlockSlot::Positional(0)), None);
        assert_eq!(types.rebound(key, &cursor::BlockSlot::Block), None);
    }
}
