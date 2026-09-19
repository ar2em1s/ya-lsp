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
//! 5. **A mark is not a type.** `String?` answers `String` to every lookup, chain and completion.
//!    This is the one inexact entry, since a value that was `nil` has no `upcase`, and the margin
//!    says so. [`Typed`] holds the rule.
//!
//! # Where one signature is not trusted alone
//!
//! - **A `bool` receiver asks both halves.** ActiveSupport defines `blank?` on `TrueClass` and
//!   `FalseClass` with opposite bodies. Where the halves name two declarations, both resolve and
//!   fold, so `"x".empty?.blank?` is `bool`. [`split_bool`].
//! - **`!` is folded against its operand, never looked up.** `!x` is `false` when `x` is truthy,
//!   `true` when falsy, and `bool` when `x` does not resolve. That types `Object#blank?`'s
//!   `!!empty?`. [`negated`].
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

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use ruby_rbs::node::{
    ClassNode, MethodDefinitionKind, MethodDefinitionNode, ModuleNode, Node, Visit, parse,
};
use rubydex::{
    model::{
        declaration::{Ancestor, Declaration, Namespace},
        definitions::{Definition, Receiver as DefinitionReceiver},
        graph::Graph,
        ids::{DeclarationId, NameId, StringId, UriId},
        name::{Name, ParentScope},
        string_ref::StringRef,
    },
    query,
};

use super::{
    cursor::{self, Arity, Block, ParameterSlot, Receiver},
    environment,
    indexed::Indexed,
    locator,
    position::{ByteSpan, Rebase},
    scopes, views,
};
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
}

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
        }
    }

    fn is_empty(&self) -> bool {
        self.by_arity.iter().all(Option::is_none) && self.beyond.is_none() && self.any.is_none()
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

/// How [`Return::Element`] is spelled in the RBS ya-lsp generates.
///
/// - **A made-up class name, because RBS has no keyword for it.** `self` is the receiver and
///   `instance` is the declaring class; the query interface needs neither.
/// - **It never reaches the graph.** Nothing declares this class, so a lookup that escaped
///   [`class_of`] answers `None` and stops the chain.
/// - **It never reaches a reader.** [`hints`](super::hints) draws a `def`'s return only for a
///   `Return::Class` the graph holds a class or module for, and this is neither.
pub const ELEMENT: &str = "ActiveRecordElement";

/// How [`Return::Collection`] is spelled. See [`ELEMENT`].
pub const COLLECTION: &str = "ActiveRecordCollection";

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
        }
    }

    /// The owner as this arm's **return type** sees it: [`Self::under`] plus the one variable a
    /// block can answer.
    ///
    /// Kept apart from `under` because the block's parameters and a tuple use the same owner and
    /// must not see this variable. Otherwise `{ (U) -> U }` would hand the block's parameter the
    /// block's own answer, a circle.
    fn returning(&self, method_type: &ruby_rbs::node::MethodTypeNode<'_>) -> Self {
        Self {
            name: self.name,
            parameters: self.parameters,
            block_returns: block_variable(method_type),
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
            // **No emptiness check here, on purpose.** Arms this document refuses are still
            // recorded: they are part of what the partitions must agree on once another document's
            // arms are read. [`Types::insert`] drops the row if the union settles to nothing.
            let (plain, with_block) = declared_return(node, &owner);
            self.types.insert(self.document, &member, plain, with_block);
        }
    }
}

impl Harvest<'_> {
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
    written
        .iter()
        .map(|argument| class_of(&argument, owner).map(|returned| returned.of))
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
fn declared_return(node: &MethodDefinitionNode<'_>, owner: &Owner<'_>) -> (Vec<Arm>, Vec<Arm>) {
    let mut plain: Vec<Arm> = Vec::new();
    let mut with_block: Vec<Arm> = Vec::new();

    for method_type in method_types(node) {
        let arm = arm_of(&method_type, &owner.under(&method_type));

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
fn arm_of(method_type: &ruby_rbs::node::MethodTypeNode<'_>, owner: &Owner<'_>) -> Arm {
    let Node::FunctionType(function) = method_type.type_() else {
        return Arm {
            least: 0,
            most: None,
            returns: None,
            takes: Vec::new(),
        };
    };
    // Trailing positionals (`(?String, Integer)`) are required, like leading ones. RBS lists them
    // separately only to mark where the optional ones went.
    let least = function.required_positionals().iter().count()
        + function.trailing_positionals().iter().count();
    // The return type is the one position a block's answer can be substituted into, so only it is
    // read with that owner. See [`Owner::returning`].
    let owner = owner.returning(method_type);
    let fixed = function.rest_positionals().is_none()
        && function.optional_positionals().iter().next().is_none();
    Arm {
        least,
        most: (function.rest_positionals().is_none())
            .then(|| least + function.optional_positionals().iter().count()),
        returns: class_of(&function.return_type(), &owner),
        // Read against the method type's own owner, not the returning one. A parameter written
        // `self` still means the receiver, and the block substitution has no bearing on what goes
        // in.
        takes: if fixed {
            function
                .required_positionals()
                .iter()
                .chain(function.trailing_positionals().iter())
                .map(|parameter| match parameter {
                    Node::FunctionParam(parameter) => class_of(&parameter.type_(), &owner),
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        },
    }
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
    Arms {
        by_arity: widest.map_or_else(Vec::new, |widest| {
            (0..=widest)
                .map(|written| agreed(arms.iter().filter(|arm| arm.accepts(written))))
                .collect()
        }),
        beyond: agreed(arms.iter().filter(|arm| arm.most.is_none())),
        any: agreed(arms.iter()),
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
            (owner.block_returns.as_deref() == Some(written))
                .then(|| Returned::plain(Return::Block))
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

/// A written-out union, folded if it is one of the two this module has a word for.
///
/// - **Both spellings mean the same thing.** `String | nil` is `String?` and `true | false` is
///   `bool`. RBS uses both (`File#size?` is `Integer?`, `Comparable#==` is `bool`), and so does a
///   project's own `sig/`. Reading only the keyword would answer one spelling and drop the other.
/// - **Two classes left after the fold is a real union, and is dropped.** These are names, not
///   declarations, so a reader cannot be shown them and a chain cannot step on them. A union a
///   reader sees comes from `body_return`, where every arm is a resolved Ruby exit.
fn union_of(union: &ruby_rbs::node::UnionTypeNode<'_>, owner: &Owner<'_>) -> Option<Returned> {
    let mut nilable = false;
    let (mut truth, mut falsehood) = (false, false);
    let mut classes: Vec<Return> = Vec::new();
    for written in union.types().iter() {
        match &written {
            Node::NilType(_) => nilable = true,
            // `true` and `false` are **literal types** in RBS, parsed like `1` or `"x"`, not the
            // classes behind them. So neither arrives as a name, and neither reaches `class_of`.
            Node::LiteralType(literal) => match literal.literal() {
                Node::Bool(value) if value.value() => truth = true,
                Node::Bool(_) => falsehood = true,
                // Any other literal (`1`, `:sym`, `"x"`) is not a class.
                _ => return None,
            },
            _ => {
                let seen = class_of(&written, owner)?;
                nilable |= seen.nilable;
                if !classes.contains(&seen.of) {
                    classes.push(seen.of);
                }
            }
        }
    }
    let folded = match (classes.len(), truth, falsehood) {
        (0, true, true) => Return::Bool,
        (1, false, false) => classes.pop()?,
        // `String | true` and `String | Integer` alike: more than one class, and this side cannot
        // carry that.
        _ => return None,
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
    /// Uses the hoisted walk when there is one for *this* document, and a fresh walk otherwise. The
    /// answer is the same; only the cost differs.
    #[must_use]
    pub fn scope_at(&self, uri_id: UriId, offset: u32) -> Scope {
        match self.walked {
            Some(walked) => walked.of(self.graph, uri_id).at(offset),
            None => Scope::at(self.graph, uri_id, offset),
        }
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
    pub read: &'a dyn Fn(&str) -> Option<(String, Rebase)>,
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
    /// - **`rails::camelize` and `rails::element_of` are not gated.** Singularising a directory
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
    /// The documents a scope has already been asked of, for callers that ask about many offsets.
    ///
    /// - **`None` is the ordinary case**, and costs a walk in one arm only:
    ///   [`Receiver::SelfObject`], which places an offset that is not the cursor's.
    /// - **`inlayHint` hands its own memo down.** It has a hint per binding and would pay that walk
    ///   per hint, the quadratic [`Scope::bodies`] avoids.
    /// - **A memo, not one walk.** The body rung reads a `def` from whichever document declares it,
    ///   so the hinted document is the first of many.
    ///
    /// Read through [`Sources::scope_at`], never directly.
    pub walked: Option<&'a Walked<'a>>,
    /// How many constant assignments have been followed to get here.
    ///
    /// - **A cycle guard for `A = B.new` beside `B = A.new`.** The per-request bulkhead catches
    ///   panics, but a stack overflow aborts the process. [`assigned_to`] is the rung that crosses
    ///   documents and can come back to where it started.
    /// - **Zero at every call site but one.** `assigned_to` hands the rungs below it a raised copy,
    ///   so this is a field of a `Copy` struct, not a shared counter.
    pub constant_hops: u8,
    /// How many **ancestors' documents** have been crossed to get here.
    ///
    /// [`Self::constant_hops`]' shape and reason, for the other rung that can loop. `@a = @b` in a
    /// superclass whose file never writes `@b` sends [`from_ancestor`] up *its* ancestry, and each
    /// level is up to `ANCESTOR_DOCUMENTS` documents, so an unbounded version multiplies. A stack
    /// overflow aborts the process.
    pub ancestor_hops: u8,
    /// How many method **bodies** have been read to get here: the chain's *depth*.
    ///
    /// `cursor::MAX_WIDTH` bounds a receiver chain's width. This bounds how far a rung may nest
    /// *inside* the definitions the chain lands on: one body read is depth 1, and a body whose exit
    /// needs another body is depth 2.
    ///
    /// Raised like [`Self::constant_hops`], by handing a copy down, and bounded for the same
    /// reason: `def a; b; end` beside `def b; a; end` is legal Ruby, and a stack overflow aborts
    /// the process.
    pub body_hops: u8,
    /// Documents [`body_return`] has already read, for the length of one request.
    ///
    /// - **[`Self::walked`]'s shape, one rung down.** `None` is the ordinary case and costs a read
    ///   per `def` asked about. `inlayHint` asks once per `def` in the file, so it hands its own
    ///   memo down instead of re-reading and re-parsing the document every time.
    /// - **Per request, never held longer.** It caches a *buffer* and the [`Rebase`] mapping it
    ///   onto the graph, and an edit moves both. A longer-lived memo would answer from text the
    ///   user has since changed.
    pub read_bodies: Option<&'a ReadBodies>,
    /// The exits every document has been walked for, **across** requests.
    ///
    /// The split with [`Self::read_bodies`]: that memo holds a buffer and its [`Rebase`], so it
    /// dies with the request. This one holds only what the text decides, which is most of a read's
    /// cost. See [`HeldExits`].
    ///
    /// Set for every request, because a single cursor pays the same walk of the same unchanged gem
    /// file that `inlayHint` does.
    pub held_exits: Option<&'a HeldExits>,
}

/// Every `def` in one document and what its body returns, keyed by the `def`'s span.
///
/// Named because two owners hold it: a [`Document`] for one request, and [`HeldExits`] across
/// requests.
type Returns = HashMap<(u32, u32), Vec<Receiver>>;

/// One document as [`body_return`] needs it: what each `def` returns, and the map from its text
/// onto the graph.
///
/// Both halves come from one [`Sources::read`] call, so the exits and the offsets they are keyed by
/// describe the same string.
struct Document {
    /// Every `def`'s exits, by the span rubydex filed the method under.
    ///
    /// Shared because [`HeldExits`] hands the same map to every request that reads this text. The
    /// walk that builds it is the expensive half of a read, and depends only on the text.
    returns: Rc<Returns>,
    /// The text the exits were read from.
    ///
    /// Kept after the walk because a card's footnote names the **line** a body was read at, and
    /// which span is asked about is only known when a caller asks.
    source: String,
    /// That text's map onto the graph's coordinates.
    rebase: Rebase,
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
    /// A fresh memo, for a caller about to ask about many methods.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One document, read and walked. **The only place a [`Document`] is made**, so the cached and
    /// uncached paths cannot drift apart.
    ///
    /// `held` is the walk's own cache and is asked before `cursor::returns_in` is. See
    /// [`HeldExits`] for why it outlives the request and this memo does not.
    fn read(
        uri: &str,
        read: &dyn Fn(&str) -> Option<(String, Rebase)>,
        held: Option<&HeldExits>,
    ) -> Option<Rc<Document>> {
        let (source, rebase) = read(uri)?;
        let returns = match held {
            Some(held) => held.of(uri, &source),
            None => Rc::new(cursor::returns_in(&source)),
        };
        Some(Rc::new(Document {
            returns,
            source,
            rebase,
        }))
    }

    /// The same, answered from the memo where this request has asked already.
    fn of(
        &self,
        uri: &str,
        read: &dyn Fn(&str) -> Option<(String, Rebase)>,
        held: Option<&HeldExits>,
    ) -> Option<Rc<Document>> {
        if let Some(found) = self.documents.borrow().get(uri) {
            return found.clone();
        }
        let made = Self::read(uri, read, held);
        self.documents
            .borrow_mut()
            .insert(uri.to_owned(), made.clone());
        made
    }
}

/// How many documents' exits [`HeldExits`] keeps before it drops them all.
///
/// - **Headroom, not a working size.** A whole-file `inlayHint` on a real Rails model reaches tens
///   of documents, not hundreds. The bound is there so a *session* cannot grow without limit.
/// - **Drop everything, not the oldest.** An eviction order is one more thing to get wrong. A
///   dropped entry costs one re-parse, so being wrong here means a slow request, never a wrong
///   answer.
const HELD_DOCUMENTS: usize = 512;

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
    held: RefCell<HashMap<String, (u64, Rc<Returns>)>>,
}

impl HeldExits {
    /// A fresh cache: the one thing that outlives a request.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The exits of `source`, walked here or reused from a previous request.
    fn of(&self, uri: &str, source: &str) -> Rc<Returns> {
        let hash = xxhash_rust::xxh3::xxh3_64(source.as_bytes());
        if let Some((held, returns)) = self.held.borrow().get(uri)
            && *held == hash
        {
            return Rc::clone(returns);
        }
        let returns = Rc::new(cursor::returns_in(source));
        let mut held = self.held.borrow_mut();
        if held.len() >= HELD_DOCUMENTS {
            held.clear();
        }
        held.insert(uri.to_owned(), (hash, Rc::clone(&returns)));
        returns
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
    /// A fresh memo, for a caller about to place many offsets.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

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
/// More than one because real chains exist: `THING = FACTORY.build` through `FACTORY = Builder.new`
/// is two. Bounded because a cycle would crash the process.
const CONSTANT_HOPS: u8 = 3;

/// How many method bodies deep one answer may read: the chain's *depth*.
///
/// 1. **Ten is headroom.** Measured over the six corpora, depths above six add no labels, and no
///    depth ever removed or changed one. The hops above six exist only to bound a cycle
///    (`def a; b; end` beside `def b; a; end`), which is not detected; an unbounded walk would
///    abort the process.
/// 2. **Ten costs what six costs.** The extra hops find nothing, and [`ReadBodies`] means they
///    re-read no document.
/// 3. **Not `cursor::MAX_WIDTH`'s twenty.** Width and depth are different questions: more width
///    buys labels, more depth does not.
/// 4. **A constant, not a setting.** A depth is not something a project has an opinion about.
const BODY_HOPS: u8 = 10;

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
/// - **A union may be shown and do nothing else.** No single class's members can be offered, and no
///   chain can step on it.
/// - **Enforced by construction.** The classes are private and [`Self::one`] is the only way to a
///   `DeclarationId`; it answers `None` for a union. Every member lookup already uses `?`, so a
///   union stops a chain and empties a completion list without any site checking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Typed {
    /// Every class the value can be, with `nil` and the boolean pair folded out. **Never empty.**
    /// In the order the answers were found; for a body, the order the exits were read, which is
    /// stable across runs.
    classes: Vec<DeclarationId>,
    pub derivation: Derivation,
    /// `nil` was one of the answers, and was folded out of [`Self::classes`].
    ///
    /// A facet, not a class, so **only a label draws the mark and every other surface reads the
    /// classes without it**. A completion on a `String?` offers `String`'s members, which is what
    /// someone typing a `.` wants. `types.md` calls this the one inexact entry; the mark makes it
    /// visible.
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
}

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

    /// Every class, for the one surface a union may reach: a label.
    #[must_use]
    pub fn classes(&self) -> &[DeclarationId] {
        &self.classes
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
            | Return::Block => {
                return None;
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
    /// The offset of the instance-variable assignment the type came from, if one did.
    pub assignment: Option<u32>,
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
    /// The class **up the receiver's own ancestry** whose file assigns an instance variable this
    /// one only reads, when that rung answered.
    ///
    /// A sibling of [`Self::renderer`], kept separate because the evidence differs. A renderer is a
    /// **path convention**: nothing in either file says `app/views/stories/` means
    /// `StoriesController`. An ancestor is the code's own `<` or `include`, linearized by rubydex.
    /// Same tier, different sentence.
    pub ancestor: Option<FromAncestor>,
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
}

/// Which of the three kinds of answer a derivation is.
///
/// - **The tier describes how a type was reached**, so it lives beside the derivation, not in
///   whichever module draws it.
/// - **Every consumer shows it**: a hover card in a footnote, a completion row in its detail.
/// - **An inlay hint refuses the bottom tier.** It is drawn unasked and has no room for a footnote.
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
    /// Which tier this answer is, read from what was followed to reach it.
    ///
    /// Destructured, not matched field by field, so a new kind of provenance cannot be added
    /// without choosing its tier. That matters: one consumer refuses to draw `Tier::Guessed` at
    /// all.
    #[must_use]
    pub fn tier(&self) -> Tier {
        let Self {
            signatures,
            assignment,
            constant,
            assigned_constant,
            body,
            renderer,
            ancestor,
            superclass,
            guess,
            named_by,
            closure,
            view,
        } = self;
        // The name rung beats everything. A chain through three signatures that *ended* at a guess
        // is a guess: the weakest rung is what the answer rests on. That is also why `hover` prints
        // this footnote last.
        if guess.is_some() {
            return Tier::Guessed;
        }
        if signatures.is_empty()
            && assignment.is_none()
            && constant.is_none()
            && assigned_constant.is_none()
            && body.is_none()
            && renderer.is_none()
            && ancestor.is_none()
            && superclass.is_none()
            && named_by.is_none()
            && closure.is_none()
            && view.is_none()
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
    /// The card names the convention, not only the class. `UserMailer` is not a controller, and a
    /// footnote saying it is would be wrong about the one thing it cites. Same rung and same tier
    /// either way.
    pub controller: bool,
    pub line: u32,
}

/// Where an instance variable that nothing in its own file writes was assigned: the class up the
/// receiver's ancestry that writes it, the file, and the line.
///
/// - **A line, not an offset**, for [`FromRenderer`]'s reason: the caller has only the text the
///   *cursor* is in.
/// - **The file is carried here, unlike `FromRenderer`.** A renderer's name is enough to find
///   `StoriesController`. An ancestor is usually a concern the reader has never opened (lobsters
///   sets `@user` in `Authenticatable`), so the file is what makes "go and look" possible.
///   [`FromAssignment`] makes the same split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromAncestor {
    pub class: String,
    pub file: String,
    pub line: u32,
}

/// Where a constant is given the object it holds: the constant, the file, and the line.
///
/// - **A line, not an offset**, for [`FromRenderer`]'s reason: the caller has only the text the
///   *cursor* is in.
/// - **The file is carried too.** The reader just hovered the constant, so its name is not news;
///   the file is. A constant assigned in an initializer is exactly where "go and look" is the point
///   of the footnote.
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
        Receiver::Constant(offset) => {
            let constant = constant_at(graph, uri_id, *offset, sources.layout)?;
            if let Some(held) = held_by(sources, constant) {
                return Some(held);
            }
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
        // for [`locator::missed`](super::locator). See `Receiver::SelfObject`.
        Receiver::SelfObject(offset) => plain(sources.scope_at(uri_id, *offset).caller(graph)?),
        // An instance, so the receiver is the class itself rather than its singleton.
        Receiver::Instance(offset) => plain(constant_at(graph, uri_id, *offset, sources.layout)?),
        // A literal's class is named, not resolved: `String` means `String` in every file.
        //
        // - **`declared` makes `[rbs]` off degrade, not break.** With no core signatures there is
        //   no such declaration, and `None` falls through to the name-based list.
        // - **What it holds rides beside the head.** `[1, 2]` is an `Array` whose element is an
        //   `Integer`, because the parser said so one level down. Read from the source, never
        //   inferred (see [`cursor::Receiver::Literal`](super::cursor::Receiver::Literal)), and
        //   resolved here because a class is only a name until the graph has one.
        Receiver::Literal { class, arguments } => Some(
            Typed::of(declared(graph, class)?, Derivation::default()).holding(
                arguments
                    .iter()
                    .map(|held| declared(graph, (*held)?))
                    .collect(),
            ),
        ),
        Receiver::Returned {
            on,
            method,
            block,
            arity,
            arguments,
        } => returned_by(sources, uri_id, on, method, *arity, block, arguments, scope),
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
            } => returned_element(
                sources,
                uri_id,
                on,
                method,
                *arity,
                block.written(),
                scope,
                *index,
            ),
            _ => None,
        },
        // The same declaration from the other end: what the block was handed, not what the call
        // returned.
        Receiver::Yielded { on, method, index } => {
            yielded_by(sources, uri_id, on, method, *index, scope)
        }
        // An instance variable is what its assignment was, plus a note saying where. The note is
        // why the variant exists; see `Receiver::Assigned`.
        Receiver::Assigned { at, was } => {
            let mut typed = method_receiver(sources, uri_id, was, scope)?;
            typed.derivation.assignment = Some(*at);
            Some(typed)
        }
        // `super`: the same lookup as a call, with a different way of finding the member. The
        // receiver is `self` and the name is the enclosing `def`'s.
        Receiver::Super {
            at,
            method,
            block,
            arity,
        } => from_super(sources, uri_id, *at, method, *arity, *block),
        // A method parameter: the one Ruby binding no write introduces. Two rungs, in order: what a
        // signature declares, then the default value written beside it. See [`from_parameter`].
        Receiver::Parameter {
            at,
            method,
            slot,
            default,
        } => from_parameter(
            sources,
            uri_id,
            *at,
            method,
            slot,
            default.as_deref(),
            scope,
        ),
        // `a && b` is `a` or `b`, never a third thing. Which one depends on whether `a` is falsy, a
        // property of `a`'s class, so it is evaluated here, not approximated in `cursor`. See
        // [`shortcut`].
        Receiver::Shortcut { left, right, and } => {
            shortcut(sources, uri_id, left, right, *and, scope)
        }
        // `!x` is `true` or `false`. Which one depends on whether `x` is falsy, the same property
        // [`shortcut`] reads, so it is evaluated in the same place. See [`negated`].
        Receiver::Negated(on) => negated(sources, uri_id, on, scope),
        // The two rungs below the graph, in the only safe order: a convention that can be checked,
        // then a guess that cannot.
        Receiver::Named(name) => named(sources, uri_id, name, scope),
        // The fall-through. The `or_else` is its safety: the assignment is asked first and the
        // spelling only when it answered nothing, so a guess never displaces a chain that resolves.
        // No new rung: `named` is the same pair a bare name reaches, so the label matches and
        // `[types] guess_from_names = false` turns it off.
        Receiver::Spelled { was, name } => method_receiver(sources, uri_id, was, scope)
            .or_else(|| named(sources, uri_id, name, scope)),
        // `::Foo.bar` reaches here as an ordinary constant; a bare `::` never does.
        Receiver::TopLevel | Receiver::Unknown => None,
    }
}

/// One link of a chain: what the call was written on, and what RBS says it returns.
///
/// The lookup happens *after* rubydex has found the member. `[].tap` is owned by `Kernel`, not
/// `Array`, so asking the table for `Array#tap()` would miss. The ancestor walk is rubydex's; this
/// only reads the declaration it found.
// Eight arguments, as [`returned_element`] takes: one *call* as the text wrote it (receiver, name,
// block, count, arguments), which is already the cursor's shape. A struct here would be unpacked
// again one line later in both.
#[allow(clippy::too_many_arguments)]
fn returned_by(
    sources: &Sources<'_>,
    uri_id: UriId,
    on: &Receiver,
    method: &str,
    arity: Arity,
    block: &Block,
    written: &[Receiver],
    scope: &Scope,
) -> Option<Typed> {
    let graph = sources.graph;
    let owner = method_receiver(sources, uri_id, on, scope)?;
    // rubydex keys members with parentheses on; see `core-invariants.md`.
    let member = format!("{method}()");
    // **Not `?`**: the rung below does not need the member to exist. `Class#new` is declared only
    // by Ruby's own signatures, so with `[rbs]` off there is no `new` to find. And `new` meaning
    // *an instance of this class* is Ruby's rule, not a signature's.
    let found =
        query::find_member_in_ancestors(graph, owner.one()?, StringId::from(&member), false).ok();
    // **`new` sent to a class object is an instance of that class.** Ruby's rule, not an inference.
    //
    // - **The half [`cursor::instantiated`] cannot see.** It reads the text, so it resolves
    //   `Foo.new` but not `new` inside `def self.call`, `self.new`, or `new` on a local holding the
    //   class. `def self.call; new(...).call; end` is the commonest service-object entry point in
    //   Rails.
    // - **Asked only after the signature answered nothing**, so a class that declares its own
    //   `self.new` keeps it. Like `cursor::instantiated`, this bets that overriding `new` to return
    //   something else is rare; the two spellings must agree.
    // - **[`instance_of`] refuses the instance side.** A bare `new` inside `def perform` is some
    //   private method, never `Class#new`; the receiver's name has no `::<` to strip, so nothing is
    //   answered.
    if method == "new"
        && found.is_none_or(|found| {
            sources
                .types
                .returns(found, arity, block.written())
                .is_none()
        })
        && let Some(instance) = owner.one().and_then(|one| instance_of(graph, one))
    {
        return Some(Typed::of(instance, owner.derivation));
    }
    let found = found?;
    // **Read only where the signature has a slot for it**, because reading it resolves a second
    // expression. `each` is the commonest block call in Ruby and its return says nothing about the
    // block. The declared return is the gate, so a block is walked for `map` and `then`, and
    // nothing else.
    let handed = sources
        .types
        .returns(found, arity, block.written())
        .is_some_and(|returns| substitutes_a_block(&returns.of))
        .then(|| block_return(sources, uri_id, block, scope))
        .flatten();
    // **A `bool` receiver is the pair, and the halves do not always agree.** Asked before the
    // ordinary resolve, and `None` wherever both halves name the same declaration, so it costs one
    // hash lookup on calls it does not change. See [`split_bool`].
    if let Some(split) = split_bool(
        sources,
        &owner,
        &member,
        found,
        arity,
        block.written(),
        handed.as_ref(),
    ) {
        return split;
    }
    // **Only where the partition already refused.** A method the table answers pays one hash lookup
    // it was paying anyway, and no argument is typed. See [`pick_by_argument`].
    let picked = sources
        .types
        .returns(found, arity, block.written())
        .is_none()
        .then(|| {
            pick_by_argument(
                sources,
                uri_id,
                found,
                arity,
                block.written(),
                written,
                scope,
                &owner,
            )
        })
        .flatten();
    returned_from(
        sources,
        found,
        owner,
        arity,
        block.written(),
        handed.as_ref(),
        picked,
    )
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
/// - **Anything else**, including one half answering alone: a union, which [`Typed::one`] makes
///   terminal, so a chain stops instead of stepping off a guess.
///
/// Reached only where [`Typed::boolean`] is set, the unresolved `bool`. A literal `true` or `false`
/// carries its own class.
fn split_bool(
    sources: &Sources<'_>,
    owner: &Typed,
    member: &str,
    found: DeclarationId,
    arity: Arity,
    block: bool,
    handed: Option<&Typed>,
) -> Option<Option<Typed>> {
    let graph = sources.graph;
    if !owner.boolean {
        return None;
    }
    let other = declared(graph, "FalseClass")?;
    let member = StringId::from(&member.to_owned());
    let elsewhere = query::find_member_in_ancestors(graph, other, member, false).ok()?;
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
    let here = returned_from(sources, found, carrier, arity, block, handed, None)?;
    let there = returned_from(sources, elsewhere, half, arity, block, handed, None)?;

    let mut classes = here.classes.clone();
    for class in &there.classes {
        if !classes.contains(class) {
            classes.push(*class);
        }
    }
    // The weaker of the two decides the tier, as in [`body_return`] and [`shortcut`]: a value that
    // depends on which half ran is only as good as the worse half.
    let derivation = if weaker(&there.derivation, &here.derivation) {
        there.derivation
    } else {
        here.derivation
    };
    let folded = Folds::of(graph).fold(classes, here.boolean || there.boolean, derivation)?;
    Some(Some(if here.nilable || there.nilable {
        folded.or_nil()
    } else {
        folded
    }))
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
#[allow(clippy::too_many_arguments)]
fn pick_by_argument(
    sources: &Sources<'_>,
    uri_id: UriId,
    found: DeclarationId,
    arity: Arity,
    block: bool,
    written: &[Receiver],
    scope: &Scope,
    owner: &Typed,
) -> Option<Returned> {
    let Arity::Exactly(counted) = arity else {
        return None;
    };
    let counted = counted as usize;
    // A call writing nothing has nothing to pick with, and a shape list that does not match the
    // count is the "no claim" an unreadable argument list files.
    if counted == 0 || written.len() != counted {
        return None;
    }
    let reachable: Vec<&Arm> = sources
        .types
        .arms(found, block)
        .into_iter()
        .filter(|arm| arm.accepts(counted))
        .collect();
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
        classes.push(method_receiver(sources, uri_id, argument, scope)?.one()?);
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
        _ => false,
    }
}

/// What the block a call was written with returns, as one type.
///
/// [`body_return`]'s rule applied to a block, and much shorter. The exits were parsed from the
/// buffer the cursor is in, so there is no span to rebase, no document to read and no depth to
/// bound; `cursor::Finder`'s budget already bounded them.
///
/// - **Every exit must agree**, with `nil` folded out as in a body.
///   `map { |r| r.blank? ? nil : r.name }` is `String?`; `map { |r| r.admin? ? r : r.name }` is
///   nothing.
/// - **One untyped exit declines the whole block.** Half a block is not evidence.
/// - **Every exit being `nil` is an answer.** `[1, 2].map { }` really is `[nil, nil]`, and a label
///   saying so beats no label.
fn block_return(
    sources: &Sources<'_>,
    uri_id: UriId,
    block: &Block,
    scope: &Scope,
) -> Option<Typed> {
    let exits = block.exits();
    if exits.is_empty() {
        return None;
    }
    let nil = declared(sources.graph, "NilClass");
    let mut agreed: Option<Typed> = None;
    let mut nilable = false;
    for exit in exits {
        let typed = method_receiver(sources, uri_id, exit, scope)?;
        let one = typed.one()?;
        if Some(one) == nil {
            nilable = true;
            continue;
        }
        match &agreed {
            Some(held) if held.one() != Some(one) => return None,
            Some(_) => {}
            None => agreed = Some(typed),
        }
    }
    let mut held = match agreed {
        Some(typed) => typed,
        None => Typed::of(nil?, Derivation::default()),
    };
    held.nilable |= nilable;
    Some(held)
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
        Return::Collection => declared(graph, &rails::relation_of(&model_of(graph, owner.one()?)?)),
        // The receiver's own type argument. Usually unknown, but `[1, 2]` knows it (the element is
        // in the source), and so does anything a call already handed an argument to. `rows.first`
        // is nothing, exactly as `Array#first`'s `() -> E` says. [`Typed::argument`] holds the
        // safety rule.
        Return::Parameter { at, of } => owner.argument(of, *at),
        // The one answer from neither the receiver nor the signature: the block wrote it and
        // [`block_return`] resolved it. [`returned_by`] is the only caller with a block to hand
        // over; see [`Return::Block`].
        Return::Block => handed?.one(),
    }
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
            .map(|argument| resolved(sources, argument.as_ref()?, owner, handed))
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
fn returned_from(
    sources: &Sources<'_>,
    found: DeclarationId,
    owner: Typed,
    arity: Arity,
    block: bool,
    handed: Option<&Typed>,
    picked: Option<Returned>,
) -> Option<Typed> {
    let graph = sources.graph;
    let returns = match sources.types.returns(found, arity, block) {
        Some(returns) => returns,
        // The partition said nothing: either no arm takes this many arguments, or the ones that do
        // disagree. [`pick_by_argument`] answers the second case by reading what was passed. It ran
        // in [`returned_by`], where the arguments are; this is only where the refusal shows up.
        None => match picked.as_ref() {
            Some(picked) => picked,
            // **The seam.** Nothing declares what this method returns, which is true of every `def`
            // an application writes. This is where `order.customer.name` would stop; [`from_body`]
            // decides whether it does.
            None => return from_body(sources, found, owner),
        },
    };
    let declaration = resolved(sources, &returns.of, &owner, handed)?;
    let arguments = held_by_return(sources, &returns.of, &owner, handed);

    // Looked up once and read twice (the gate and the note), which keeps the `?` in one place.
    let named = graph.declarations().get(&found)?;
    // **Read before `owner` is consumed, and only when the cheap gate says so** (see
    // [`disputed_by_bodies`]). The clone copies a class list and provenance strings, which no typed
    // call should pay for.
    let body = disputed_by_bodies(graph, named)
        .then(|| from_body(sources, found, owner.clone()))
        .flatten();

    let mut derivation = owner.derivation;
    // Named by the declaration rubydex found, not by what was written: that is the signature the
    // answer came from. `[].tap` says `Kernel#tap()`, which is where a reader would go to check.
    derivation.signatures.push(named.name().to_owned());
    let declared = Typed::of(declaration, derivation)
        .faceted(returns)
        .holding(arguments);
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
/// - **`.rbs` definitions are not counted**, as in [`body_return`]: an RBS `def:` line is a
///   `Definition::Method` like a real `def`, so counting it would call a method disputed by its own
///   signature.
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
        if document.uri().ends_with(".rbs") {
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
    let mut classes = declared.classes.clone();
    for class in body.classes() {
        if !classes.contains(class) {
            classes.push(*class);
        }
    }
    // The body added nothing the signature did not say, the ordinary case when the bodies agree
    // with each other and the declaration. **The signature is returned untouched**, not rebuilt
    // around the same class, so it cannot lose its arguments or facets here.
    if classes.len() == declared.classes.len() {
        return declared;
    }
    // Destructured, not read field by field, so a new `Derivation` field cannot be silently dropped
    // on the way out of the other document ([`from_body`]'s rule).
    let Derivation {
        signatures,
        // An offset from *that* document, while every reader draws it as a line of the request's
        // document (`types.md`).
        assignment: _,
        constant,
        assigned_constant,
        body: wrote,
        renderer,
        ancestor,
        superclass,
        guess,
        named_by,
        closure,
        view,
    } = body.derivation;
    let mut derivation = declared.derivation.clone();
    derivation.signatures.extend(signatures);
    derivation.constant = derivation.constant.or(constant);
    derivation.assigned_constant = derivation.assigned_constant.or(assigned_constant);
    // **Set, not `or`ed.** The note is why the reader is shown a union at all, and the signature
    // half never has one to keep.
    derivation.body = wrote;
    derivation.renderer = derivation.renderer.or(renderer);
    derivation.ancestor = derivation.ancestor.or(ancestor);
    derivation.superclass = derivation.superclass.or(superclass);
    derivation.guess = derivation.guess.or(guess);
    derivation.named_by = derivation.named_by.or(named_by);
    derivation.closure = derivation.closure.or(closure);
    derivation.view = derivation.view.or(view);
    let Some(folded) = Folds::of(graph).fold(classes, declared.boolean || body.boolean, derivation)
    else {
        return declared;
    };
    if declared.nilable || body.nilable {
        folded.or_nil()
    } else {
        folded
    }
}

/// What a **method parameter** is: what the enclosing `def`'s signature declares, or else the
/// default value written beside it.
///
/// 1. **Finding the `def`**, as in [`from_super`]: the type of `self` where the name was written
///    names the class, and the method is looked up on it. Unlike `super`, this means *this* method,
///    so the ordinary ancestor walk is right; the nearest `def` is the one it is written in.
/// 2. **The signature wins over the default.** A signature covers every call; a default covers only
///    a call that passed nothing. Where they differ, the signature is the maintained one, so it is
///    asked first, as every rung pair in this module orders it.
/// 3. **The tier is Derived, and the footnote names the declaration**, as [`returned_by`] and
///    [`yielded_by`] do: `def show(story)` with `-> Story` cites `StoriesController#show()`. A
///    default carries no signature note; it brings whatever tier its shape earned, so a guessed
///    default stays a guess.
fn from_parameter(
    sources: &Sources<'_>,
    uri_id: UriId,
    at: u32,
    method: &str,
    slot: &ParameterSlot,
    default: Option<&Receiver>,
    scope: &Scope,
) -> Option<Typed> {
    let declared = declared_parameter(sources, uri_id, at, method, slot);
    // The default is resolved only where the signature said nothing, so it can never displace a
    // declared type.
    declared.or_else(|| method_receiver(sources, uri_id, default?, scope))
}

/// The signature half of [`from_parameter`], split out so the fall-through reads as one line.
fn declared_parameter(
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
    let found =
        query::find_member_in_ancestors(graph, here, StringId::from(&format!("{method}()")), false)
            .ok()?;
    let declared = sources.types.parameter(found, slot)?;
    // **A declared type that names every object (`Object`, `BasicObject`, `Class`, `Module`) is no
    // type, and must not displace the rungs below.** The same refusal the module makes for a
    // `Namespace::Todo`, applied to a parameter. solidus writes
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
                if let Ok(found) = query::find_member_in_ancestors(graph, *id, member, false) {
                    let owner = Typed::of(here, Derivation::default());
                    let mut typed = returned_from(sources, found, owner, arity, block, None, None)?;
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
#[allow(clippy::too_many_arguments)]
fn returned_element(
    sources: &Sources<'_>,
    uri_id: UriId,
    on: &Receiver,
    method: &str,
    arity: Arity,
    block: bool,
    scope: &Scope,
    index: u32,
) -> Option<Typed> {
    let graph = sources.graph;
    let owner = method_receiver(sources, uri_id, on, scope)?;
    let member = format!("{method}()");
    let found =
        query::find_member_in_ancestors(graph, owner.one()?, StringId::from(&member), false)
            .ok()?;
    // **A call that wrote a block is refused.** The table holds only what blockless arms agree on
    // (see [`declared_tuple`]). `IO.pipe` hands its block the tuple and returns what the block
    // returned, so answering from the blockless arm would read the wrong half of the signature.
    if block {
        return None;
    }
    // The arity is carried and deliberately unread. The tuple table is one entry per method, for
    // `Types::yields`' reason: the number of arguments does not change what the result is spread
    // into. It stays in the signature so a caller holding a chain need not know which twin of
    // [`returned_by`] it calls.
    let _ = arity;
    let declaration = declared(graph, sources.types.tupled(found, index as usize)?)?;
    let mut derivation = owner.derivation;
    derivation
        .signatures
        .push(graph.declarations().get(&found)?.name().to_owned());
    Some(Typed::of(declaration, derivation))
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
///    overridden, has several bodies and no reason to prefer one; they must agree.
/// 2. **Every exit of every body, not the last statement.** A guard-clause `return` and a final
///    chain are both resolved, and must name one declaration. This is [`settle`]'s rule for
///    overloaded arms, applied to Ruby.
/// 3. **One exit that answers nothing declines the method.** Two branches of three is not the
///    return.
/// 4. **The weakest exit decides the tier.** A body whose exit was guessed from a name is a guess,
///    as [`Derivation::tier`] says about `guess`.
/// 5. **[`Sources::body_hops`] bounds the depth at [`BODY_HOPS`].** `def a; b; end` beside
///    `def b; a; end` is legal Ruby, and an overflow aborts the process. Each round raises the copy
///    it hands down, so a cycle costs the bound, never the process.
///
/// [`Derivation::assignment`]'s offset is deliberately **not** brought back. Readers draw it
/// against the *request's* text, and this one came from another document (`types.md`).
fn from_body(sources: &Sources<'_>, found: DeclarationId, owner: Typed) -> Option<Typed> {
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
    } = body_return(sources, found)?;
    // **`self` is what the call was written on, which is what `owner` already is.** The same rule
    // [`resolved`] applies to RBS's `self`, refusal included: `owner.one()` answers nothing for a
    // union. This line makes `1.probe` an `Integer` and `"x".presence` a `String?`, not the class
    // the `def` is written in.
    let (classes, arguments) = if same {
        (vec![owner.one()?], owner.arguments.clone())
    } else {
        (classes, arguments)
    };
    // Destructured, not read field by field, so a new `Derivation` field cannot be silently dropped
    // on the way out of another document.
    let Derivation {
        signatures,
        // An offset from *that* document, while every reader draws it as a line of the request's
        // document (`types.md`).
        assignment: _,
        constant,
        assigned_constant,
        body,
        renderer,
        ancestor,
        superclass,
        guess,
        named_by,
        closure,
        view,
    } = inner;
    let mut derivation = owner.derivation;
    derivation.signatures.extend(signatures);
    derivation.constant = derivation.constant.or(constant);
    derivation.assigned_constant = derivation.assigned_constant.or(assigned_constant);
    derivation.body = body;
    derivation.renderer = derivation.renderer.or(renderer);
    derivation.ancestor = derivation.ancestor.or(ancestor);
    derivation.superclass = derivation.superclass.or(superclass);
    derivation.guess = derivation.guess.or(guess);
    derivation.named_by = derivation.named_by.or(named_by);
    derivation.closure = derivation.closure.or(closure);
    derivation.view = derivation.view.or(view);
    Some(Typed {
        classes,
        derivation,
        nilable,
        boolean,
        arguments,
        // Answered here, so it goes no further: the next link asks about the class this returns,
        // not the receiver two steps back.
        same: false,
    })
}

/// What **this `def`** returns, asked of the declaration alone.
///
/// [`from_body`] is this plus the chain that reached it. The other caller has no chain: an inlay
/// hint on a `def`'s own signature. One set of rules for both, so a margin and a card cannot
/// disagree about what a body returns.
pub fn body_return(sources: &Sources<'_>, found: DeclarationId) -> Option<Typed> {
    if sources.body_hops >= BODY_HOPS {
        return None;
    }
    let graph = sources.graph;
    let name = graph.declarations().get(&found)?.name().to_owned();
    let deeper = Sources {
        body_hops: sources.body_hops + 1,
        ..*sources
    };

    // Sorted, not in graph order, as in [`assigned_to`]: an answer must not depend on which worker
    // indexed which file first.
    let mut written: Vec<(String, UriId, u32, u32)> = locator::definitions_of(graph, found)
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
        .collect();
    written.sort();
    written.dedup();
    if written.is_empty() {
        return None;
    }

    // The three classes the fold uses, resolved once. With `[rbs]` off none is declared, so nothing
    // folds: the comparisons miss and every exit stays its own class.
    let folds = Folds::of(graph);
    let mut agreed: Option<(Vec<DeclarationId>, Derivation, String, u32)> = None;
    // Set by an exit that was *itself* a `bool`, which differs from two exits naming the two
    // halves. `def maybe; predicate; end` has one exit, and the pair is a fact about the method it
    // called.
    let mut boolean = false;
    let mut nilable = false;
    // **Whether every exit that contributed a class was a bare `self`** ([`Typed::same`]). Starts
    // true and the first other exit clears it, so a body with no exits never reaches the reader. A
    // `nil` exit does not clear it: `nil` folds into `nilable` below, and `self if present?` is the
    // shape this is for.
    let mut only_self = true;
    // What every exit agreed its class holds. **`def rows; [1, 2]; end` keeps the element**, as an
    // assignment does, so `rows.each { |n| ... }` knows what `[1, 2].each { |n| ... }` knows.
    // `None` is "no exit read yet"; an empty list is "nothing to say", which is what two
    // disagreeing exits settle on.
    let mut held: Option<Vec<Option<DeclarationId>>> = None;
    // Where the first `nil` exit was written, for a body whose *every* exit is `nil`. That method
    // answers `NilClass`, and its note must name a real line.
    let mut from_nil: Option<(Derivation, String, u32)> = None;
    for (uri, written_in, start, end) in &written {
        // Read once per document per request where the caller passed a memo, and once per ask
        // otherwise. [`Sources::read_bodies`] says which callers do which.
        let document = match sources.read_bodies {
            Some(memo) => memo.of(uri, sources.read, sources.held_exits),
            None => ReadBodies::read(uri, sources.read, sources.held_exits),
        }?;
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
        let exits = document.returns.get(&(span.start, span.end))?;
        if exits.is_empty() {
            return None;
        }
        // The `def`'s own scope, so `self` or a constant in the body resolves in the body's
        // nesting, not the caller's.
        //
        // Through `scope_at`, not `Scope::at`: `Scope::at` walks every definition in the document,
        // and this line runs once per `def` a request asks about.
        let scope = sources.scope_at(*written_in, *start);
        for exit in exits {
            // Parsed from that document's buffer; everything below keys the graph.
            let exit = exit.rebased(&document.rebase)?;
            let typed = method_receiver(&deeper, *written_in, &exit, &scope)?;
            let line = line_of(&document.source, span.start);
            // **An exit brings its whole answer, facets included.** One exit of `maybe` calls
            // `predicate`, a `bool`; reading only the carrier class would relabel the method
            // `true`. The same holds for an exit's `nil` mark, and for an exit that is a union: a
            // chain cannot step off it, but a label can show it.
            boolean |= typed.boolean;
            nilable |= typed.nilable;
            // **`nil` is folded, not treated as disagreement.** A bailing guard, a missing `else`
            // and a bare `return` each add a `nil` exit beside the real value. The `nil` becomes a
            // mark on the other answer. Where *every* exit is `nil`, there is nothing to mark and
            // the answer is `NilClass`.
            let mut brought = typed.classes().to_vec();
            if folds.nil.is_some_and(|nil| brought.contains(&nil)) {
                brought.retain(|class| Some(*class) != folds.nil);
                nilable |= !brought.is_empty();
                if brought.is_empty() {
                    if from_nil.is_none() {
                        from_nil = Some((typed.derivation, where_written(sources, uri), line));
                    }
                    continue;
                }
            }
            // Past the `nil` fold, so this exit contributes a class. A bare `self` is the one exit
            // whose answer is a question about the caller ([`Typed::same`]); any other exit makes
            // the body an ordinary answer again.
            only_self &= matches!(exit, Receiver::SelfObject(_));
            held = Some(match held {
                None => typed.arguments.clone(),
                Some(held) => agreed_arguments(held, &typed.arguments),
            });
            match &mut agreed {
                Some((classes, weakest, file, at)) => {
                    for class in brought {
                        if !classes.contains(&class) {
                            classes.push(class);
                        }
                    }
                    // The weakest exit decides the tier: a union is only as good as its worst half,
                    // and the note names that half's body.
                    if weaker(&typed.derivation, weakest) {
                        *weakest = typed.derivation;
                        *file = where_written(sources, uri);
                        *at = line;
                    }
                }
                None => {
                    agreed = Some((brought, typed.derivation, where_written(sources, uri), line));
                }
            }
        }
    }

    // Every exit was `nil`, so the method answers `NilClass` and there is nothing for a mark to sit
    // on. **An answer, not a gap** (`types.md`); it is drawn as `nil`.
    let Some((classes, mut derivation, file, line)) = agreed else {
        let (mut derivation, file, line) = from_nil?;
        derivation.body = Some(FromBody {
            method: name,
            file,
            line,
        });
        return Some(Typed::of(folds.nil?, derivation));
    };
    // The outermost body is the one a reader opens, so it replaces any inner note. At depth 1 there
    // is none to replace.
    derivation.body = Some(FromBody {
        method: name,
        file,
        line,
    });
    let folded = folds.fold(classes, boolean, derivation)?;
    // **Only where the fold left one class.** A union has no single parameter list to count a
    // position along, and [`Typed::argument`] would refuse it a step later anyway. So the list is
    // dropped rather than carried as a claim nobody checks.
    let folded = match folded.one() {
        Some(_) => folded.holding(held.unwrap_or_default()),
        None => folded,
    };
    let mut folded = if nilable || from_nil.is_some() {
        folded.or_nil()
    } else {
        folded
    };
    folded.same = only_self;
    Some(folded)
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
    let other = method_receiver(sources, uri_id, right, scope)?;
    if and && !side.falsy() || !and && !side.truthy() {
        return Some(other);
    }
    // Both branches are live. The taken side is the right operand, whole. The other side is the
    // half of the left operand that reaches the end: its `nil` and `false` for `&&`, its classes
    // and `true` for `||`.
    let mut kept = Sides::of(&other, &folds);
    if and {
        kept.nil |= side.nil;
        kept.falsehood |= side.falsehood;
    } else {
        kept.truth |= side.truth;
        for class in &side.classes {
            if !kept.classes.contains(class) {
                kept.classes.push(*class);
            }
        }
    }
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
    kept.typed(&folds, derivation, arguments)
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
    // same reason: it is the language's own rule, like `&&`.
    let Some(held) = method_receiver(sources, uri_id, on, scope) else {
        return either(Derivation::default());
    };
    let side = Sides::of(&held, &folds);
    // The operand's evidence carries over: `!` states a fact about a class, so an answer read off a
    // guessed receiver is still a guess.
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
fn yielded_by(
    sources: &Sources<'_>,
    uri_id: UriId,
    on: &Receiver,
    method: &str,
    index: usize,
    scope: &Scope,
) -> Option<Typed> {
    let graph = sources.graph;
    let owner = method_receiver(sources, uri_id, on, scope)?;
    let member = format!("{method}()");
    let found =
        query::find_member_in_ancestors(graph, owner.one()?, StringId::from(&member), false)
            .ok()?;
    let yielded = sources.types.yielded(found, index)?;
    // The same answers the return side reads, from the same functions (see [`resolved`]). What the
    // receiver holds is what `Array#each`'s `{ (E element) -> void }` asks for. A block parameter
    // that is itself a generic (`each_slice`'s `(Array[E] slice)`) carries its element into the
    // block below.
    let declaration = resolved(sources, &yielded.of, &owner, None)?;
    let arguments = held_by_return(sources, &yielded.of, &owner, None);

    let mut derivation = owner.derivation;
    derivation
        .signatures
        .push(graph.declarations().get(&found)?.name().to_owned());
    Some(
        Typed::of(declaration, derivation)
            .faceted(yielded)
            .holding(arguments),
    )
}

/// A receiver that is only a name, answered by the three rungs below the graph.
///
/// - **The order is the feature.** An answer that names a file and line is checkable; a guess from
///   letters is not. So both crossings are asked first, and the guess answers only what they leave.
///   None of the three runs until rubydex and every derivation above come back empty
///   ([`locator::resolve_typed`]), so a guess never displaces an answer the code states.
/// - **The crossings are ordered by how much they assume, weaker first.** [`from_renderer`] reads a
///   class from a *path* (a Rails convention); [`from_ancestor`] reads one from the receiver's own
///   `<` and `include` (the code). The stronger one cannot answer the weaker one's cursors anyway:
///   a template has no enclosing class, so `from_ancestor` declines every cursor `from_renderer`
///   takes.
fn named(sources: &Sources<'_>, uri_id: UriId, name: &str, scope: &Scope) -> Option<Typed> {
    if let Some(typed) = from_renderer(sources, uri_id, name) {
        return Some(typed);
    }
    if let Some(typed) = from_ancestor(sources, uri_id, name, scope) {
        return Some(typed);
    }
    if !sources.guess {
        return None;
    }
    guessed(sources.graph, name, scope)
}

/// The class a template's path names, and every document that class is written in.
///
/// Shared by [`from_renderer`] (what the assignment *is*) and [`renderer_writes`] (*where* it is
/// written). Both must agree on the class, or a card and a jump at the same `@story` would name two
/// classes.
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
    let path = DocUri::from_graph_uri(graph.documents().get(&uri_id)?.uri())?.to_file_path()?;
    // The exact name. A template whose class does not exist answers nothing, rather than reaching
    // for a similarly spelled one.
    let rendered = sources.views.rendered_by(graph, &path)?;

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
/// [`from_renderer`] is the card's half; this is the jump's. Same convention, same class, same
/// documents. A card needs the one assignment that produced a type; a jump needs all of them,
/// because `@stories = Story.where(live: true)` is a line a reader wants to land on even when this
/// crate cannot type it.
///
/// - **Every write, not only the typed ones.** [`cursor::assignments_in`] drops an assignment it
///   cannot type, which is right for a type and wrong for a place. So this reads
///   [`scopes::writes_to`] directly, which also tells a `def self.`'s same-named `@story` apart.
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

/// A template's instance variable, typed by the class its path names.
///
/// `app/views/stories/show.html.erb` is rendered by `StoriesController` and
/// `app/views/user_mailer/welcome.html.erb` by `UserMailer`, and the template reads the instance
/// variables that class assigns. A template has no enclosing class, so this is
/// [`cursor::assignments_in`] pointed at a class the file never names.
///
/// - **Only the class the path names, not its ancestors.** `@user` is often set in
///   `ApplicationController`, but it is usually assigned an ActiveRecord chain this could not type
///   anyway.
/// - **The whole controller, not the matching action.** As with any instance variable, the last
///   assignment that produced a type wins, and the provenance names its line. Walked in reverse,
///   stopping at the first the *graph* can answer, so an assignment naming an undeclared class
///   falls through to the one above.
/// - **`None` for anything but a template reading a plain `@name`.** This rung costs a second file
///   and is never reached from ordinary Ruby.
fn from_renderer(sources: &Sources<'_>, uri_id: UriId, name: &str) -> Option<Typed> {
    let graph = sources.graph;
    let (rendered, documents) = renderer_documents(sources, uri_id, name)?;

    for (uri, renderer_uri_id) in documents {
        let Some((source, rebase)) = (sources.read)(&uri) else {
            continue;
        };
        for (at, receiver) in cursor::assignments_in(&source, &rendered.name, name)
            .iter()
            .rev()
        {
            // **The renderer's map, not the template's.** This is the one rung where they are
            // different documents. `assignments_in` parsed the controller's *buffer*; everything
            // below keys the graph under `renderer_uri_id`, as last indexed. A `continue` here is
            // an assignment being typed right now, and the one above it (or the name rung) is the
            // honest answer until the edit settles.
            let (Some(at_in_graph), Some(receiver)) =
                (rebase.to_graph(*at), receiver.rebased(&rebase))
            else {
                continue;
            };
            // The scope of the *assignment*, in the controller's file: `@x = self` means the
            // controller, and a constant on the right resolves against the controller's nesting,
            // not the template's.
            let scope = Scope::at(graph, renderer_uri_id, at_in_graph);
            let Some(mut typed) = method_receiver(sources, renderer_uri_id, &receiver, &scope)
            else {
                continue;
            };
            // **The inner assignment is dropped; this rung must.** `Derivation::assignment` is an
            // offset with no document, and every consumer turns it into a line of the text the
            // *cursor* is in. Here the chain was resolved in the controller, so a nested
            // `Receiver::Assigned` (`@messages = @mod_mail.mod_mail_messages.order(:created_at)`)
            // would leave an offset into a file the card never holds, drawn as a wrong line of the
            // template. Nothing is lost: the footnote names the controller and the line of the
            // assignment that typed this variable.
            typed.derivation.assignment = None;
            typed.derivation.renderer = Some(FromRenderer {
                renderer: rendered.name.clone(),
                controller: rendered.controller,
                // `*at`, not `at_in_graph`: this names a line for a reader in the text just read,
                // the buffer. The same rule keeps `Receiver::Assigned`'s offset untranslated one
                // module over.
                line: line_of(&source, *at),
            });
            return Some(typed);
        }
    }
    None
}

/// How many of a receiver's ancestors' documents this rung reads before giving up.
///
/// Measured: across the corpora, the class that writes the variable is one step up for most reads
/// and two for nearly all the rest. rubydex's linearization is exact, so this bound is about cost
/// on the hover path, not correctness. Eight documents is past the deepest real answer.
const ANCESTOR_DOCUMENTS: usize = 8;

/// How many times one answer may cross into an ancestor's document.
///
/// - **Two, because the chain is real.** `@subject = @account` in a superclass whose file writes
///   neither needs one crossing for `@subject` and a second for `@account`.
/// - **Not more**, because nearly every answer is one step up, and each level multiplies by
///   `ANCESTOR_DOCUMENTS`. That is why this is a counter on [`Sources`], not a loop limit.
const ANCESTOR_HOPS: u8 = 2;

/// An instance variable nothing in its own file assigns, typed from the class up the chain that
/// does.
///
/// `@account` in mastodon's `ActivityPub::LikesController` is written in `AccountOwnedConcern`, two
/// `include`s away. [`Finder::type_the_instance_variable`](super::cursor) stays in one file, so
/// such a read arrives here as a bare [`Receiver::Named`]. This is [`cursor::assignments_in`]
/// pointed at the classes the code's own `<` and `include` name: [`from_renderer`]'s crossing, for
/// a different reason.
///
/// 1. **rubydex's linearization, never a re-derived one.** `Namespace::ancestors` is Ruby's order
///    (a `prepend` above the class, a later `include` ahead of an earlier one). A second walk would
///    drift from what `implementation` and the type hierarchy answer.
/// 2. **The nesting, never `self`.** [`scopes::writes_to`](super::scopes) answers for an
///    *instance*. A cursor in `def self.build` or `class << self` asks about a different `@foo`,
///    and an instance's writes would be a **wrong** answer, so the singleton side is refused.
/// 3. **Only the workspace's own code.** A gem never assigns an application's instance variables,
///    and a controller's ancestry has dozens of actionpack declarations before the project's first
///    line. One prefix test excludes both the impossible answers and the cost.
/// 4. **A guess is refused, not carried.** An ancestor's file guessing `@user` from its letters
///    guesses what the rung below guesses for free, and would attach a file citation to it. A large
///    share of ancestor reads are this, so refusing bounds the cost.
fn from_ancestor(sources: &Sources<'_>, uri_id: UriId, name: &str, scope: &Scope) -> Option<Typed> {
    let graph = sources.graph;
    // A local or a receiverless call is not an instance variable, and `assignments_in` answers only
    // about those (as in `from_renderer`).
    //
    // The third clause is the cycle guard. See `Sources::ancestor_hops`: this rung can come back to
    // the question it started from, and its recursion multiplies.
    if !name.starts_with('@') || scope.self_id.is_some() || sources.ancestor_hops >= ANCESTOR_HOPS {
        return None;
    }
    let deeper = Sources {
        ancestor_hops: sources.ancestor_hops + 1,
        ..*sources
    };
    let namespace = graph
        .declarations()
        .get(&scope.nesting_id(graph)?)
        .and_then(Declaration::as_namespace)?;

    let mut read = 0;
    for ancestor in namespace.ancestors() {
        let Ancestor::Complete(id) = ancestor else {
            continue;
        };
        let Some(class) = graph
            .declarations()
            .get(id)
            .map(|one| one.name().to_owned())
        else {
            continue;
        };
        for definition in locator::definitions_of(graph, *id) {
            let elsewhere = *definition.uri_id();
            // The cursor's own document was already walked by the arm that fell through to here,
            // and no write of this name there produced a type. Reading it again would spend a parse
            // to reach the same refusal.
            let Some(uri) = graph
                .documents()
                .get(&elsewhere)
                .map(|document| document.uri().to_owned())
                .filter(|uri| elsewhere != uri_id && sources.layout.is_own(uri))
            else {
                continue;
            };
            if read >= ANCESTOR_DOCUMENTS {
                return None;
            }
            read += 1;
            let Some((source, rebase)) = (sources.read)(&uri) else {
                continue;
            };
            // Reversed, stopping at the first the *graph* can answer (as in `from_renderer` and
            // `type_the_instance_variable`): the last assignment that produced a type wins, and one
            // naming an undeclared class falls through to the one above.
            for (at, receiver) in cursor::assignments_in(&source, &class, name).iter().rev() {
                // The **ancestor's** map, not the cursor's: the second rung where they are
                // different documents. As in `from_renderer`, a `continue` here is an assignment
                // being typed right now, and what was written above it is the honest answer until
                // it settles.
                let (Some(at_in_graph), Some(receiver)) =
                    (rebase.to_graph(*at), receiver.rebased(&rebase))
                else {
                    continue;
                };
                let written_in = Scope::at(graph, elsewhere, at_in_graph);
                let Some(mut typed) = method_receiver(&deeper, elsewhere, &receiver, &written_in)
                else {
                    continue;
                };
                if typed.derivation.tier() == Tier::Guessed {
                    continue;
                }
                // **The inner assignment is dropped**, as `from_renderer` explains:
                // `Derivation::assignment` has no document, every consumer draws it against the
                // *cursor's* text, and this chain was resolved in another file. The footnote below
                // names that file and line instead.
                typed.derivation.assignment = None;
                typed.derivation.ancestor = Some(FromAncestor {
                    class,
                    file: where_written(sources, &uri),
                    // `*at`, not `at_in_graph`: this names a line for a reader, in the text just
                    // read, the buffer.
                    line: line_of(&source, *at),
                });
                return Some(typed);
            }
        }
    }
    None
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

/// The declaration a constant written at `offset` resolves to.
///
/// Through the locator, not a re-derived constant lookup: rubydex resolved the reference under the
/// cursor against the real nesting and ancestors, which beats a name match.
#[must_use]
pub fn constant_at(
    graph: &Indexed,
    uri_id: UriId,
    offset: u32,
    layout: environment::Layout<'_>,
) -> Option<DeclarationId> {
    locator::locate(graph, uri_id, offset)
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
    if let Some(element) = rails::element_of(name) {
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
///    below, walked backwards), as for instance variables. The footnote names the file and line.
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
    // the answer does not depend on indexing order (as in `from_renderer`).
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
        // `from_renderer`: that rung searches the buffer by name and translates the result *into*
        // the graph; this one starts from a span the graph recorded and must find it in text the
        // user may have edited since. `None` is a span overlapping an unsettled edit. Falling
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
            // (the same rule as in `from_renderer`).
            line: line_of(&source, from),
        });
        return Some(typed);
    }
    None
}

/// The file `uri` names, as the shortest path that still identifies it for a reader.
///
/// - **Workspace-relative when the file is in the workspace**, the case the footnote is for: an
///   initializer eight directories down is unreadable as an absolute path.
/// - **The whole path otherwise.** That is a gem's file: long, mostly a version number, but still
///   something an editor can open, which a bare file name is not.
/// - **Never empty.** A URI this cannot parse is printed as itself, so the footnote always names a
///   place and `hover::provenance` has one sentence to render.
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
    use super::*;
    use crate::analysis::testing::*;

    /// What a signature says each constant holds, for the constants it says anything about.
    ///
    /// The refusals matter most. A constant is *one* thing, so a union names no class to offer, and
    /// `untyped` names nothing. Both fall through to the older arms, which is what keeps the rung
    /// additive.
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
        // - **A union with two classes left after the fold** (`a_wide_union`) is dropped, like
        //   `a_union`. These are names; a union a reader sees comes from resolved Ruby exits in
        //   `body_return`.
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
        // and a union are both dropped; by arity, the zero-argument side agrees with itself. A
        // fact, not a guess, like `"x".bytes.`.
        assert_eq!(
            drawn(
                "class Text\n  def round: (?half: Symbol) -> Integer\n          | (Integer digits, ?half: Symbol) -> (Integer | Float)\nend\n"
            ),
            "Text#round/0 -> Integer"
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
                assignment: Some(4),
                ..Derivation::default()
            },
            Derivation {
                renderer: Some(FromRenderer {
                    renderer: "StoriesController".to_owned(),
                    controller: true,
                    line: 9,
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

        // `Math.lgamma` is `-> [Float, -1 | 1]`: position one is a union of literals, not one
        // class. A tuple is refused **whole**, not filed with a hole, because a reader counting
        // names off the left of an `=` cannot see which position was dropped.
        assert_eq!(at("Math::<Math>#lgamma()", 0), "(none)");
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
            !left.contains("possible definitions") && !left.contains("the method name alone"),
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
                !written.contains(owned) || written.contains("the method name alone"),
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
                    && !written.contains("the method name alone"),
                "exactly, not by the name — the decoy declares `{needle}` too: {written}"
            );
        }

        // **The control, and why the check is not just `method == \"new\"`.** `self` here is the
        // instance, not the class object, so `new` is someone's private method and `instance_of`
        // answers nothing.
        let instance = card(&mut harness, &uri, source, "delta");
        assert!(
            instance.contains("possible definitions") || instance.contains("the method name alone"),
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
        // `(int, ?half: ...) -> (Integer | Float)`. Not a union either: a written digit count picks
        // the arm, and the zero-argument side agrees with itself.
        assert_eq!(answers("Float#round()"), "Integer");
        assert_eq!(called("Float#round()", 1, false), "(none)");

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
        assert_eq!(types.len(), 342, "methods typed across four core files");
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

        // Still refused, for reasons unrelated to literals: a bare `nil` is `NilClass` written as a
        // keyword, and an empty tuple or record is not one class.
        assert_eq!(answers("NilClass#to_a()"), "(none)");
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
        assert_eq!(
            types.len(),
            18,
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
        // - **`split` answers `String | Integer`.** No one class can answer a `.`, so the chain
        //   stops and completion falls to the name-based list, as for an untyped receiver. That is
        //   what [`Typed::one`] returning `None` buys, at every rung at once.
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
        // `type_the_local` goes beyond literals and `.new`.
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
        // The arm the digit count reaches is a union, which arity does not rescue.
        let digits = class_at(&mut harness, &uri, "3.7.round(1).~");
        assert!(digits.starts_with("(everything"), "{digits}");
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
    ///   answers `NilClass`, and a return type with its footnote on every card would bury the tier
    ///   line each row is there to show.
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
            markdown.contains("Matched on the method name alone"),
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
        // members, and no name `locator::missed` can print. The card would call the type unknown
        // while `completion` listed the anonymous class's members at the same byte.
        let mut harness = Harness::new();
        let source = "class Thing\n  def go\n    held = self\n    Class.new(Object) do\n      \
                      held.frob\n    end\n  end\n\n  def frob\n  end\nend\n";
        let uri = harness.write("app/thing.rb", source);
        harness.index();

        // `find` takes the first `frob`, the one inside the block.
        let markdown = card(&mut harness, &uri, source, "frob");
        assert!(markdown.contains("Thing#frob"), "{markdown}");
        assert!(
            !markdown.contains("Matched on the method name alone"),
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
        assert!(
            answered.contains(
                "*Type taken from the signature for `GREETING` — what it declares the constant \
                 holds, not what this expression says.*"
            ),
            "and the card says which constant said so: {answered}"
        );

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

    const GUESS_FOOTNOTE: &str = "Matched on the method name alone";

    /// The other half of "a constant is not the class it holds": the Ruby that assigns it.
    ///
    /// - **A signature typed `ENV` because Ruby ships one.** No one will ship a signature for an
    ///   application's own config object; the line that builds it says the same thing, in a file
    ///   far from the cursor, which is why the footnote names it.
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
        assert!(
            answered.contains(
                "*Type taken from where `Vault::CABINET` is assigned — \
                 `config/initializers/vault.rb` line 2 — and not from what this \
                 expression says.*"
            ),
            "and the card names the file and the line, because that is the one thing the \
             reader does not already have on screen: {answered}"
        );

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
        let footnote = "Type taken from where `HOLDER` is assigned";
        let settled = card(&mut harness, &main);
        assert!(settled.contains(footnote), "{settled}");

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
        assert!(
            appended.contains(footnote),
            "an edit the span does not overlap costs nothing: {appended}"
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
            !mid_rename.contains(footnote),
            "the rung answered from a span the graph no longer holds: {mid_rename}"
        );
        assert!(
            mid_rename.contains(GUESS_FOOTNOTE),
            "and the name rung answered instead: {mid_rename}"
        );
    }

    const WIRING: &str = "HOLDER = Vault::Store.new\n";

    /// A constant a **gem** assigns, where the path cannot be made relative.
    ///
    /// `where_written`'s other arm, and why it prints the whole path: long and mostly a version
    /// number, but openable in an editor, unlike a bare `shouty.rb`.
    #[test]
    fn a_constant_a_gem_assigns_is_named_by_the_path_it_is_written_at() {
        let (dir, elsewhere, env) = project_with_gem(
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
        let written = elsewhere
            .path()
            .join("gems/shouty-1.2.3/lib/shouty.rb")
            .display()
            .to_string();
        assert!(
            card.contains(&format!(
                "Type taken from where `Shouty::LOUD` is assigned — `{written}` line 7"
            )),
            "the whole path, because nothing shorter would open: {card}"
        );
    }

    /// Two constants deep, then two constants in a circle.
    ///
    /// - **Why the hop limit is not one:** `THING = FACTORY.build` over `FACTORY = Builder.new` is
    ///   ordinary wiring, and the card shows both rungs.
    /// - **Why there is a limit:** this rung can come back to the constant it started from. **The
    ///   assertion is that this test returns.** A stack overflow is not a panic the request
    ///   bulkhead can contain; it kills the process.
    #[test]
    fn a_chain_of_constant_assignments_is_followed_and_a_circle_of_them_stops() {
        let (mut harness, _uri) = with_types("class Unrelated\nend\n");
        harness.write(
            "sig/wiring.rbs",
            "class Builder\n  def build: () -> Vault::Store\nend\n",
        );
        harness.write(
            "lib/vault.rb",
            "\
class Builder
  def build; end
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
        assert!(
            chained.contains("*Type derived through `Builder#build()`"),
            "and the card shows the signature the second hop was read through: {chained}"
        );
        assert!(
            chained.contains("*Type taken from where `THING` is assigned — `lib/wiring.rb` line 2"),
            "as well as the assignment the first one was: {chained}"
        );

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
  LEFT.unlock # circle
end
";

    #[test]
    fn the_three_tiers_of_answer_drawn_side_by_side() {
        // The three tiers side by side, `GALLERY`-style. Each card is plausible alone; what must be
        // legible is the *difference*. Read the right-hand column: nothing, a signature, a line,
        // both, a guess, a name, a signature declaring what a constant holds, the Ruby that
        // assigned one, three kinds of name match, and a block scope the file does not state.
        //
        // - **`held` and `constant` are the pair to compare.** Both say a constant holds an object;
        //   one cites a signature, the other a file and line, because those are different things to
        //   check.
        // - **The bottom tier has four rows, and they differ.** Only `guessed` has nothing. The
        //   other three typed a receiver and found the member missing from it, and name the class
        //   so the claim can be checked. `audit.md` requires every footnote `hover.rs` writes to be
        //   pinned here whole; `answers.GUESSED` matches these four on their shared opening clause.
        //
        // The property under test is not the wording. It is that a reader can tell, without leaving
        // the card, which of the three things ya-lsp did.
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
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
  String#upcase
  ```
length # derived
  ```ruby
  String#length
  ```

  *Type derived through `String#upcase()` — from what those methods declare, not from this expression.*
digits # chained
  ```ruby
  Integer#digits
  ```

  *Type derived through `String#upcase()` \u{2192} `String#length()` — from what those methods declare, not from this expression.*
upcase # assigned
  ```ruby
  String#upcase
  ```

  *Type taken from the assignment on line 3, which may not be the one that ran.*
succ # both
  ```ruby
  Integer#succ
  ```

  *Type derived through `String#length()` — from what those methods declare, not from this expression.*

  *Type taken from the assignment on line 4, which may not be the one that ran.*
upcase # guessed
  ```ruby
  String#upcase
  ```

  *Matched on the method name alone — the receiver's type is unknown.*
shout # named
  ```ruby
  Person#shout
  ```

  *Type guessed from the name `person` alone — nothing in the code says so.*
upcase # constant
  ```ruby
  String#upcase
  ```

  *Type taken from the signature for `GREETING` — what it declares the constant holds, not what this expression says.*
shout # held
  ```ruby
  Person#shout
  ```

  *Type taken from where `HOLDER` is assigned — `app/report.rb` line 76 — and not from what this expression says.*
tally # missed
  ```ruby
  Counting#tally
  ```

  *Matched on the method name alone — the receiver is a `Person`, which has no such method.*
tally # class object
  ```ruby
  Counting#tally
  ```

  *Matched on the method name alone — the receiver is the class object `Person`, which has no such method.*
tally # guessed receiver
  ```ruby
  Counting#tally
  ```

  *Matched on the method name alone — the receiver was guessed from the name `@person` to be a `Person`, which has no such method.*
tally # closure
  ```ruby
  Counting#tally
  ```

  *Found on an instance of `Ledger` — `self` in a block written into a class body is the class object unless whoever takes the block re-binds it, and this name is only on an instance.*
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
            fell_through.contains("Type guessed from the name `story` alone"),
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
            !card.contains("guessed from the name"),
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
        assert!(
            guessed.contains("Type guessed from the name `story` alone"),
            "{guessed}"
        );

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
        let silenced = card(&mut off, &uri, source, "title");
        assert!(!silenced.contains("guessed from the name"), "{silenced}");
        assert!(
            silenced.contains("Matched on the method name alone"),
            "with the rung off there is nothing below the failed chain: {silenced}"
        );
    }

    #[test]
    fn a_block_parameter_is_typed_by_what_the_method_says_it_yields() {
        // The block parameter, end to end. `Story::Relation#each` is
        // `() { (Story) -> void } -> Story::Relation`. Reading only the return would drop the block
        // half, leaving `.each do |story|` to a *guess* and `.each do |instance|` (as mastodon
        // writes) with nothing.
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
        assert!(card.contains("db/schema.rb"), "{card}");
        assert!(
            !card.contains("Matched on the method name alone"),
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
        // **Every exit must agree**: the body rung's rule, over a block's branches.
        let split = class_at(
            &mut harness,
            &uri,
            "[1, 2].map { |n| if n then n.to_s else n.succ end }.each { |x| x.~ }",
        );
        assert!(split.starts_with("(everything"), "{split}");
        // A `nil` branch is folded, as in a `def`'s guard clause: the unwritten `else` returns
        // `nil`, and one class is left.
        assert_eq!(
            class_at(
                &mut harness,
                &uri,
                "[1, 2].map { |n| n.to_s if n }.each { |x| x.~ }"
            ),
            "String"
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
            assert_eq!(
                card,
                "```ruby\nclass Title\n```\n\n*Type taken from the assignment on line 6, \
                 which may not be the one that ran.*",
                "at {needle:?}"
            );
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
        assert!(
            markdown.contains("Matched on the method name alone"),
            "{markdown}"
        );
        assert!(!markdown.contains("StoriesController"), "{markdown}");
        assert!(!markdown.contains("guessed from the name"), "{markdown}");
    }

    #[test]
    fn a_guess_never_displaces_an_answer_the_code_states() {
        // The rung order: what makes the last rung safe to ship. `@user` is assigned a `Draft` in
        // its own class, and a `User` class sits in the graph ready to be guessed. If the rungs
        // were reversed, or tried in parallel, this card would say `User`.
        let mut harness = Harness::new();
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
        assert!(
            markdown.contains("Type taken from the assignment on line 3"),
            "{markdown}"
        );
        assert!(
            !markdown.contains("guessed from the name"),
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
        let view = rails_app(&harness);
        let guessed_source = "<%= @story.title %>\n";
        let elsewhere = harness.write("app/views/comments/show.html.erb", guessed_source);
        harness.index();

        let source = "<h1><%= @story.title %></h1>\n";
        let kept = card(&mut harness, &view, source, "title");
        assert!(kept.contains("`StoriesController`"), "{kept}");

        // The same variable and class, with no controller to reach it through: with the guess off,
        // nothing is left to say.
        let silenced = card(&mut harness, &elsewhere, guessed_source, "title");
        assert!(
            silenced.contains("Matched on the method name alone"),
            "{silenced}"
        );
        assert!(!silenced.contains("guessed from the name"), "{silenced}");
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
        // go-to-definition wants. `cursor::assignments_in` drops that assignment, so this reads the
        // writes themselves.
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
    fn a_partial_no_one_controller_renders_jumps_nowhere() {
        // The same refusal the card makes: `shared/_header.html.erb` names a `SharedController` no
        // file declares, and the path does not say which controller assigned this. Picking one of
        // the controllers that render the partial would be a jump the reader cannot see is wrong.
        let mut harness = Harness::new();
        rails_app(&harness);
        let source = "<%= @story.title %>\n";
        let view = harness.write("app/views/shared/_header.html.erb", source);
        harness.index();

        let found = harness.definition_at(&view, source, "@story");
        assert!(found.is_null(), "{found}");
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
        // there is no `UserMailerController`. All of mastodon's `.erb` files are mailer views.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
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
        assert!(
            markdown.contains("`UserMailer`, line 3 — the mailer Rails renders this template from"),
            "{markdown}"
        );

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
        assert!(
            overruled
                .contains("`UserMailerController`, line 3 — the controller Rails renders this"),
            "{overruled}"
        );
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
        // The convention, naming the assignment that actually typed this variable.
        assert!(
            markdown.contains(
                "`StoriesController`, line 4 — the controller Rails renders this \
                     template from"
            ),
            "{markdown}"
        );
        // And **not** a line of this file: the controller's offset is not a place here.
        assert!(
            !markdown.contains("Type taken from the assignment on line"),
            "{markdown}"
        );
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
        assert!(
            settled.contains("the receiver's type is unknown"),
            "{settled}"
        );
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
    fn a_method_parameter_is_typed_by_what_its_def_declares_and_then_by_its_default() {
        let (mut harness, uri) = with_types(PARAMETERS);
        let mut answer = |needle: &str| card(&mut harness, &uri, PARAMETERS, needle);
        let guessed = "guessed from the name";

        // **The rung itself.** `held` is bound by nothing this module collects (a parameter is not
        // a write), so without this it would reach `Receiver::Named`, camelize onto no class, and
        // answer nothing. A `sig` says what it is.
        let declared = answer("declared_call\n  end\n\n  # @param");
        assert!(declared.contains("Story#declared_call"), "{declared}");
        assert!(!declared.contains(guessed), "{declared}");
        // The footnote names the `def` the type was read off, so a reader can check it, as with
        // `returned_by` and `yielded_by`.
        assert!(declared.contains("Shelf#declared"), "{declared}");

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

        // **The second rung: the value after the `=`.** Nothing declares this one, and `Story.new`
        // states what the name holds when nobody passed anything.
        let defaulted = answer("defaulted_call\n  end");
        assert!(defaulted.contains("Story#defaulted_call"), "{defaulted}");

        // **`= nil` says nothing.** It is Ruby for *optional, type unstated*, so reading it as
        // `NilClass` would put a confident wrong class on the commonest optional parameter.
        //
        // Asserted on **this rung's fingerprint** (the footnote naming the `def`), not on the
        // absence of an answer: `optional_call` is unique in the graph, so the name-based list
        // still matches it. What must not happen is this arm claiming a receiver.
        let optional = answer("optional_call\n  end");
        assert!(!optional.contains("Shelf#optional"), "{optional}");
        assert!(!optional.contains("NilClass"), "{optional}");
        assert!(
            optional.contains("the receiver's type is unknown"),
            "{optional}"
        );

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
        assert!(
            singleton.contains("Shelf::<Shelf>#on_the_class")
                || singleton.contains("Shelf.on_the_class"),
            "{singleton}"
        );

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
        assert!(bare.contains("the receiver's type is unknown"), "{bare}");
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

    #[test]
    fn a_method_that_only_calls_super_is_typed_from_the_one_it_overrides() {
        // Ruby takes `super`'s name from the enclosing method and starts the lookup one place above
        // the class the method was found on: a walk of the linearization `from_ancestor` reads,
        // entered one step further along.
        let mut harness = Harness::new();
        let uri = super_app(
            &harness,
            "class Story < BaseStory\n  def title\n    super\n  end\nend\n",
        );
        harness.index();

        let markdown = card(&mut harness, &uri, SUPER_PROBE, "title");
        assert!(markdown.contains("Story#title -> Label"), "{markdown}");
        // Derived, and the note names the declaration the answer came from, where a reader would go
        // to check.
        assert!(
            markdown.contains("`super` here reaches `BaseStory#title()`"),
            "{markdown}"
        );
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
        // The boundary `Finder::type_the_instance_variable` states: *bounded to the file; a class
        // reopened elsewhere is a second question.* Many instance-variable reads are of a name
        // their own file never writes, and the writing class is usually one step up the receiver's
        // ancestry. This is `cursor::assignments_in` pointed at the classes the code's `<` and
        // `include` name, rather than one a path implies.
        let mut harness = Harness::new();
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
        assert!(
            markdown.contains(
                "Type taken from `ApplicationController` — `app/controllers/\
                 application_controller.rb` line 3"
            ),
            "{markdown}"
        );
        // Derived, not guessed, which is what matters: a guess is the one tier a margin never draws
        // and `implementation` declines.
        assert!(!markdown.contains("guessed from the name"), "{markdown}");
    }

    #[test]
    fn a_concern_that_assigns_it_answers_exactly_as_a_superclass_does() {
        // The commonest real shape: mastodon writes `@account` in `AccountOwnedConcern` and reads
        // it in a dozen controllers; lobsters writes `@user` in `Authenticatable`. An `include` and
        // a `<` are treated the same, because rubydex has already linearized both into one chain in
        // Ruby's order. Re-deriving that order would drift from what the type hierarchy answers.
        let mut harness = Harness::new();
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
        assert!(
            markdown.contains("Type taken from `SubjectOwned`"),
            "{markdown}"
        );
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
            "class ApplicationController\n  def load\n    @story = fetch\n  end\nend\n",
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
        assert!(markdown.contains("guessed from the name"), "{markdown}");
        assert!(
            !markdown.contains("Type taken from `ApplicationController`"),
            "{markdown}"
        );
    }

    #[test]
    fn a_name_no_ancestor_assigns_falls_through_to_the_rung_below() {
        // The chain was walked, and it never writes this variable. The same refusal `from_renderer`
        // makes for a controller that assigns nothing by that name: a class is not an answer to
        // *what is this variable*.
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
    /// **`relays` relays cost `relays + 1` crossings.** `@step0` is read here, found in `Relay0` as
    /// `@step1` (one), in `Relay1` as `@step2` (two), and so on, then in `Source` as a class (one
    /// more). `Sources::ancestor_hops` bounds this recursion. It multiplies: every level may read
    /// up to `ANCESTOR_DOCUMENTS` documents.
    fn a_relay_of(harness: &Harness, relays: usize) -> (DocUri, String) {
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
    fn a_relay_the_hop_guard_admits_is_followed_to_the_class_that_names_one() {
        // `@a = @b` in a superclass whose file never writes `@b` is a real shape, and why the guard
        // is two, not one. The positive case, so the refusal below cannot pass merely because
        // relaying never worked.
        let mut harness = Harness::new();
        let (uri, source) = a_relay_of(&harness, usize::from(ANCESTOR_HOPS) - 1);
        harness.index();

        let markdown = card(&mut harness, &uri, source.as_str(), "title");
        assert!(markdown.contains("Story#title"), "{markdown}");
        assert!(
            markdown.contains("which is above this class in its ancestry"),
            "{markdown}"
        );
    }

    #[test]
    fn a_relay_longer_than_the_hop_guard_stops_rather_than_multiplying() {
        // **The guard between this rung and a stack overflow**, which the per-request bulkhead
        // cannot contain. Each crossing may read up to `ANCESTOR_DOCUMENTS` documents, so unbounded
        // is `8^depth`, not `8 * depth`. One crossing past the bound, the rung below answers.
        let mut harness = Harness::new();
        let (uri, source) = a_relay_of(&harness, usize::from(ANCESTOR_HOPS));
        harness.index();

        let markdown = card(&mut harness, &uri, source.as_str(), "title");
        assert!(
            !markdown.contains("which is above this class in its ancestry"),
            "{markdown}"
        );
    }

    #[test]
    fn a_chain_the_bound_admits_is_walked_to_the_end_of_it() {
        // The control for the test below: the same fixture one document shorter answers, so a
        // failure to linearize plain classes would show up here instead of passing as the bound
        // working.
        let mut harness = Harness::new();
        let (uri, source) = a_chain_of(&harness, ANCESTOR_DOCUMENTS - 1);
        harness.index();

        let markdown = card(&mut harness, &uri, source.as_str(), "title");
        assert!(markdown.contains("Type taken from `Deepest`"), "{markdown}");
    }

    #[test]
    fn a_chain_of_ancestors_longer_than_the_bound_is_given_up_on_rather_than_walked() {
        // `ANCESTOR_DOCUMENTS` bounds what a hover-path rung may spend, not what it may answer.
        // Real answers are almost always one step up, so a chain silent after eight documents will
        // not answer on the ninth. Here the writing class is the ninth, and the rung below answers
        // instead of a ninth parse.
        let mut harness = Harness::new();
        let (uri, source) = a_chain_of(&harness, ANCESTOR_DOCUMENTS);
        harness.index();

        let markdown = card(&mut harness, &uri, source.as_str(), "title");
        assert!(
            !markdown.contains("which is above this class in its ancestry"),
            "{markdown}"
        );
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
        assert!(
            markdown.contains("Type guessed from the name `@story` alone"),
            "{markdown}"
        );
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
    fn the_memo_reads_a_document_once_and_answers_what_reading_it_again_would() {
        let source = "class Story\n  def title\n    \"x\"\n  end\nend\n";
        let reads = std::cell::Cell::new(0);
        let read = |_: &str| {
            reads.set(reads.get() + 1);
            Some((source.to_owned(), Rebase::identity(source.len() as u32)))
        };

        let held = ReadBodies::new();
        let first = held
            .of("file:///story.rb", &read, None)
            .expect("a document");
        let again = held
            .of("file:///story.rb", &read, None)
            .expect("a document");
        assert_eq!(reads.get(), 1, "the second ask re-read the document");
        assert!(Rc::ptr_eq(&first, &again));

        // The memoized answer equals the unmemoized one. Only `inlayHint` passes a memo, so a
        // mismatch would be a difference between surfaces, not just speed.
        let fresh = ReadBodies::read("file:///story.rb", &read, None).expect("a document");
        assert_eq!(reads.get(), 2);
        assert_eq!(first.returns, fresh.returns);
        assert_eq!(first.source, fresh.source);
        assert_eq!(first.returns.len(), 1);

        // A different document is a different read: the memo is keyed, not a single slot.
        assert!(held.of("file:///other.rb", &read, None).is_some());
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
        let held = ReadBodies::new();
        assert!(held.of("file:///gone.rb", &read, None).is_none());
        assert!(held.of("file:///gone.rb", &read, None).is_none());
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
        assert_eq!(*edited, cursor::returns_in(after));
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
        assert_eq!(*over, cursor::returns_in(source));
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

        let walked = Walked::new();
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
}
