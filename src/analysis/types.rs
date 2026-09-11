//! What a method returns, for the methods RBS says it about.
//!
//! # The table, and why it is keyed by a `DeclarationId`
//!
//! rubydex answers "which declaration is this name" and nothing else: `return_type` appears
//! nowhere in its source, so a signature's types are read at index time and dropped. Every type
//! ya-lsp knows, ya-lsp owns, and this is the one place it owns any. The table sits *beside* the
//! graph rather than inside it, which keeps the rubydex pin cheap to reverse — nothing here is a
//! rubydex type except the key.
//!
//! Keying on `(class name, method name)` would be wrong in a way that is invisible: `[].tap` is
//! owned by `Kernel`, not `Array`, so a receiver-keyed table would re-derive the ancestor walk
//! rubydex has already done. The lookup instead happens *after* rubydex has found the member,
//! keyed by the id it filed that declaration under. `DeclarationId::from("String#upcase()")` is
//! a pure hash of the name, so the harvest builds keys without a graph and the two meet at
//! lookup. rubydex's spelling is pinned by
//! `tests::rubydex_spells_a_signature_the_way_this_keys_it` against a real graph: get it wrong
//! and every lookup misses in silence.
//!
//! # What is taken and what is dropped
//!
//! An RBS return type is richer than a class name, and the table must not pretend otherwise.
//!
//! | RBS | example | table |
//! | --- | --- | --- |
//! | a class instance | `-> String` | `String` |
//! | a generic | `-> Array[Integer]` | `Array` — the erased head |
//! | optional | `-> String?` | `String` |
//! | `self` | `-> self` | whatever the call was made *on*, resolved at lookup |
//! | `instance` | `-> instance` | an instance of the owner |
//! | `class`, `singleton(Foo)` | `-> class` | the singleton class |
//! | a union, an interface, `untyped`, `void`, `bool`, a proc, a tuple, a record, a type variable, a type alias | | **dropped** |
//!
//! - **The erased head answers a different question, not an approximate one.** Method lookup on
//!   `Array[Integer]` and on `Array[String]` reaches the same declarations, so `.bytes.` offering
//!   `Array`'s members is exact. What it cannot do is follow `Array#first`, whose declared return
//!   is the type variable `E` — dropped like any other non-class, which is why a chain through
//!   `first` stops.
//! - **Optional takes the inner type**, the one inexact entry: a method that returned `nil` this
//!   time really has no `upcase`. `String`'s members are what the user is asking for, and the
//!   provenance footnote is what keeps it honest.
//! - **A union is dropped rather than merged.** A list built from two classes is half wrong with
//!   nothing saying which half; `Unknown` is honest and the name rung already handles it.
//!
//! # Overloads are a union — except where a block tells them apart
//!
//! Several arms naming several classes is a union written across lines. The commonest multi-arm
//! shape in Ruby's core is not one:
//!
//! ```text
//! def bytes: () -> Array[Integer]
//!          | () { (Integer byte) -> void } -> self
//! ```
//!
//! Which arm applies is decided by **whether the caller wrote a block** — syntax the cursor has
//! already read. So the table holds an answer per block-ness: `"x".bytes.` gets `Array` and
//! `"x".bytes { }.` gets `String`, both exact, neither a guess about which arm was meant. Reading
//! these as unions would throw away `bytes`, `chars`, `lines` and `split`, among the most chained
//! methods in the language. An arm whose block is optional (`?{ ... } -> T`) applies both ways
//! and counts in both.
//!
//! # The rungs below the signatures, in order
//!
//! Two of the five answers are not read out of a signature at all, and the order is what makes
//! the last one safe to ship:
//!
//! 1. rubydex named the receiver — nothing here runs.
//! 2. A signature or an assignment in the same class.
//! 3. **The class a template's path names.** `app/views/stories/show.html.erb` is rendered by
//!    `StoriesController` and `app/views/user_mailer/welcome.html.erb` by `UserMailer`, so
//!    `@story` is whatever that class assigns it — the controller the directory names, or the
//!    mailer where there is no controller, which is the order Rails itself resolves them in. A
//!    convention rather than a fact, shippable because it names the class and the line it read
//!    them from: a reader who thinks Rails renders this template from elsewhere can go and
//!    look, and the card says *controller* or *mailer* rather than guessing which it was.
//! 4. **The receiver's own spelling.** `@user` is a `User` — a guess in the strict sense, and the
//!    only answer ya-lsp gives that is allowed to be wrong. Labelled wherever it appears;
//!    [`Sources::guess`] turns it off.
//! 5. The name-based list, where every caller has always degraded.
//!
//! A guess never displaces a convention, a convention never displaces a derivation, and none is
//! reached until rubydex has come back imprecise. Three places enforce that ordering and all
//! three are load-bearing: [`locator::resolve_typed`] gates the ladder, [`named`] asks 3 before
//! 4, and `cursor` refuses to let a bare name count as an assignment's answer.

use std::collections::HashMap;
use std::path::Path;

use ruby_rbs::node::{
    ClassNode, MethodDefinitionKind, MethodDefinitionNode, ModuleNode, Node, Visit, parse,
};
use rubydex::{
    model::{
        declaration::{Declaration, Namespace},
        definitions::{Definition, Receiver as DefinitionReceiver},
        graph::Graph,
        ids::{DeclarationId, NameId, StringId, UriId},
        name::{Name, ParentScope},
    },
    query,
};

use super::{
    cursor::{self, Arity, Receiver},
    environment, locator,
    position::{ByteSpan, Rebase},
    scopes, views,
};
use crate::workspace::{DocUri, rails};

/// What RBS declares each method to return, keyed the way rubydex keys the method.
///
/// The value is a class *name* rather than a `DeclarationId` because the name is what a
/// declaration that has not been indexed yet still has: signatures arrive in whatever order the
/// walk finds them, and `Array` may be harvested before `array.rbs` has been read.
#[derive(Debug, Default)]
pub struct Types {
    returns: HashMap<DeclarationId, Overloads>,
    /// What a method's **block** is handed, by position.
    ///
    /// A separate map rather than a field of [`Overloads`], because it answers a different
    /// question and is not partitioned the same way. `returns` is a function of the *call site*
    /// — how many arguments were written, whether a block was — and a block parameter is not:
    /// it is what the signature says the block receives, and a call that reaches this has
    /// already written the block. So there is one entry per method, and it exists only where
    /// every arm that declares a block agrees about it.
    ///
    /// `None` in a slot is a parameter whose type the policy refuses — `untyped`, a type
    /// variable — kept as a slot so the positions after it still line up.
    yields: HashMap<DeclarationId, Box<[Option<Return>]>>,
    /// What a **constant** holds, for the constants a signature states a type for.
    ///
    /// The third table, and the one that is not about a method at all. `ENV` and
    /// `URI::RFC2396_PARSER` hold an *object*; the constant is not the class, so
    /// `graph.declarations()` has a `Declaration::Constant` with no singleton and
    /// `receiver_type` had nothing to answer with. The signature does say:
    /// `ENV: RBS::Unnamed::ENVClass` is a declaration of the type, exactly as a `-> String` is,
    /// and it is read here for the same reason and keyed the same way.
    ///
    /// Keyed by the constant's own [`DeclarationId`], which is the hash of its qualified name —
    /// so `Float::INFINITY` is one key and the `INFINITY` of some other class is another. There
    /// is no overload structure and no call site to partition by: a constant is one thing.
    constants: HashMap<DeclarationId, Held>,
}

/// One constant a signature gives a type: what it is called, and what it holds.
///
/// The name is kept beside the class although the key is its hash, because a hash cannot be
/// read back and the card has to name what was followed. `ENV.fetch("HOME").upcase` carries
/// the derivation two rungs past the constant, where a reader can no longer see which one it
/// was.
#[derive(Debug)]
struct Held {
    constant: Box<str>,
    class: Box<str>,
}

/// What one method hands back, which can depend on how the caller wrote the call.
///
/// Two facts about the call site tell arms apart, and both are syntax the cursor has already
/// read: whether a block was written, and how many positional arguments were. Neither is
/// inference and neither can make an answer *wrong*. The arity does take answers **away**,
/// which the block rule does not: 1,241 methods across `vendor/rbs` answered a zero-argument
/// call before it and answer nothing now, every one of them a method that requires an argument.
/// `types.md` has the argument for why that is the right trade. A method whose every partition
/// is empty is not stored at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Overloads {
    plain: Arms,
    with_block: Arms,
}

impl Overloads {
    fn get(&self, arity: Arity, with_block: bool) -> Option<&Return> {
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

/// What one side of the block hands back, as a function of how many arguments were written.
///
/// `Float#round` is the shape this exists for: `(?half: ...) -> Integer` beside
/// `(int digits, ?half: ...) -> (Integer | Float)`. Read as one partition the two disagree and
/// both are dropped; read by arity the zero-argument side agrees with itself and `3.7.round.`
/// is an `Integer`, exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Arms {
    /// Indexed by how many positional arguments were written: `by_arity[n]` is what a call
    /// writing `n` of them reaches, and `None` there means no arm on this side takes `n` — or
    /// that the ones which do disagree. Long enough to hold every arity an arm names exactly.
    by_arity: Vec<Option<Return>>,
    /// What a call writing more arguments than `by_arity` covers reaches. Non-`None` only where
    /// some arm declares a rest parameter, which is the "applies to every arity" case, exactly
    /// as `?{ }` applies to both sides of the block.
    beyond: Option<Return>,
    /// What a call whose arguments cannot be counted reaches: every arm on this side, agreeing.
    ///
    /// `foo(*args)` is the whole of this field's reason to exist, and holding the unpartitioned
    /// answer rather than nothing is what makes the split a partition instead of a filter — the
    /// answer such a call got before arity was read is the answer it still gets.
    any: Option<Return>,
}

impl Arms {
    fn at(&self, arity: Arity) -> Option<&Return> {
        match arity {
            Arity::Unknown => self.any.as_ref(),
            // Past the end is not "the nearest bucket": it is the rest arms, and nothing at all
            // where there are none. A call the signature cannot accept is answered for by no
            // arm, which is the same thing RBS says about it.
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
    /// A class, by name: `String`, `Enumerator::Lazy`, `Widget::<Widget>`.
    Class(Box<str>),
    /// `self` — and it means the *receiver's* type, not the declaring class.
    ///
    /// The distinction is not pedantry and it is the reason this is not resolved at harvest
    /// time. `Kernel#tap` is declared `() { (self) -> void } -> self`, and `"x".tap` returns a
    /// `String`: filing it as `Kernel` would answer `"x".tap.` with `Kernel`'s three methods
    /// and none of `String`'s. Every `self` in Ruby's core signatures that is reached through
    /// an ancestor has this shape.
    Same,
    /// The **model** the receiver is about: `Story::Relation` and `Story`'s class object both
    /// mean `Story`.
    ///
    /// [`Return::Same`] one step further, and for the same reason. `self` says "whatever this
    /// was called on" and lets one signature serve every receiver; this says "the model that
    /// receiver belongs to", which is what lets one signature of `first` serve every relation
    /// and every model in the project instead of one per element type — which is what keeps
    /// the generated declaration count from growing with the model count. See
    /// [`RELATION_BASE`](crate::workspace::rails::RELATION_BASE).
    ///
    /// Written [`ELEMENT`] in the RBS this crate generates, and nowhere else: no signature
    /// anybody ships spells it, so a harvest that reads it read text ya-lsp wrote.
    Element,
    /// The **relation** of the model the receiver is about: `Story` and `Story::Relation` both
    /// mean `Story::Relation`.
    ///
    /// [`Return::Element`]'s twin and the half `self` cannot cover: on a relation `where`
    /// really does return `self`, and on the *class object* it returns the relation, which is a
    /// different type from the receiver. Written [`COLLECTION`].
    Collection,
}

/// What [`Return::Element`] is spelled as in the RBS ya-lsp generates.
///
/// A sentinel class name rather than an RBS keyword, because RBS has no keyword for it: `self`
/// is the receiver and `instance` is the *declaring* class, and what the query interface needs
/// is neither. It never reaches the graph, because nothing declares a class of this name: a
/// lookup that somehow escaped [`class_of`] would answer `None` and stop the chain rather than
/// name a class that is not there.
///
/// **And it never reaches a reader**, which used to be true because nothing printed a return
/// type at all and is now a rule with one place to keep it: an inlay hint draws a `def`'s
/// declared return, and [`hints`](super::hints) draws a label only for a `Return::Class` whose
/// name the graph holds a class or module for. These two are neither, by construction — so the
/// spelling stays an implementation detail of the query interface, which is the whole reason it
/// is a made-up name.
pub const ELEMENT: &str = "ActiveRecordElement";

/// What [`Return::Collection`] is spelled as. [`ELEMENT`]'s argument, for the other half.
pub const COLLECTION: &str = "ActiveRecordCollection";

impl Types {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read every method definition in one RBS document, and keep the ones the policy takes.
    ///
    /// Answers whether the document parsed at all. A signature rbs cannot parse contributes
    /// nothing, which is what happens to it everywhere else in the crate: rubydex will not index
    /// it either. The answer matters to exactly one caller —
    /// [`synthesized::Synthesized::record`](super::synthesized::Synthesized::record), which
    /// writes its own RBS and so is the one place a rejection is this crate's bug rather than a
    /// malformed file somebody else shipped.
    ///
    /// Harvesting the same document twice overwrites rather than accumulates, so re-indexing an
    /// edited buffer cannot leave two answers for one method. A method *deleted* from a buffer
    /// does leave its entry behind, and that entry is unreachable: a lookup only ever happens
    /// after rubydex has found the member, and rubydex no longer has one.
    pub fn harvest(&mut self, source: &str) -> bool {
        let Ok(signature) = parse(source) else {
            return false;
        };
        let mut walk = Harvest {
            nesting: Vec::new(),
            types: self,
        };
        walk.visit(&signature.as_node());
        true
    }

    /// What a method returns when called the way `arity` and `with_block` say it was, or `None`
    /// where the policy has no answer for calls written that way.
    ///
    /// Both are facts about the *call site* rather than about the signature, and between them
    /// they are the whole reason `"x".bytes` and `3.7.round` can be answered at all.
    #[must_use]
    pub fn returns(
        &self,
        method: DeclarationId,
        arity: Arity,
        with_block: bool,
    ) -> Option<&Return> {
        self.returns.get(&method)?.get(arity, with_block)
    }

    /// How many methods the table has an answer for. For the log line, and for the tests that
    /// measure the policy against Ruby's own signatures.
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
        self.yields.clear();
        self.constants.clear();
    }

    /// The class a signature says this constant holds, if one does.
    ///
    /// Asked of a constant that is a receiver, and only there: it is the one question about a
    /// constant this crate cannot answer out of the graph, because rubydex records a constant's
    /// name and never its value.
    #[must_use]
    fn held(&self, constant: DeclarationId) -> Option<&Held> {
        self.constants.get(&constant)
    }

    /// What the block of `method` is handed at position `index`, if the signature says.
    ///
    /// The block half of a signature. `Story::Relation#each` is declared
    /// `() { (Story) -> void } -> Story::Relation`, and until this the block's parameter was
    /// typed only if its *name* happened to camelize onto a class — so
    /// `.each do |instance|` answered nothing and `.each do |story|` answered by a guess.
    #[must_use]
    pub fn yielded(&self, method: DeclarationId, index: usize) -> Option<&Return> {
        self.yields.get(&method)?.get(index)?.as_ref()
    }

    /// What a method hands back **however the call is written**, or `None` where that depends on
    /// how it is written.
    ///
    /// [`Self::returns`] is asked about a call site and partitions by what the call wrote;
    /// this is asked about the `def` itself, where there is no call to read. So it takes the
    /// arm every side agrees on and refuses where the sides disagree: `String#bytes` is
    /// `Array[Integer]` plainly and `self` with a block, and a label on the `def` that picked
    /// one of those would be wrong half the time it was read.
    ///
    /// One side answering and the other not is agreement rather than disagreement — a method
    /// whose every arm declares a block has no plain arm to contradict it — which is what
    /// `bytes` and an ordinary `() -> String` are told apart by.
    #[must_use]
    pub fn declared_return(&self, method: DeclarationId) -> Option<&Return> {
        let overloads = self.returns.get(&method)?;
        match (
            overloads.get(Arity::Unknown, false),
            overloads.get(Arity::Unknown, true),
        ) {
            (Some(plain), Some(with_block)) => (plain == with_block).then_some(plain),
            (answer, None) | (None, answer) => answer,
        }
    }

    fn insert(&mut self, method: &str, returns: Overloads) {
        self.returns.insert(DeclarationId::from(method), returns);
    }

    fn insert_yield(&mut self, method: &str, yields: Box<[Option<Return>]>) {
        self.yields.insert(DeclarationId::from(method), yields);
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

/// The walk, carrying the lexical nesting RBS declarations sit in.
struct Harvest<'t> {
    /// The enclosing `class`/`module` names, qualified, outermost first.
    nesting: Vec<String>,
    types: &'t mut Types,
}

impl Visit for Harvest<'_> {
    fn visit_class_node(&mut self, node: &ClassNode<'_>) {
        let qualified = self.qualify(&type_name(&node.name()));
        self.nesting.push(qualified);
        ruby_rbs::node::visit_class_node(self, node);
        self.nesting.pop();
    }

    fn visit_module_node(&mut self, node: &ModuleNode<'_>) {
        let qualified = self.qualify(&type_name(&node.name()));
        self.nesting.push(qualified);
        ruby_rbs::node::visit_module_node(self, node);
        self.nesting.pop();
    }

    /// Deliberately empty, and it is the same rule [`signatures`](super::signatures) enforces
    /// by editing the text: **ya-lsp does not index RBS interfaces**, so their members have no
    /// declaration for this table to be keyed by. Harvesting them would file `_Rand#rand` under
    /// whatever the enclosing scope is and answer for a method that is not there.
    fn visit_interface_node(&mut self, _: &ruby_rbs::node::InterfaceNode<'_>) {}

    /// `ENV: RBS::Unnamed::ENVClass` — the one declaration in RBS that types something other
    /// than a method.
    ///
    /// **Only a class instance type is kept**, which [`class_of`] already decides: a constant
    /// declared `String | Symbol`, `untyped` or a literal names no single class whose members
    /// can be offered, and a `Return` that is [`Return::Same`], [`Return::Element`] or
    /// [`Return::Collection`] is relative to a receiver a constant does not have.
    ///
    /// The owner handed over is the **constant's own name**, and it is never read: `instance`
    /// and `class` are the two types that use it, and RBS's grammar refuses both in a
    /// constant's type — a document holding one does not parse at all, which
    /// `what_a_signature_says_a_constant_holds` pins. So there is no owner to be wrong about.
    fn visit_constant_node(&mut self, node: &ruby_rbs::node::ConstantNode<'_>) {
        let name = self.qualify(&type_name(&node.name()));
        let Some(Return::Class(class)) = class_of(&node.type_(), &name) else {
            return;
        };
        self.types.insert_constant(&name, &class);
    }

    fn visit_method_definition_node(&mut self, node: &MethodDefinitionNode<'_>) {
        let Some(owner) = self.nesting.last() else {
            // A method written at the top level of a signature file. RBS has no such thing —
            // every member is inside a declaration — so there is nothing to own it.
            return;
        };
        let symbol = node.name();
        let name = symbol.as_str();

        // `def self?.foo` declares the method twice, on the instance side and on the singleton,
        // and rubydex files two declarations for it. Both get the entry, because either can be
        // the one a call reaches.
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
                singleton_name(owner)
            } else {
                owner.clone()
            };
            let member = format!("{receiver}#{name}()");
            // The two tables are filled independently: a method whose return the policy refuses
            // may still say exactly what its block is handed — `each` is declared
            // `() { (Story) -> void } -> void` all over Ruby's own signatures — and refusing
            // the block because the return was refused would throw that away.
            if let Some(yields) = declared_yield(node, owner) {
                self.types.insert_yield(&member, yields);
            }
            let returns = declared_return(node, owner);
            if returns.is_empty() {
                continue;
            }
            self.types.insert(&member, returns);
        }
    }
}

impl Harvest<'_> {
    /// A declaration's name, under whatever it is written inside.
    fn qualify(&self, written: &Written) -> String {
        match (written.absolute, self.nesting.last()) {
            // `class ::Foo::Bar` names the top level however deep it is written.
            (true, _) | (false, None) => written.path.clone(),
            (false, Some(outer)) => format!("{outer}::{}", written.path),
        }
    }
}

/// rubydex's name for a class's singleton: the qualified name, then the *unqualified* one in
/// angle brackets — `Shelf::Book` becomes `Shelf::Book::<Book>`.
///
/// The unqualified half is rubydex's spelling and not a choice made here; `hover::attached_name`
/// is the other place in the crate that has to know it.
fn singleton_name(owner: &str) -> String {
    let last = owner.rsplit("::").next().unwrap_or(owner);
    format!("{owner}::<{last}>")
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
/// The arms are partitioned twice — by whether the call wrote a block, and by how many
/// positional arguments it wrote — and each partition has to agree with itself: one whose arms
/// name two classes is a union and is dropped, and so is one holding a single arm the policy
/// refuses. `String#gsub` declares three arms of one arity — two returning `String`, one an
/// `Enumerator` — and none of them takes a block, so there is nothing anywhere to offer.
/// `String#bytes` is told apart by the block and `Float#round` by the arity, and both answer.
fn declared_return(node: &MethodDefinitionNode<'_>, owner: &str) -> Overloads {
    let mut plain: Vec<Arm> = Vec::new();
    let mut with_block: Vec<Arm> = Vec::new();

    for overload in node.overloads().iter() {
        let Node::MethodDefinitionOverload(overload) = overload else {
            continue;
        };
        let Node::MethodType(method_type) = overload.method_type() else {
            continue;
        };
        let arm = arm_of(&method_type, owner);

        match method_type.block() {
            // A required block: this arm is only what a call *with* one reaches.
            Some(block) if block.required() => with_block.push(arm),
            // `?{ ... }` — an optional block, so the arm applies whether or not one was written.
            Some(_) => {
                plain.push(arm.clone());
                with_block.push(arm);
            }
            None => plain.push(arm),
        }
    }

    Overloads {
        plain: settle(&plain),
        with_block: settle(&with_block),
    }
}

/// What every arm of `node` that declares a block agrees its block is handed, by position.
///
/// `None` where they do not agree, where none of them has a block, or where the block's own
/// function type is one RBS has given up describing — the same refusal [`arm_of`] makes for a
/// `(?) -> T`. **Agreement is the whole of the safety**: `Enumerable#each_entry` is declared
/// one way yielding an element and another yielding an array of them, and an answer that
/// depends on which overload the reader meant is not one.
///
/// A **required** block and an optional one are both read. The call site that reaches this has
/// written a block whatever the signature permitted, so `?{ (Story) -> void }` is as much a
/// statement about what that block receives as `{ (Story) -> void }` is.
fn declared_yield(node: &MethodDefinitionNode<'_>, owner: &str) -> Option<Box<[Option<Return>]>> {
    let mut agreed: Option<Vec<Option<Return>>> = None;
    for overload in node.overloads().iter() {
        let Node::MethodDefinitionOverload(overload) = overload else {
            continue;
        };
        let Node::MethodType(method_type) = overload.method_type() else {
            continue;
        };
        let Some(block) = method_type.block() else {
            continue;
        };
        let Node::FunctionType(function) = block.type_() else {
            return None;
        };
        let parameters: Vec<Option<Return>> = function
            .required_positionals()
            .iter()
            .filter_map(|parameter| match parameter {
                Node::FunctionParam(parameter) => Some(class_of(&parameter.type_(), owner)),
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

/// One overload, as the two things a partition needs: which calls reach it, and what it hands
/// back.
#[derive(Clone)]
struct Arm {
    /// The fewest positional arguments a call must write to reach this arm.
    least: usize,
    /// The most it may write, or `None` for a rest parameter, which has no most.
    most: Option<usize>,
    /// `None` where the policy refuses the return type. The arm still counts — a partition with
    /// one usable arm and one that says nothing cannot be answered for.
    returns: Option<Return>,
}

/// One arm's shape, read off its method type.
///
/// The one function shape refused on purpose is rbs's `UntypedFunctionType`, `(?) -> T`. It does
/// carry a return type; a signature that has given up on describing its own parameters is not
/// one to derive a receiver from — and it has given up on its arity too, so it is an arm that
/// reaches every partition and poisons each of them, which is what it did before arity was read.
fn arm_of(method_type: &ruby_rbs::node::MethodTypeNode<'_>, owner: &str) -> Arm {
    let Node::FunctionType(function) = method_type.type_() else {
        return Arm {
            least: 0,
            most: None,
            returns: None,
        };
    };
    // Trailing positionals — `(?String, Integer)` — are required like the leading ones; RBS
    // keeps them in their own list only so it can say where the optional ones went.
    let least = function.required_positionals().iter().count()
        + function.trailing_positionals().iter().count();
    Arm {
        least,
        most: (function.rest_positionals().is_none())
            .then(|| least + function.optional_positionals().iter().count()),
        returns: class_of(&function.return_type(), owner),
    }
}

/// The arms of one side of the block, partitioned by arity and each partition settled.
///
/// `by_arity` runs to the largest arity any arm names exactly, because past that point every
/// arm that still applies is a rest arm and they all answer the same thing — which is what
/// `beyond` holds. `any` is every arm together, for a call whose arity cannot be counted.
fn settle(arms: &[Arm]) -> Arms {
    // A rest arm names its `least` exactly and everything above it through `beyond`, so it is
    // the one that decides how far the buckets run.
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

/// What some set of arms agrees on, or `None` where they do not — or where there are none,
/// which is a partition no call can reach and is answered for the same way.
fn agreed<'a>(arms: impl Iterator<Item = &'a Arm>) -> Option<Return> {
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
    held: Option<Return>,
    /// Set by an arm the policy refused, or by two arms naming different classes. Sticky: an
    /// arm that agrees afterwards does not undo it.
    broken: bool,
}

impl Agreement {
    fn saw(&mut self, returns: Option<Return>) {
        match (returns, &self.held) {
            (None, _) => self.broken = true,
            (Some(seen), Some(held)) if *held != seen => self.broken = true,
            (Some(seen), None) => self.held = Some(seen),
            (Some(_), Some(_)) => {}
        }
    }

    fn settled(self) -> Option<Return> {
        if self.broken { None } else { self.held }
    }
}

/// The class an RBS type names, under the policy in the module docs.
///
/// `owner` is what `instance` and `class` mean, and they are resolved here rather than stored,
/// so a lookup is a hash and nothing more. `self` is the one that cannot be — see
/// [`Return::Same`].
fn class_of(node: &Node<'_>, owner: &str) -> Option<Return> {
    match node {
        // The generic's arguments are deliberately not looked at: `Array[Integer]` and
        // `Array[String]` have the same members, and the element type is a question this
        // module does not answer.
        Node::ClassInstanceType(class) => {
            let written = type_name(&class.name());
            // The two names the query interface writes and nobody else does. Read here rather than in the
            // generator's own consumer because this is the one place an RBS type becomes a
            // [`Return`], and a spelling recognised anywhere else would be a second table.
            match written.path.as_str() {
                ELEMENT => Some(Return::Element),
                COLLECTION => Some(Return::Collection),
                _ => Some(Return::Class(written.path.into())),
            }
        }
        // `String?` is `String | nil`, and `nil`'s members are not what anybody typing a `.`
        // after it wants. The one inexact entry in the table.
        Node::OptionalType(optional) => class_of(&optional.type_(), owner),
        // `self` is the receiver's type, which this side cannot know — see [`Return::Same`].
        // It is what makes `.strip.strip` and `[].each.` work, and it needs no singleton case:
        // a singleton method's receiver *is* the singleton class, so "the same thing it was
        // called on" is right on both sides.
        Node::SelfType(_) => Some(Return::Same),
        // `instance` and `class` name the declaration the method is written in, which is what
        // makes them resolvable here where `self` is not: `def self.new: () -> instance`.
        Node::InstanceType(_) => Some(Return::Class(owner.into())),
        Node::ClassType(_) => Some(Return::Class(singleton_name(owner).into())),
        // `singleton(Foo)` names a singleton exactly, and rubydex has a declaration for it.
        Node::ClassSingletonType(singleton) => {
            let written = type_name(&singleton.name());
            Some(Return::Class(singleton_name(&written.path).into()))
        }
        // Everything else: a union, an interface, `untyped`, `void`, `bool` (which is `true |
        // false`, a union, and the reason YARD had to invent a `Boolean` class Ruby does not
        // have), `nil`, a proc, a tuple, a record, a literal, a type variable, a type alias.
        // None of them names one class whose members can be offered.
        _ => None,
    }
}

/// The lexical scope and the `self` type at an offset.
pub struct Scope {
    /// The innermost `class`/`module`/`class << self` the offset is inside, as rubydex names it.
    /// Top-level code is inside `Object`, which is what Ruby says too.
    pub nesting: NameId,
    /// Set only where `self` is not the nesting: `def self.build` and `def Foo.build`.
    pub self_id: Option<DeclarationId>,
}

/// rubydex's name for the top-level scope.
///
/// Ruby's top level *is* `Object`, and rubydex indexes a built-in `class Object` so the name
/// exists in every graph. Building the id rather than looking it up costs a hash and no lookup.
///
/// The graph is read for the names map alone, which `Name::new` needs to compute a name's
/// *depth*. Depth is not one of the parts `Name::id` hashes, so the id this returns is the same
/// whatever the map holds — but constructing a `Name` with a wrong depth and letting it escape
/// is not a habit worth starting, so the real map is passed.
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
        // A *point* cannot straddle a definition's edge — it is inside the span or outside it —
        // so the refusal below is unreachable from here. The fallback is written rather than
        // asserted because a panic is a worse way to be wrong about that than a top-level scope.
        Self::covering(graph, uri_id, offset, offset).unwrap_or_else(|| Self {
            nesting: object_name(graph),
            self_id: None,
        })
    }

    /// The scope every offset in `lo..=hi` is written in, when one body holds all of them.
    ///
    /// **Why a scope question takes a range at all.** A deferred index means the commonest
    /// cursor — the one at the end of what was just typed — is inside the text the graph has
    /// never been given, so [`Rebase::to_graph`](crate::analysis::position::Rebase::to_graph)
    /// refuses it. But a scope is not an offset-precise question: it is which `class` and which
    /// `def` the cursor is inside, and definition spans are large where an edit is small. Asking
    /// for the body that contains the *whole* changed region answers it without naming a point
    /// inside that region.
    ///
    /// **`None` where the region runs out of a body, and widening instead is not safe.** A wider
    /// scope does not offer a shorter list; it offers *another class's*: `self_of` reads the
    /// enclosing method to decide whether `self` is an instance or the class object, so losing the
    /// method does not shorten the answer, it swaps the side. An edit running from inside one `def`
    /// to inside the next is covered by the class body alone, and `self.` there answered the
    /// singleton's members while the caret sat in an instance method — a wrong answer rather than
    /// an empty one, which no fallback can notice. So any of the four bodies this reads
    /// that *overlaps* the region without containing it is a body the region leaves, and the
    /// question is refused instead of widened.
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
        // constructs it is written inside, and the narrowest is the innermost.
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
                // Not containing the region. If it *meets* it, the region crosses this body's
                // edge — the edit deleted a `def`, or ran past an `end` — and the graph's answer
                // about which body the caret is in no longer describes the buffer at all.
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

    /// Every body in one document, read once, so that many offsets can be placed for one walk.
    ///
    /// [`Scope::at`] is the right shape for a caller with one cursor and the wrong one for a
    /// caller with a hundred: it reads every definition the document has, so asking it per
    /// candidate is quadratic in the file. Measured on a file of 2,000 methods with a hint on
    /// each, `textDocument/inlayHint` spent **325 of its 352 ms here** and 28 ms on everything
    /// else it does. This is the same question with the walk hoisted out of the loop.
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
            uri_id,
            namespaces,
            methods,
        }
    }

    /// The declaration the nesting names, if the graph resolved it.
    #[must_use]
    pub fn nesting_id(&self, graph: &Graph) -> Option<DeclarationId> {
        graph.name_id_to_declaration_id(self.nesting).copied()
    }

    /// Who is calling — for a receiver context to check visibility against, and what `self` *is*
    /// for an expression.
    ///
    /// rubydex derives nothing from the nesting when an `Expression` is handed `None`: such a
    /// context collects no methods and no instance variables. So every context asks this, and the
    /// derivation lives here — an unstated `self` is the nesting, which is what Ruby means.
    #[must_use]
    pub fn caller(&self, graph: &Graph) -> Option<DeclarationId> {
        self.self_id.or_else(|| self.nesting_id(graph))
    }
}

impl Sources<'_> {
    /// The lexical scope and the `self` at one offset of one document.
    ///
    /// The hoisted walk when there is one for *this* document, and a fresh one otherwise. Both
    /// answer the same question; which is used is a cost, never a difference in the answer.
    #[must_use]
    pub fn scope_at(&self, uri_id: UriId, offset: u32) -> Scope {
        match self.bodies {
            Some(bodies) if bodies.uri_id() == uri_id => bodies.at(offset),
            _ => Scope::at(self.graph, uri_id, offset),
        }
    }
}

/// One document's bodies, kept so that a scope question costs a containment test and not a walk.
///
/// **Points only**, which is the difference from [`Scope::covering`] and the reason this can be
/// a plain innermost lookup: a point is inside a body or outside it and can never straddle one's
/// edge, so the refusal `covering` exists for is unreachable here — `Scope::at` says the same
/// thing and falls back rather than asserting it.
pub struct Bodies<'g> {
    graph: &'g Graph,
    /// Which document was walked. Carried because [`Sources::bodies`] is a *borrowed* walk that
    /// travels down into rungs taking a `uri_id` of their own, and a containment test against
    /// another document's spans would answer confidently and wrongly.
    uri_id: UriId,
    namespaces: Vec<&'g Definition>,
    methods: Vec<&'g Definition>,
}

impl Bodies<'_> {
    /// Which document this walked.
    #[must_use]
    pub fn uri_id(&self) -> UriId {
        self.uri_id
    }

    /// The lexical scope and the `self` type at one offset.
    ///
    /// Innermost is the narrowest containing span, which is [`Scope::covering`]'s rule written
    /// for a point: a definition nested inside another is narrower than it, and two definitions
    /// that are not nested cannot both contain one offset.
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
/// Narrowest is innermost: a definition nested inside another is shorter than it, and two that
/// are not nested cannot both contain one offset. Ties keep the first, which is what
/// [`Scope::covering`]'s strict [`wider`] does with them.
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
/// Two places it is not, and both matter:
///
/// - **A class or module body.** `self` there is the class *object*, so what can be called is
///   `Foo`'s singleton methods — which is the entire Rails DSL. `validates`, `has_many`, `scope`
///   and `belongs_to` are all class methods, and completing a model's body against the instance
///   side offers `valid?` and `validate` while silently omitting every macro anyone writes
///   there. Measured on a real app: 49 suggestions for `valid`, not one of them `validates`.
/// - **`def self.build` and `def Foo.build`.** The lexical scope stays the class while `self`
///   moves to the singleton, and constants follow the first while methods follow the second.
///
/// The top level is not one of them: `self` is `main`, an ordinary `Object`, so what rubydex
/// derives from the nesting is already right and `None` says so.
///
/// **The two bodies are read innermost-first, and the enclosing method is not always the inner
/// one.** Ruby refuses a `class` keyword inside a method body, so `Class.new(base) do … end` is
/// the only way to write one — and rubydex records that block as a class all the same. The block
/// is `class_eval`'d, so `self` inside it is the new class *object*, exactly as in a written
/// class body, and the `def` around it says nothing about it. Bodies nest and never overlap, so
/// the one that starts later is the inner one and it is the one that decides.
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
/// One parameter rather than four, and that is not tidying. Two of the five answers cannot be
/// found in the file the cursor is in — a template's instance variables are assigned by a
/// controller in another file, and the last rung is a guess that has to be switchable off — so
/// what the type side consults grew from two things to four, in `completion` and the locator as
/// well as here. Four parameters that always travel together are one thing with four fields;
/// keeping them apart is how a signature reaches eight arguments and stops being readable.
///
/// `Copy` for one field's sake — see [`Sources::constant_hops`], which is the only one a rung
/// ever changes, and changes by handing the rungs below it a *copy* rather than by mutating
/// anything a sibling branch can see.
#[derive(Clone, Copy)]
pub struct Sources<'a> {
    pub graph: &'a Graph,
    /// What RBS says each method returns. Beside the graph rather than inside it — see the
    /// module docs.
    pub types: &'a Types,
    /// Another document's text, by the URI rubydex filed it under, **and how that text's
    /// offsets relate to the ones the graph holds for it**.
    ///
    /// A closure because the reading belongs to [`analysis`](super): an open buffer is
    /// authoritative over the file on disk, and only the server knows which buffers are open —
    /// a controller being edited types the template it renders before it is saved. `None` for a
    /// document with no readable text, which is an answer and not an error.
    ///
    /// The [`Rebase`] travels with the text because it is a fact about *that* document and the
    /// caller is in another one. A controller being edited is the whole point of reading the
    /// buffer, and it is also exactly when the buffer's offsets stop naming the graph's text —
    /// so the text and its map are one value, and a reader that takes the first cannot forget
    /// the second.
    pub read: &'a dyn Fn(&str) -> Option<(String, Rebase)>,
    /// What a template's implicit receiver can answer.
    ///
    /// Beside the graph rather than in it for the type table's reason, and consulted by
    /// [`locator::resolve_typed`] and by `completion` rather than from here: it answers a
    /// *member* and not a receiver's type, which is the one thing on this struct that no rung
    /// in this module reads. It travels here because it travels with the other three, and
    /// because both of its readers already take a `Sources`.
    pub views: &'a views::Views,
    /// Which bodies of knowledge this project asked for.
    ///
    /// Read here by the three rungs that are **outside the generator pass**, which is the whole
    /// reason it has to travel: switching a generator off empties its list, and a rung that
    /// reads a path rather than a declaration would go on answering from a convention nobody
    /// asked for. `rails::camelize` and `rails::element_of` are deliberately *not* gated by it —
    /// see the two call sites, and `types.rs`'s own note that the inflection is shared rather
    /// than copied. Singularising a directory name is not a Rails feature.
    pub features: crate::workspace::Features,
    /// Whether the name-based guess may answer at all.
    ///
    /// Off is a supported configuration and the reason the tier is shippable: every other
    /// answer ya-lsp gives is defensible when it is wrong, and this one is not. A user who
    /// wants only checkable answers can have them.
    pub guess: bool,
    /// Where this project's own files are, and what `require` can name.
    ///
    /// Nothing in *this* module reads it, the way `views` is read only by its two callers. It
    /// travels here because it travels with the other four and because the readers already take
    /// a `Sources`: [`locator::resolve_typed`](super::locator) and `completion` both build an
    /// [`environment::Fence`](super::environment::Fence) from it, and a fence built without it
    /// calls a gem's `lib/rack/test/` a suite.
    pub layout: environment::Layout<'a>,
    /// One document's bodies, already walked, for the callers that ask about many offsets.
    ///
    /// `None` is the ordinary case and costs a walk in exactly one arm —
    /// [`Receiver::SelfObject`], which has to place an offset that is not the cursor's. A
    /// request with one cursor pays that once and the document has been walked once already;
    /// `inlayHint` has a hint per binding and would pay it per hint, which is the quadratic
    /// [`Scope::bodies`] exists to avoid, so that request hands its own walk down here.
    ///
    /// Read through [`Sources::scope_at`] and never directly: it is a walk of *one* document
    /// and the rungs below take a `uri_id` of their own.
    pub bodies: Option<&'a Bodies<'a>>,
    /// How many constant assignments have already been followed to get here.
    ///
    /// **A cycle guard, and the only thing standing between `A = B.new` / `B = A.new` and a
    /// stack overflow** — which a language server cannot contain, because the per-request
    /// bulkhead catches a panic and an overflow aborts the process. Every other recursion in
    /// this module is bounded by a shape that was parsed once; [`assigned_to`] is the one rung
    /// that crosses into another document and can arrive back where it started.
    ///
    /// Zero at every call site but one: `assigned_to` hands the rungs below it a copy with this
    /// raised, which is why it is a field of a `Copy` struct and not a counter anything shares.
    pub constant_hops: u8,
}

/// How many `CONST = Klass.new` hops a single answer may follow.
///
/// Three rather than one, because the chain is real: `THING = FACTORY.build` through
/// `FACTORY = Builder.new` is two, and neither line is unusual. Three rather than unbounded
/// because nothing measured needs a fourth and a cycle costs a process.
const CONSTANT_HOPS: u8 = 3;

/// A receiver's declaration, and how ya-lsp arrived at it.
///
/// The second half is the point. Three of the five rungs are *derived* — correct if RBS is
/// correct, and if the assignment ya-lsp followed is the one that ran — and a
/// user who cannot tell those from the ones the code states outright has lost the property that
/// makes this server different from one that guesses well. So the derivation travels with the
/// declaration rather than being reconstructed by whoever displays it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Typed {
    pub declaration: DeclarationId,
    pub derivation: Derivation,
}

/// What was followed to get a type, in the order it was followed.
///
/// Empty means nothing was: the receiver named its own type — a constant, a literal, `self`,
/// `Foo.new`, a local assigned one of those.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Derivation {
    /// The signatures a chain was read through, in the order the chain was walked:
    /// `["String#upcase()", "String#strip()"]`.
    pub signatures: Vec<String>,
    /// The offset of the instance-variable assignment the type came from, if one did.
    pub assignment: Option<u32>,
    /// The constant whose *declared* type answered, when the receiver was one holding an object.
    ///
    /// A field of its own rather than a row in [`Self::signatures`], because the sentence that
    /// list is rendered into says "from what those **methods** declare" and this is not a
    /// method. It is the same strength of evidence — somebody wrote the type down in a
    /// signature — and the same tier.
    ///
    /// The constant's name and not the class it holds: the class is already the answer on the
    /// card, and what a reader needs told is that `ENV` being an `ENVClass` is something a
    /// signature states rather than something this expression says.
    pub constant: Option<String>,
    /// The constant whose **Ruby assignment** answered, and where that line is.
    ///
    /// The sibling of [`Self::constant`] one rung down and a separate field for the same
    /// reason: both say "a constant holds an object", and they differ in what said so. A
    /// signature is a type somebody wrote down; an application's own `CONFIG = Settings.new`
    /// is a line of Ruby that will have run. Same tier, different sentence, and a reader who
    /// cannot tell them apart cannot tell which one to go and check.
    pub assigned_constant: Option<FromAssignment>,
    /// The class a *template's* instance variable was typed from, when the view↔renderer
    /// convention answered — the controller its path names, or the mailer where there is no
    /// controller.
    pub renderer: Option<FromRenderer>,
    /// The receiver's own spelling, when nothing but it was left and the last rung answered.
    ///
    /// The name rather than the class, because the class is already the answer on the card: what
    /// a reader needs told is that `@user` being a `User` is something ya-lsp inferred from six
    /// letters and not something the code says.
    pub guess: Option<String>,
    /// The macro that made a `:symbol` a name, when the cursor was on one.
    ///
    /// A convention like the two above it and stated for their reason: nothing in `:normalise`
    /// says it is a method, and the only evidence that it is one is the word to its left. So the
    /// card names that word, and a reader who thinks `before_save` does something else can see
    /// what the answer rests on.
    pub named_by: Option<String>,
    /// The class whose **instance** side answered a bare name written inside a block in its
    /// body, when that rung answered it.
    ///
    /// A closure in a class body is the one place in Ruby where `self` is not what the file
    /// says it is: the block is a value, and whoever receives it may run it against something
    /// else entirely — `rule(:colon) { str(':') }`, `scope :recent, -> { where(...) }`,
    /// `validates :x, if: -> { active? }`. Nothing in the file states that, so the name is
    /// *derived* and the card has to say off what: the member is absent from the class object
    /// and present on an instance, which is evidence and not proof.
    ///
    /// The class rather than the member, for [`Self::guess`]'s reason — the member is already
    /// the answer on the card, and what a reader needs told is whose instance it was found on.
    pub closure: Option<String>,
    /// How a **bare** name in a template was reached, when the view-context rung answered it.
    ///
    /// The one field here that is not about a receiver's type, and it is on this struct because
    /// it is the same kind of fact as the other four: nothing in the template says that
    /// `app/helpers` is in scope or which class renders it, so the answer is *derived* and the
    /// card has to say through which of the two conventions.
    pub view: Option<views::InView>,
}

/// Which of the three kinds of answer a derivation is.
///
/// The tier is a fact about *how* a type was arrived at, so it belongs beside the derivation
/// rather than in whichever module is drawing it. Every consumer states it somehow — a hover
/// card in a footnote, a completion row in its detail — and one of them does something stronger
/// than state it: an inlay hint is drawn whether anyone asked or not and has no room for a
/// footnote, so it **refuses** the bottom tier outright. That refusal has to be a test on the
/// tier and not a list of shapes, or the next rung added below the graph is drawn in the margin
/// of every file by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The code names the type. Nothing was followed and there is nothing to doubt.
    Resolved,
    /// A signature, an assignment or a convention was followed. Correct if that is.
    Derived,
    /// Matched on a name alone — the one answer ya-lsp gives that is allowed to be wrong.
    Guessed,
}

impl Derivation {
    /// Which tier this answer is, read off what was followed to reach it.
    ///
    /// Destructured rather than matched field by field, so that another kind of provenance
    /// cannot be added without deciding which tier it belongs to. That decision is not
    /// cosmetic: `Tier::Guessed` is what one consumer refuses to draw at all.
    #[must_use]
    pub fn tier(&self) -> Tier {
        let Self {
            signatures,
            assignment,
            constant,
            assigned_constant,
            renderer,
            guess,
            named_by,
            closure,
            view,
        } = self;
        // The name rung, and it wins over everything: a chain read through three signatures
        // that *ended* at a guess is a guess, because the weakest rung is what the answer rests
        // on. The same reason `hover` prints this footnote last.
        if guess.is_some() {
            return Tier::Guessed;
        }
        if signatures.is_empty()
            && assignment.is_none()
            && constant.is_none()
            && assigned_constant.is_none()
            && renderer.is_none()
            && named_by.is_none()
            && closure.is_none()
            && view.is_none()
        {
            return Tier::Resolved;
        }
        Tier::Derived
    }
}

/// Where a template's instance variable came from: the class Rails' path convention names, and
/// the line its assignment is on **in that file**.
///
/// The line is carried rather than the offset, which every other provenance field does the
/// other way round. An offset is only a line once you have the text it indexes, and the caller
/// that draws the card has the *template's* text — the assignment is in a file it never reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromRenderer {
    pub renderer: String,
    /// Whether a controller answered rather than a mailer — [`views::RenderedBy`].
    ///
    /// Carried because the card names the convention and not only the class: `UserMailer` is
    /// not a controller, and a footnote saying it is would be wrong about the one thing it is
    /// citing. The rung is one rung either way and the tier does not move.
    pub controller: bool,
    pub line: u32,
}

/// Where a constant is given the object it holds: the constant, the file, and the line.
///
/// A line rather than an offset, for [`FromRenderer`]'s reason — the caller drawing the card
/// holds the text the *cursor* is in, and this names a line in a file it never reads.
///
/// The file is carried as well, which `FromRenderer` does not need: a template names no
/// renderer, so naming the class is news, whereas the constant here is the thing the user
/// just hovered. What a reader does not have is the file, and a constant assigned in an
/// initializer is exactly the case where "go and look" is the whole point of the footnote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromAssignment {
    pub constant: String,
    pub file: String,
    pub line: u32,
}

/// The declaration whose members can be written after a `.` on `receiver`.
///
/// The one place a [`Receiver`] becomes a thing in the graph, shared by completion and by
/// navigation so that the two cannot disagree about what `person.` is. `scope` is where the
/// cursor is written — the nesting a guessed constant is resolved in — which is the only part
/// of the answer that comes from the enclosing code rather than from the receiver.
///
/// **A `self` is placed by its own offset and not by this.** It used to read `scope` too, on
/// the reasoning that an unstated `self` is the enclosing body's — true of where the `self` is
/// *written*, and the receiver is not always written where it is read. See
/// [`Receiver::SelfObject`] and the arm below.
///
/// **`receiver` is in `uri_id`'s graph coordinates, and every caller owes that.** A `Receiver`
/// is parsed out of a *buffer* and the offsets inside it are graph keys, so the two agree only
/// while nothing has been typed since the last index — [`Receiver::rebased`] is the translation
/// and refusing is what it does instead of guessing. Three callers were reaching here with
/// buffer offsets and a fourth translated; the symptom was a card that knew the type until any
/// keystroke anywhere in the file, and then guessed it from the variable's name.
///
/// `None` is "nothing exact can be said", which is the signal every caller degrades on.
#[must_use]
pub fn method_receiver(
    sources: &Sources<'_>,
    uri_id: UriId,
    receiver: &Receiver,
    scope: &Scope,
) -> Option<Typed> {
    let graph = sources.graph;
    let plain = |declaration| {
        Some(Typed {
            declaration,
            derivation: Derivation::default(),
        })
    };
    match receiver {
        // `Foo.bar` calls a *singleton* method, so the receiver is `Foo`'s singleton class. A
        // constant that is not a namespace — `MAX.times` — has a type we cannot name, and falls
        // through to the name-based list.
        //
        // **A `Todo` is one of those, and it has to be said out loud.** rubydex spells a
        // namespace it never saw a definition of `Namespace::Todo`, and it promotes a *constant
        // used as a receiver* into one — so `ENV` and
        // `URI::RFC2396_PARSER`, which hold an object rather than a class, stopped being
        // `Declaration::Constant` and started having a singleton class. Its ancestors are
        // `Class`, `Module` and `Object`, so `ENV.` would answer `alias_method` and
        // `attr_accessor`: precise, wrong, and enough to displace the name-based list.
        // A class object you can call a singleton method on has a definition somewhere, which is
        // exactly what a `Todo` does not, and `hierarchy` and `search` already turn them away
        // for the same reason.
        //
        // **And a signature may say what the object is, which is the rung above both of those.**
        // `ENV: RBS::Unnamed::ENVClass` declares the type as plainly as a `-> String` does, so it
        // is asked **first**: the `Todo` above is the absence of evidence and this is evidence.
        // Asked before the singleton too, because a constant a signature gives a type is a
        // constant holding an object, and the singleton of the namespace rubydex invented for it
        // is exactly the wrong answer. There is no name it could take from a real class object:
        // across `vendor/rbs`, **0** of 2,473 constant declarations spell a name that is also
        // one of the 945 `class`/`module` declarations.
        //
        // **And where no signature says so, the Ruby that assigned it may.** A constant an
        // application builds for itself — `CONFIG = Settings.new` — is the same fact written in
        // the other language, and it
        // is asked **last** of the three: a class object is the type outright, a signature is a
        // type somebody stated, and an assignment is a line that will have run — which is the
        // order the five rungs are in everywhere else in this module. It is reached from both
        // of the two arms that used to answer nothing: the `Todo` above, which is what a
        // constant holding an object is promoted into the moment it is used as a receiver, and
        // a plain `Declaration::Constant` with no singleton to walk.
        Receiver::Constant(offset) => {
            let constant = constant_at(graph, uri_id, *offset)?;
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
        // **Placed where the `self` was written, not where the cursor is.** Those are the same
        // offset for a written `self.` and for the implicit one a bare call has, which is every
        // receiver but one: a `self` captured into a variable above a block that rebinds it.
        // rubydex records a `Class.new(base) do … end` body as an anonymous class, so reading
        // this against the cursor's scope answered *that* — a class with none of the captured
        // instance's members and no name [`locator::missed`](super::locator) will print, which
        // is how one card said the type was unknown while `completion` listed the other's
        // members at the same byte. See `Receiver::SelfObject`.
        Receiver::SelfObject(offset) => plain(sources.scope_at(uri_id, *offset).caller(graph)?),
        // An instance, so the receiver is the class itself rather than its singleton.
        Receiver::Instance(offset) => plain(constant_at(graph, uri_id, *offset)?),
        // A literal's class is named, not resolved — `String` means `String` in every file.
        // `declared` is what makes turning `[rbs]` off degrade rather than break: with no core
        // signatures in the graph there is no such declaration, and answering `None` reaches
        // the name-based list instead of answering with nothing at all.
        Receiver::Literal(class) => plain(declared(graph, class)?),
        Receiver::Returned {
            on,
            method,
            block,
            arity,
        } => returned_by(sources, uri_id, on, method, *arity, *block, scope),
        // The same declaration read from the other end: what the block was handed rather than
        // what the call gave back.
        Receiver::Yielded { on, method, index } => {
            yielded_by(sources, uri_id, on, method, *index, scope)
        }
        // An instance variable is what its assignment was, plus a note saying where that was
        // written. The note is the whole reason the variant exists — see `Receiver::Assigned`.
        Receiver::Assigned { at, was } => {
            let mut typed = method_receiver(sources, uri_id, was, scope)?;
            typed.derivation.assignment = Some(*at);
            Some(typed)
        }
        // The two rungs below the graph, in the only order that is safe: a convention that can
        // be checked, and then a guess that cannot.
        Receiver::Named(name) => named(sources, uri_id, name, scope),
        // The fall-through, and the `or_else` is the whole of its safety: the assignment is
        // asked first and the spelling is reached only when it answered nothing, so a chain
        // that resolves can never be displaced by a guess. This adds no rung — `named` is the
        // same pair of rungs a bare name reaches, so the answer is labelled the same way and
        // `[types] guess_from_names = false` turns it off with the rung it belongs to.
        Receiver::Spelled { was, name } => method_receiver(sources, uri_id, was, scope)
            .or_else(|| named(sources, uri_id, name, scope)),
        // `::Foo.bar` reaches here as an ordinary constant; a bare `::` never does.
        Receiver::TopLevel | Receiver::Unknown => None,
    }
}

/// One link of a chain: what the call was written on, what RBS says it hands back.
///
/// The lookup happens *after* rubydex has found the member, never before it. `[].tap` is owned
/// by `Kernel` and not by `Array`, and asking the table for `Array#tap()` would miss — so the
/// ancestor walk is rubydex's, and this only reads the answer off the declaration it found.
fn returned_by(
    sources: &Sources<'_>,
    uri_id: UriId,
    on: &Receiver,
    method: &str,
    arity: Arity,
    block: bool,
    scope: &Scope,
) -> Option<Typed> {
    let graph = sources.graph;
    let owner = method_receiver(sources, uri_id, on, scope)?;
    // rubydex keys a declaration's members with the parentheses on: see `core-invariants.md`.
    let member = format!("{method}()");
    let found =
        query::find_member_in_ancestors(graph, owner.declaration, StringId::from(&member), false)
            .ok()?;
    let declaration = match sources.types.returns(found, arity, block)? {
        Return::Class(class) => declared(graph, class)?,
        // `self` is the thing the call was written on, and that is what `owner` already is.
        Return::Same => owner.declaration,
        // **The two receiver-relative returns are ActiveRecord's and nobody else's**, which is
        // why one guard covers both and why it is written here rather than left to the lookup
        // failing. With the generator off the name would not resolve anyway; saying so is what
        // makes `Story.where(...)` fall to the next rung deliberately rather than by accident.
        Return::Element | Return::Collection if !sources.features.models => return None,
        Return::Element => declared(graph, &model_of(graph, owner.declaration)?)?,
        Return::Collection => declared(
            graph,
            &rails::relation_of(&model_of(graph, owner.declaration)?),
        )?,
    };

    let mut derivation = owner.derivation;
    // Named by the declaration rubydex found rather than by what was written, because that is
    // the signature the answer actually came from: `[].tap` says `Kernel#tap()`, which is where
    // a reader would have to go to check it.
    derivation
        .signatures
        .push(graph.declarations().get(&found)?.name().to_owned());
    Some(Typed {
        declaration,
        derivation,
    })
}

/// What the block written on one call is handed, at one position.
///
/// [`returned_by`]'s twin, and the first three lines are the same three: the call's receiver is
/// typed, rubydex finds the member on it, and the table is asked about the declaration rubydex
/// found rather than about the name that was written. Only the last step differs — the block's
/// parameter instead of the return — which is what keeps `Story::Relation#each` and
/// `Story::Relation#first` reading one signature two ways rather than two signatures.
///
/// [`Return::Same`] means the receiver, exactly as it does for a return: `Kernel#tap` is
/// declared `() { (self) -> void } -> self`, so `"x".tap { |it| ... }` hands the block a
/// `String`.
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
        query::find_member_in_ancestors(graph, owner.declaration, StringId::from(&member), false)
            .ok()?;
    let declaration = match sources.types.yielded(found, index)? {
        Return::Class(class) => declared(graph, class)?,
        Return::Same => owner.declaration,
        // The same two, and the same guard: see `returned_by` above.
        Return::Element | Return::Collection if !sources.features.models => return None,
        Return::Element => declared(graph, &model_of(graph, owner.declaration)?)?,
        Return::Collection => declared(
            graph,
            &rails::relation_of(&model_of(graph, owner.declaration)?),
        )?,
    };

    let mut derivation = owner.derivation;
    derivation
        .signatures
        .push(graph.declarations().get(&found)?.name().to_owned());
    Some(Typed {
        declaration,
        derivation,
    })
}

/// A receiver that is nothing but a name, answered by the two rungs below the graph.
///
/// **The order is the feature.** A convention that names a file and a line is checkable, and a
/// guess made from letters is not, so the convention is asked first and the guess only answers
/// what it leaves. Neither can be reached at all until rubydex and every derivation above them
/// have come back empty — that is [`locator::resolve_typed`]'s doing, and it is what keeps a
/// guess from ever displacing an answer the code states.
fn named(sources: &Sources<'_>, uri_id: UriId, name: &str, scope: &Scope) -> Option<Typed> {
    if let Some(typed) = from_renderer(sources, uri_id, name) {
        return Some(typed);
    }
    if !sources.guess {
        return None;
    }
    guessed(sources.graph, name, scope)
}

/// The class a template's path names, and every document that class is written in.
///
/// The half [`from_renderer`] and [`renderer_writes`] share, and all of the Rails convention
/// either of them reads. One asks what the assignment on the other side *is* and the other asks
/// *where it is written*; both have to agree about which class answers, or a card and a jump at
/// the same `@story` would name two different classes.
///
/// **Which class that is belongs to [`views::Views::rendered_by`] and not to this module.**
/// Rails renders a template from the controller its directory names, and a *mailer's* views
/// from the mailer itself — `app/views/user_mailer/welcome.html.erb` is `UserMailer`, because
/// `ActionMailer::Base` derives its view path from its own name. The second half needs a gate
/// the first does not (a directory spells whatever it spells, and only a controller's name is
/// unmistakable), that gate is the mailer list the view-context pass already holds, and one
/// copy of the rule is what keeps a card, a jump and a view context from naming three classes
/// for one path.
///
/// `rendered_by` reads a **path** and not a call, so this rung applies Rails' convention to any
/// project with an `app/views/` whether or not it is Rails — and would answer a *Derived* card
/// citing a class that does not exist. That is the case `[rails] views` is for, and the switch is
/// **not** read from [`Sources::features`](Sources) here: a project that turned the convention
/// off gets a `Views` the pass left switched off, and `rendered_by` declines on that. One gate,
/// travelling with the value it gates, rather than two flags that could one day disagree.
///
/// **A partial is not special-cased and must not be.** `rails::controller_of` reads the
/// directory and not the file name, so `stories/_story.html.erb` names `StoriesController`
/// exactly as `stories/show.html.erb` does, and `shared/_header.html.erb` names a
/// `SharedController` no file declares — which `rendered_by` refuses, and whose mailer half
/// refuses `Shared` in the same breath unless the application really does define a mailer by
/// that name. That refusal is the whole answer to *several possible writes in several files*:
/// the convention names one class or it names none, and a partial that several controllers
/// render is the second. Nothing here ranks candidates, because there is no evidence in a path
/// with which to rank them.
///
/// **Sorted rather than the graph's order.** A class reopened across files is answered from
/// whichever document declares the variable, so the answer must not depend on the order the
/// walk happened to index in.
fn renderer_documents(
    sources: &Sources<'_>,
    uri_id: UriId,
    name: &str,
) -> Option<(views::RenderedBy, Vec<(String, UriId)>)> {
    let graph = sources.graph;
    // A local and a receiverless call are not instance variables, and only an instance
    // variable is what a controller or a mailer hands a template.
    if !name.starts_with('@') {
        return None;
    }
    let path = DocUri::from_uri_str(graph.documents().get(&uri_id)?.uri())?.to_path()?;
    // The name and nothing like it. A template whose class does not exist answers nothing
    // rather than reaching for one that happens to be spelled similarly.
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
/// [`from_renderer`] is the card's half of this and this is the jump's: one convention, one
/// class, one set of documents, and a different question asked of each file. A card needs the
/// one assignment that produced a type; a jump needs all of them, because `@stories =
/// Story.where(live: true)` is a line a reader wants to stand on and not a type this crate can
/// name.
///
/// **Every write, and not only the ones that typed.** [`cursor::assignments_in`] drops an
/// assignment whose right-hand side is a shape nothing could be made of, which is right for a
/// type and wrong for a place — that dropped line is the common case in a real controller. So
/// this reads [`scopes::writes_to`] directly, which is the same walk one filter earlier, and
/// which is also what decides that a `def self.` holds a different `@story` of the same name.
///
/// **The spans are the controller's *buffer*, and nothing here is rebased.** They were read out
/// of the text [`Sources::read`](Sources) handed back — the open buffer where there is one, the
/// file on disk where there is not — and the caller measures them against that same text. An
/// instance variable's writes still never come out of the graph; what changed is which buffer
/// they come out of.
///
/// Grouped by document, one entry per file and no empty entries, because the caller pays one
/// text read for each one it is handed.
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
/// `app/views/user_mailer/welcome.html.erb` by `UserMailer`, and the instance variables either
/// template reads are the ones that class assigns. A template has no enclosing class of its
/// own, so there is nothing in the file to walk; this is [`cursor::assignments_in`] pointed at
/// a class the file never names.
///
/// **Bounded to the class the path names, and not its ancestors.** `@user` in a real Rails
/// application is usually set in `ApplicationController`, and walking up would find it — and
/// then fail to type it anyway, because what it is assigned is an ActiveRecord chain. The
/// refinement waits for evidence that it pays.
///
/// **Bounded to the whole controller class rather than to the matching action.** Same rule, and
/// same caveat, as an ordinary instance variable: the textually last assignment that produced a
/// type wins, and the provenance line names the line so a reader can see which one that was.
/// Walked in reverse and stopped at the first that the *graph* can answer, because an
/// assignment naming a class nothing declares has to fall through to the one above it.
///
/// `None` for everything that is not a template reading a plain `@name`, which is most calls:
/// this is the one rung that costs a second file, and it is never reached from ordinary Ruby.
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
            // **The renderer's map, not the template's**, and this is the one rung where the
            // two are different documents. `assignments_in` parsed the controller's *buffer*;
            // everything below keys the graph under `renderer_uri_id`, which holds whatever
            // that file looked like when it was last indexed. A `continue` here is an
            // assignment being typed right now, and the one above it — or the name rung — is
            // the honest answer until the edit settles.
            let (Some(at_in_graph), Some(receiver)) =
                (rebase.to_graph(*at), receiver.rebased(&rebase))
            else {
                continue;
            };
            // The scope the *assignment* is written in, in the controller's own file: `@x =
            // self` means the controller, and a constant on the right-hand side resolves
            // against the controller's nesting rather than the template's.
            let scope = Scope::at(graph, renderer_uri_id, at_in_graph);
            let Some(mut typed) = method_receiver(sources, renderer_uri_id, &receiver, &scope)
            else {
                continue;
            };
            // **The inner assignment is dropped, and this is the one rung that has to.**
            // `Derivation::assignment` is an offset with no document attached to it: every
            // consumer turns it into a line against the text the *cursor* is in, which is
            // right for every other rung because every other rung read that same document.
            // Here the chain was resolved in the controller, so a nested `Receiver::Assigned`
            // — `@messages = @mod_mail.mod_mail_messages.order(:created_at)`, which is what a
            // real `show` action looks like — leaves behind an offset into a file the card
            // will never hold. Measured on lobsters: `@messages` in
            // `app/views/mod_mails/_mail.html.erb` carried `@mod_mail`'s offset 182 out of
            // `mod_mails_controller.rb`, and the template's own text renders 182 as line 7 —
            // markup, and an assignment to nothing. Nothing is lost by dropping it, because
            // the footnote below names the controller *and* the line of the assignment that
            // typed this variable, which is the line that inner hop is written on.
            typed.derivation.assignment = None;
            typed.derivation.renderer = Some(FromRenderer {
                renderer: rendered.name.clone(),
                controller: rendered.controller,
                // `*at` and not `at_in_graph`: this names a line for a reader, in the text that
                // was just read, which is the buffer's — the same rule that keeps
                // `Receiver::Assigned`'s offset untranslated one module over.
                line: line_of(&source, *at),
            });
            return Some(typed);
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

/// The last rung: a class spelled like the receiver, resolved in the nesting the cursor is in.
///
/// `@user` is a `User`, `person` is a `Person`, `first_name` is a `FirstName`. Rails names
/// instance variables after their classes and so does most Ruby, which is why this is worth
/// having at all — and it is a guess in the strict sense: nothing in the code says so, and the
/// tier vocabulary is what makes it safe to ship. The card and the completion row both say the
/// answer was guessed, and [`Sources::guess`] turns it off.
///
/// The name is resolved as a constant would be, outwards through the nesting: `User` written
/// inside `Admin::UsersController` reaches `Admin::UsersController::User`, then `Admin::User`,
/// then `User`. In a template the nesting is the top level, which is where a Rails model lives.
fn guessed(graph: &Graph, name: &str, scope: &Scope) -> Option<Typed> {
    let class = class_named_like(name)?;
    let nesting = scope
        .nesting_id(graph)
        .and_then(|id| graph.declarations().get(&id))
        .map(Declaration::name);
    Some(Typed {
        declaration: constant_named(graph, nesting, &class)?,
        derivation: Derivation {
            guess: Some(name.to_owned()),
            ..Derivation::default()
        },
    })
}

/// `@user_session` -> `UserSession`, and `None` for a spelling no constant could have.
///
/// The inflection is [`rails::camelize`]'s, shared rather than copied: `user_sessions/` naming
/// a `UserSessionsController` and `@user_session` naming a `UserSession` are the same rule, and
/// two inflectors are two inflectors that disagree.
///
/// The guard before it is Ruby's: a receiver spelled `valid?`, `[]` or `+` is a method whose
/// name is punctuation, and no constant is spelled that way. Sigils lead and are stripped —
/// `@@count` is a `Count` — and everything after them has to be a name.
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

/// A constant resolved the way Ruby resolves one: outwards through the nesting, then the top
/// level.
///
/// Not [`locator::locate`]'s job, and this is the difference: that one reads a reference
/// rubydex recorded against the real nesting and the real ancestors, which is a great deal more
/// than a name match. There is no reference here to read — nobody wrote `User` anywhere — so
/// what is left is the lexical half of the lookup, spelled out. The ancestors are deliberately
/// not walked: a guess reaching through an inheritance chain is a guess with more ways to be
/// wrong and no more ways to be right.
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
/// This goes through the locator rather than re-deriving Ruby's constant lookup: the reference
/// under the cursor was resolved by rubydex against the real nesting and the real ancestors,
/// which is a great deal more than a name match would be.
#[must_use]
pub fn constant_at(graph: &Graph, uri_id: UriId, offset: u32) -> Option<DeclarationId> {
    locator::locate(graph, uri_id, offset)
        .into_iter()
        .find_map(|located| match located.target {
            locator::Target::Constant(_) => locator::resolve(graph, &located)
                .declarations
                .into_iter()
                .next(),
            _ => None,
        })
}

/// The **model** a receiver is about, for [`Return::Element`] and [`Return::Collection`].
///
/// Two spellings and both of them are names this crate or rubydex made rather than names a
/// user wrote: `Story::Relation` is the class this crate invents for a collection, and
/// `Story::<Story>` is rubydex's own name for a singleton class. So the question "which model
/// is this receiver about" is answered off the receiver's *name*, which is the only thing the
/// two shapes have in common.
///
/// **Anything else answers `None`, and that is the safety.** These two returns are declared on
/// exactly two kinds of body — the relation base class and a model's own class side — so a
/// receiver that is neither a relation nor a class object has reached one of them through an
/// ancestor nobody meant, and naming the receiver itself would be a guess. A `None` stops the
/// chain, which is what every other type this table cannot answer already does.
fn model_of(graph: &Graph, receiver: DeclarationId) -> Option<String> {
    let name = graph.declarations().get(&receiver)?.name();
    if let Some(element) = rails::element_of(name) {
        return Some(element.to_owned());
    }
    name.rsplit_once("::<").map(|(class, _)| class.to_owned())
}

/// The declaration a name refers to, when the graph holds one.
///
/// A `DeclarationId` is a hash of the name, so this builds the key without a lookup — but the
/// lookup still has to happen, because an id for a declaration that was never indexed is a
/// perfectly well-formed id that answers nothing.
#[must_use]
pub fn declared(graph: &Graph, name: &str) -> Option<DeclarationId> {
    let id = DeclarationId::from(name);
    graph.declarations().contains_key(&id).then_some(id)
}

/// What a signature says this constant holds, as a receiver's type.
///
/// The rung is the second of the five — a signature was read — so it is *derived* and the card
/// says so. It is the only rung reached from a `Receiver::Constant`, which is otherwise either
/// a class object (the name is the type) or nothing at all.
///
/// **The class is looked up rather than trusted**: a signature may name a class nothing in this
/// workspace declares, and an id built from such a name is a hash that answers nothing. Where
/// the lookup misses the caller falls through to the arms below exactly as it did before this
/// existed, which is what makes the rung purely additive.
fn held_by(sources: &Sources<'_>, constant: DeclarationId) -> Option<Typed> {
    let held = sources.types.held(constant)?;
    let declaration = declared(sources.graph, &held.class)?;
    Some(Typed {
        declaration,
        derivation: Derivation {
            constant: Some(held.constant.to_string()),
            ..Derivation::default()
        },
    })
}

/// What the Ruby that **assigns** this constant says it holds.
///
/// The second rung — an assignment was read — so it is *derived* and the card says so, and it
/// is the sibling of [`held_by`] in the other language. `ENV` is typed by a signature because
/// Ruby's own `sig/` has one; an application's own configuration constant is typed by the line
/// that builds it, because no signature anybody ships will ever mention it.
///
/// **Read out of the file the graph says declares it, not out of a name match.** rubydex files
/// a `Definition::Constant` under the span of the name it writes, and
/// [`cursor::constant_assignment`] finds that exact span in the text — so two `HANDLE`s in two
/// namespaces are two spans and never each other's answer.
///
/// **The last assignment that produces a type wins** — last by document and then by offset,
/// which is the order below, walked backwards. That is the rule the instance-variable rung
/// beside it already follows, and the footnote names the file and the line so a reader can see
/// which one it was. A constant assigned twice is a warning in Ruby rather than a shape worth a
/// policy of its own; a constant assigned in a file the reader has open and again in one they
/// have not is the case the footnote is for.
///
/// **The assignment is resolved in its own document's coordinates and its own nesting**, both
/// of which are somebody else's: a `CONFIG = Settings.new` written inside `module Store` names
/// `Store::Settings`, and reading that constant against the *caller's* nesting would resolve it
/// somewhere else or nowhere at all.
fn assigned_to(sources: &Sources<'_>, constant: DeclarationId) -> Option<Typed> {
    // The cycle guard. See `Sources::constant_hops`: this is the one rung in the module that
    // can arrive back at the constant it started from.
    if sources.constant_hops >= CONSTANT_HOPS {
        return None;
    }
    let graph = sources.graph;
    let name = graph.declarations().get(&constant)?.name().to_owned();
    let deeper = Sources {
        constant_hops: sources.constant_hops + 1,
        ..*sources
    };

    // A constant reopened across files is answered in a stable order — sorted rather than the
    // graph's, so the answer does not depend on the order the walk happened to index in, which
    // is `from_controller`'s rule for the same reason.
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
        // **The graph's span, in the buffer's coordinates** — the opposite direction from
        // `from_controller`, and for the opposite reason. That rung searches the buffer by name
        // and has to translate what it found *into* the graph; this one starts from a span the
        // graph recorded and has to find it in text the user may have been editing since. A
        // `None` is a span overlapping an edit that has not settled, and falling through is how
        // every rung here declines rather than reading whichever constant is at that offset now.
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
        // Parsed out of that document's *buffer*, and everything below keys the graph.
        let Some(receiver) = receiver.rebased(&rebase) else {
            continue;
        };
        let scope = Scope::at(graph, *written_in, *start);
        let Some(mut typed) = method_receiver(&deeper, *written_in, &receiver, &scope) else {
            continue;
        };
        typed.derivation.assigned_constant = Some(FromAssignment {
            constant: name.clone(),
            file: where_written(sources, uri),
            // `from` and not `start`: this names a line for a reader, in the text that was just
            // read, which is the buffer's — the same rule `from_controller` keeps one rung up.
            line: line_of(&source, from),
        });
        return Some(typed);
    }
    None
}

/// The file `uri` names, as short a path as still identifies it for a reader.
///
/// Relative to the workspace where the file is in it, which is the case the footnote exists
/// for — an initializer eight directories down is unreadable as an absolute path and obvious as
/// a relative one. A file outside the workspace is a gem's, and there the whole path is what is
/// left: it is long and mostly a version number, and it is still a thing an editor can open,
/// which a bare file name is not.
///
/// **Never empty**, and the caller relies on it: a URI this cannot parse is printed as itself
/// rather than dropped, so the footnote always names a place and `hover::provenance` has one
/// sentence to render instead of two.
fn where_written(sources: &Sources<'_>, uri: &str) -> String {
    let Some(path) = DocUri::from_uri_str(uri).and_then(|uri| uri.to_path()) else {
        return uri.to_owned();
    };
    let root = DocUri::from_uri_str(sources.layout.root).and_then(|root| root.to_path());
    match root.and_then(|root| path.strip_prefix(root).ok().map(Path::to_path_buf)) {
        Some(relative) => relative.display().to_string(),
        None => path.display().to_string(),
    }
}

/// Whether rubydex invented this namespace because something named it and nothing defined it.
///
/// Deliberately not folded into [`singleton_of`], which `completion`'s `Distance` also calls to
/// *rank* rows rather than to decide what a receiver is: the two want different answers.
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
    /// The refusals are the half worth reading: a constant is *one* thing, so a union names no
    /// class whose members can be offered and `untyped` names nothing at all. Both fall through
    /// to the arms that were there before this table, which is what makes the rung additive.
    #[test]
    fn what_a_signature_says_a_constant_holds() {
        let mut types = Types::new();
        assert!(types.harvest(
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
            // A generic's head, for `class_of`'s reason: `Array[String]` and `Array[Integer]`
            // reach the same declarations, and the element type is a question this module does
            // not answer.
            "ARGV",
            // `String?` is `String | nil`, and the one inexact entry the return side has too.
            "MAYBE",
            "UNKNOWN",
            "EITHER",
            "LITERAL",
            "NOTHING",
            // Nesting is the constant's own, so two `INFINITY`s in two classes are two keys.
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
                ("LITERAL", None),
                ("NOTHING", None),
                ("Float::INFINITY", Some("Float")),
                ("Deep::Inner::HANDLE", Some("String")),
                ("INFINITY", None),
            ]
        );

        // And the two `Return`s that name the declaring class — `instance` and `class` — cannot
        // reach this at all, because RBS's own grammar refuses them in a constant's type. The
        // whole document is rejected rather than the one line, which is why this is asserted
        // here rather than as another `None` row above: a harvest that returned `false` would
        // have taken the other constants with it.
        assert!(!Types::new().harvest("class Holder\n  SELFISH: instance\nend\n"));
    }

    /// The table built from one RBS document, as `method -> class` lines, sorted.
    ///
    /// A row per way a *call* can be written rather than per method, because that is what the
    /// table is now keyed by: `Text#round/0` is what a call with no arguments reaches, `/1` one
    /// with an argument, `/2+` two or more, and a trailing ` { }` marks the block side. Printed
    /// rather than asserted per entry: the policy is a *set* of decisions and reading them side
    /// by side is what shows one arm answering where its neighbour does not.
    fn harvested(source: &str) -> Vec<(String, String)> {
        // The names are rebuilt here rather than kept, because the table is keyed by a hash and
        // a hash cannot be printed back. Every name a test asks about has to be spelled the way
        // the harvest spelled it, which is exactly the property under test.
        let mut types = Types::new();
        types.harvest(source);
        let mut rows: Vec<(String, String)> = Vec::new();
        for name in candidate_names(source) {
            let Some(overloads) = types.returns.get(&DeclarationId::from(name.as_str())) else {
                continue;
            };
            // The rubydex key carries empty parentheses; what follows here is the call's own
            // shape, and two sets of them side by side would read as one.
            let called = name.strip_suffix("()").unwrap_or(&name);
            for (label, arity) in probes(overloads) {
                let plain = overloads.plain.at(arity);
                let with_block = overloads.with_block.at(arity);
                if let Some(returns) = plain {
                    rows.push((format!("{called}{label}"), spelled(returns)));
                }
                // Only where the block changes the answer, which is what the fixtures are
                // about; most methods answer the same either way and a second identical row is
                // noise.
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
        types.harvest(source);
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
                // Two parameters are kept in order, so `|_, index|` reaches the second.
                ("Relation#agreeing[0]".to_owned(), "Story".to_owned()),
                ("Relation#each[0]".to_owned(), "Story".to_owned()),
                ("Relation#each_with_index[0]".to_owned(), "Story".to_owned()),
                (
                    "Relation#each_with_index[1]".to_owned(),
                    "Integer".to_owned()
                ),
                // `?{ }` is read too: the call that reaches this wrote a block whatever the
                // signature permitted, so what the signature says that block receives applies.
                ("Relation#optional[0]".to_owned(), "Story".to_owned()),
                // `self` means the receiver, exactly as it does for a return type.
                ("Relation#tap[0]".to_owned(), "self".to_owned()),
                // The two tables really are independent: a method whose *return* the policy
                // refuses still says what its block receives. It is what would let the
                // relation declare `map` — whose element type is a block's return and so
                // cannot be typed — and still type the block's parameter.
                ("Relation#untyped_return[0]".to_owned(), "Story".to_owned()),
            ]
        );
        // What is absent is the test: a method with no block, a block with no parameters, a
        // parameter the policy refuses, and — the one that matters — two arms that disagree
        // about what the block is handed, which is an answer that depends on which overload the
        // reader meant.
    }

    /// Every arity worth asking one method about: each one an arm names exactly, and then one
    /// past the last, which is where a rest parameter answers and nothing else does.
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

    /// A [`Return`] as the table's own tests read it. `self` stays `self`, because that is what
    /// the table holds — resolving it needs a receiver and the table has none.
    fn spelled(returns: &Return) -> String {
        match returns {
            Return::Class(class) => class.to_string(),
            Return::Same => "self".to_owned(),
            Return::Element => ELEMENT.to_owned(),
            Return::Collection => COLLECTION.to_owned(),
        }
    }

    /// Every name the harvest could have filed a method under, from the same document.
    ///
    /// A second, dumber walk: it exists so the assertions can name a method rather than a hash,
    /// and it is deliberately not shared with the harvest itself.
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
        // Every arm of `class_of`, side by side, so that what is taken and what is dropped can
        // be read as one decision rather than twelve.
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
  def nothing: () -> void
  def anything: () -> untyped
  def nil_only: () -> nil
  def a_proc: () -> ^() -> void
  def a_tuple: () -> [String, Integer]
  def a_record: () -> { name: String }
  def an_interface: () -> _ToS
  def a_literal: () -> :symbol
end
";
        assert_eq!(
            drawn(source),
            "\
Policy#generic/0 -> Array
Policy#klass/0 -> Policy::<Policy>
Policy#made/0 -> Policy
Policy#me/0 -> self
Policy#named_singleton/0 -> Policy::<Policy>
Policy#optional/0 -> String
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
        // `def self.new: () -> instance` is the shape every RBS constructor is written in, and
        // it is the one place `self` and `instance` differ by more than a word.
        assert_eq!(
            drawn("class Widget\n  def self.new: () -> instance\nend\n"),
            "Widget::<Widget>#new/0 -> Widget"
        );
    }

    #[test]
    fn a_module_function_is_filed_on_both_sides() {
        // `def self?.` declares the method twice and rubydex files two declarations, so both
        // have to be in the table or half the calls to it answer nothing.
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
        // Two arms naming one class is one answer; two arms naming two is a union with a
        // different spelling, and unions are dropped. Both arms take one argument, so it is a
        // one-argument call that reaches them — a call writing none reaches neither, which is
        // `a_call_no_arm_accepts_is_answered_for_by_none_of_them`.
        assert_eq!(
            drawn(
                "class Text\n  def sub: (String) -> String\n         | (Regexp) -> String\nend\n"
            ),
            "Text#sub/1 -> String"
        );
        // Two *blockless* arms naming two classes is the union this rule is for. Written with
        // a block it would not be one — see `a_block_tells_two_overloads_apart`.
        assert_eq!(
            drawn(
                "class Text\n  def each: (Integer) -> Enumerator\n          | (String) -> Array\nend\n"
            ),
            ""
        );
        // One usable arm beside one the policy refuses is still refused: the method can return
        // something this table cannot name.
        assert_eq!(
            drawn("class Text\n  def maybe: () -> String\n           | () -> void\nend\n"),
            ""
        );
    }

    #[test]
    fn a_block_tells_two_overloads_apart() {
        // The shape 139 of core's method definitions have, and the reason they are not unions:
        // which arm applies is decided by whether the caller wrote a block, which is syntax the
        // cursor has already read. Both answers are exact.
        assert_eq!(
            drawn(
                "class Text\n  def bytes: () -> Array[Integer]\n           | () { (Integer byte) -> void } -> self\nend\n"
            ),
            "\
Text#bytes/0 -> Array
Text#bytes/0 { } -> self"
        );
        // An arm with a *required* block answers only the block side; a call without one has
        // nothing to reach.
        assert_eq!(
            drawn("class Text\n  def each: () { (String) -> void } -> self\nend\n"),
            "Text#each/0 { } -> self"
        );
        // `?{ ... }` is an optional block, so its arm applies whichever way the call is
        // written and both sides answer the same.
        assert_eq!(
            drawn("class Text\n  def count: () ?{ (String) -> bool } -> Integer\nend\n"),
            "Text#count/0 -> Integer"
        );
        // And the rule does not rescue a side that disagrees with itself: two blockless arms
        // naming two classes is still a union, block arms or no block arms.
        assert_eq!(
            drawn(
                "class Text\n  def mixed: (Integer) -> Enumerator\n           | (String) -> Array\n           | () { (String) -> void } -> self\nend\n"
            ),
            "Text#mixed/0 { } -> self"
        );
    }

    #[test]
    fn an_arity_tells_two_overloads_apart() {
        // The other half of the same argument the block makes, and `Float#round` is the shape
        // it is for: read as one partition the two arms name `Integer` and a union and both are
        // dropped, read by arity the zero-argument side agrees with itself. Not a guess and not
        // a tier — the same kind of fact `"x".bytes.` already gets.
        assert_eq!(
            drawn(
                "class Text\n  def round: (?half: Symbol) -> Integer\n          | (Integer digits, ?half: Symbol) -> (Integer | Float)\nend\n"
            ),
            "Text#round/0 -> Integer"
        );
        // Keywords are not positional arguments on either side of the question — the arm that
        // answered above declares only `?half:`, and `3.7.round(half: :up)` writes only
        // `half:`. `cursor::arity_of` is the half of that rule this module cannot see.
        //
        // And both sides of a split can answer, with a different class each: `Array#first` is
        // this shape in real signatures, where the zero-argument arm is a type variable and is
        // dropped for a second reason. See `types.md`.
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
        // `?String` is to the arity what `?{ }` is to the block: one arm answering on both
        // sides of the split rather than one arm per side.
        assert_eq!(
            drawn("class Text\n  def strip: (?String chars) -> String\nend\n"),
            "\
Text#strip/0 -> String
Text#strip/1 -> String"
        );
        // A rest parameter has no most, so it answers past every arity an arm names exactly —
        // which is what the `+` row is. `(Integer, *String)` starts at one and never stops.
        assert_eq!(
            drawn("class Text\n  def join: (Integer, *String) -> String\nend\n"),
            "\
Text#join/1 -> String
Text#join/2+ -> String"
        );
    }

    #[test]
    fn a_call_no_arm_accepts_is_answered_for_by_none_of_them() {
        // The rule that keeps this a reading of the signature rather than a guess at it: a call
        // writing an argument no arm takes does not fall to the nearest arm, it reaches none.
        // `"x".sub.` is not a `String` — it is a `LocalJumpError`'s cousin, and RBS says so.
        assert_eq!(
            drawn("class Text\n  def sub: (String) -> String\nend\n"),
            "Text#sub/1 -> String"
        );
        // Over a thousand methods in Ruby's own signatures require an argument, so a zero-argument
        // call reaches no arm and answers nothing rather than answering the only arm there is.
        // `types.md` has the decision.
        let mut types = Types::new();
        types.harvest("class Text\n  def sub: (String) -> String\nend\n");
        let id = DeclarationId::from("Text#sub()");
        assert_eq!(types.returns(id, Arity::Exactly(0), false), None);
        assert_eq!(types.returns(id, Arity::Exactly(2), false), None);
        // And a call whose arguments cannot be counted gets what every arm agrees on, which is
        // the answer it got before arity was read at all. `foo.sub(*args).` is the whole reason
        // this is a partition and not a filter.
        assert_eq!(
            types.returns(id, Arity::Unknown, false),
            Some(&Return::Class("String".into()))
        );
    }

    #[test]
    fn an_interfaces_members_are_not_in_the_table() {
        // The rule `signatures::without_interfaces` enforces on the text, enforced here on the
        // walk: an interface's methods never enter the graph, so an entry for one would be
        // keyed by a declaration that does not exist — or, worse, by the enclosing class's.
        assert_eq!(
            drawn(
                "class Array\n  interface _Rand\n    def rand: () -> Integer\n  end\n  def sample: () -> String\nend\n"
            ),
            "Array#sample/0 -> String"
        );
    }

    #[test]
    fn a_return_a_label_can_carry_is_one_the_whole_signature_agrees_on() {
        // `returns` is asked about a call and reads what the call wrote; this is asked about the
        // `def`, where there is no call to read. So the two sides of the block have to agree, or
        // a label on the declaration would be wrong half the times it was read.
        let mut types = Types::new();
        types.harvest(
            "class Text\n  \
             def upcase: () -> String\n  \
             def bytes: () -> Array[Integer]\n         \
             | () { (Integer byte) -> void } -> self\n  \
             def each: () { (String line) -> void } -> self\n\
             end\n",
        );

        // One side, so there is nothing to disagree with it.
        assert_eq!(
            types.declared_return(DeclarationId::from("Text#upcase()")),
            Some(&Return::Class("String".into()))
        );
        // The other side only: every arm declares a block, so the plain side has no arms at all
        // rather than arms that contradict.
        assert_eq!(
            types.declared_return(DeclarationId::from("Text#each()")),
            Some(&Return::Same)
        );
        // Both sides, and they disagree — which is exactly the shape a label must refuse.
        assert_eq!(
            types.declared_return(DeclarationId::from("Text#bytes()")),
            None
        );
        // And a method the table has never heard of.
        assert_eq!(
            types.declared_return(DeclarationId::from("Text#gone()")),
            None
        );
    }

    #[test]
    fn the_tier_is_the_weakest_rung_the_answer_rests_on() {
        // The one place the three tiers are a value rather than a sentence. A guess wins over
        // everything above it — a chain read through three signatures that *ended* at a guess is
        // a guess — which is why the field is tested first and not last.
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

        // Every other kind of provenance is derived, and each is named here so that another
        // one cannot be added without this test being read: the destructuring in `tier` makes it
        // a compile error, and this is what says which answer the new arm should give.
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
        types.harvest("class Foo\n  def");
        assert!(types.is_empty());
    }

    #[test]
    fn harvesting_the_same_document_twice_overwrites() {
        let mut types = Types::new();
        types.harvest("class Foo\n  def bar: () -> String\nend\n");
        types.harvest("class Foo\n  def bar: () -> Integer\nend\n");
        assert_eq!(types.len(), 1);
        assert_eq!(
            types.returns(DeclarationId::from("Foo#bar()"), Arity::Exactly(0), false),
            Some(&Return::Class("Integer".into()))
        );
        types.clear();
        assert!(types.is_empty());
    }

    #[test]
    fn a_method_outside_every_declaration_has_no_owner() {
        // An interface's members never enter the graph, so they never enter the table — that is
        // the rule above. This is the other way a method can have nothing owning it: rbs will
        // parse a bare `def` at the top level of a file, and there is no declaration for it to
        // be keyed under.
        let mut types = Types::new();
        types.harvest("interface _Free\n  def x: () -> String\nend\n");
        assert!(types.is_empty());

        // The guard in the walk is for the shape rbs will not give it: a `def` outside every
        // declaration. Asserted here rather than left as an unexplained arm, because "rbs
        // refuses this" is the whole reason the arm can never be taken.
        assert!(parse("def free: () -> String\n").is_err());
        types.harvest("def free: () -> String\n");
        assert!(types.is_empty());
    }

    #[test]
    fn rubydex_spells_a_signature_the_way_this_keys_it() {
        // The contract the whole module rests on, asserted against a real graph rather than
        // against this module's own idea of it. Every name here is one the harvest builds, and
        // a rubydex that renamed any of them would make every lookup miss in silence.
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
        types.harvest(source);

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
    /// A table rather than a test each, because the interesting half is the `None`s: a guess
    /// that fires on `valid?` or `[]` is a guess that offers a class's methods for something
    /// that is not a thing at all.
    #[test]
    fn the_class_a_receiver_is_spelled_like() {
        let rows = [
            ("@user", Some("User")),
            ("person", Some("Person")),
            ("first_name", Some("FirstName")),
            // Both sigils, because both are stripped and the rule is Ruby's rather than ours.
            ("@@count", Some("Count")),
            ("@user_session", Some("UserSession")),
            // Ruby constants begin with an ASCII capital. Nothing else can name a class, so
            // there is no point building a name to look one up by.
            ("_", None),
            ("@", None),
            ("123", None),
            // A method whose name is punctuation is not the name of a thing.
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
        // Ruby's lexical lookup, and the reason it is spelled out here rather than asked of
        // rubydex: there is no reference to resolve, because nobody wrote `Story` anywhere.
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

        // Innermost first: written inside `Admin::Posts`, `Story` is `Admin::Story`.
        assert_eq!(
            constant_named(&graph, Some("Admin::Posts"), "Story"),
            declared(&graph, "Admin::Story")
        );
        // And outwards to the top level, which is where a template's nesting starts.
        assert_eq!(
            constant_named(&graph, Some("Object"), "Story"),
            declared(&graph, "Story")
        );
        // No nesting at all is the same lookup with nothing to walk, rather than no lookup.
        assert_eq!(
            constant_named(&graph, None, "Story"),
            declared(&graph, "Story")
        );
        assert_eq!(constant_named(&graph, None, "Ghost"), None);
    }

    #[test]
    fn ruby_s_own_signatures_are_what_the_policy_was_written_against() {
        // A fixture over real core signatures rather than hand-written RBS: the policy is a
        // claim about a corpus, and a document written to exercise it can only ever agree with
        // itself. These three files are in the repository, so this needs no Ruby and no
        // network.
        let mut types = Types::new();
        for file in ["string.rbs", "hash.rbs", "array.rbs", "float.rbs"] {
            let path = format!("vendor/rbs/core/{file}");
            types.harvest(&std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("{path}")));
        }

        let called = |method: &str, arity: u32, block: bool| {
            types
                .returns(DeclarationId::from(method), Arity::Exactly(arity), block)
                .map_or_else(|| "(none)".to_owned(), spelled)
        };
        let answers = |method: &str| called(method, 0, false);
        let with_block = |method: &str| called(method, 0, true);

        // Taken. `upcase` and `sort` are several overloads that all name one class; `keys` is a
        // generic whose head is the answer however it is parameterised; `upcase!` is `self?`,
        // which is the optional rule over the `self` rule, and stays `self` in the table
        // because what `self` is depends on the receiver rather than on the signature.
        assert_eq!(answers("String#upcase()"), "String");
        assert_eq!(answers("String#upcase!()"), "self");
        assert_eq!(answers("Hash#keys()"), "Array");
        assert_eq!(answers("Array#sort()"), "Array");

        // `bytes` declares `() -> Array[Integer]` and `() { (Integer) -> void } -> self`. Not
        // a union: which arm applies is decided by the block, and both answers are exact.
        assert_eq!(answers("String#bytes()"), "Array");
        assert_eq!(with_block("String#bytes()"), "self");
        // `round` declares `(?half: ...) -> Integer` beside `(int, ?half: ...) -> (Integer |
        // Float)`. Not a union either: which arm applies is decided by whether the caller wrote
        // a digit count, and the zero-argument side agrees with itself.
        assert_eq!(answers("Float#round()"), "Integer");
        assert_eq!(called("Float#round()", 1, false), "(none)");

        // Dropped, and one of them for two reasons at once. `gsub` declares three blockless
        // arms of one arity naming two classes — a union, and no block anywhere to tell them
        // apart.
        assert_eq!(answers("String#gsub()"), "(none)");
        // `first` declares `() -> E` and `(int count) -> Array[E]`, which arity does tell
        // apart: `[1, 2].first(3)` is an `Array`, exactly. The zero-argument side is still
        // nothing, because a type variable is not a class — the half of it only generic
        // instantiation would fix. See `types.md`.
        assert_eq!(answers("Array#first()"), "(none)");
        assert_eq!(called("Array#first()", 1, false), "Array");

        // Pinned rather than bounded, and for the reason `rbs-signatures.md` gives: bumping
        // `vendor/rbs` changes the answers ya-lsp gives, so it should be a visible failure
        // rather than a silent drift. It moves on a policy change too, which is the other half
        // of what this number is for — reading the block apart from the arms without one took
        // it from 164 to 220 across the first three files, and reading the arity apart as well
        // took those same three to 230. `float.rbs` is the fourth, added for `round`, and it
        // brings the four to 250.
        assert_eq!(types.len(), 250, "methods typed across four core files");
    }

    #[test]
    fn a_chain_completes_against_what_the_signature_says_it_returns() {
        // A chain through signatures, end to end and through the wire. Without it every line
        // here answers `(unrecognised)` —
        // the name-based list, every method in the graph — before the return-type table
        // existed.
        let (mut harness, uri) = with_types("");
        assert_eq!(class_at(&mut harness, &uri, "\"hi\".upcase.~"), "String");
        // A chain of two, which is the property that makes it a chain rather than one step.
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".upcase.upcase.~"),
            "String"
        );
        // A link that changes class, and then a link on the class it changed to.
        assert_eq!(class_at(&mut harness, &uri, "\"hi\".length.~"), "Integer");
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".length.succ.~"),
            "Integer"
        );
        // A generic: the head is the answer, and the element type is a question nothing here
        // asks.
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
    fn self_in_a_signature_is_the_receiver_and_not_the_class_that_declared_it() {
        // `Kernel#tap` returns `self`. Resolving that where the signature is *written* answers
        // `Kernel` — three methods, none of them the receiver's. The lookup goes through
        // rubydex's ancestor walk and the `self` is resolved against what the call was made on.
        //
        // Written with a block, because rbs declares `tap` with a required one and Ruby raises
        // `LocalJumpError` without it. Which is the second thing this pins: an arm whose block
        // is required is not what a blockless call reaches.
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
        // `type_the_local` does not stop at literals and `.new`.
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
        // The example that is easy to get wrong. `String#bytes`
        // declares `() -> Array[Integer]` and `() { (Integer) -> void } -> self`. That is not a
        // union: which arm applies is decided by whether a block was written, which is syntax
        // the cursor has already read. Both answers are exact, and they are different.
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
        // The block's argument, made for the arity. `Float#round` declares `(?half: ...) ->
        // Integer` beside `(Integer, ?half: ...) -> (Integer | Float)`. Read as one partition
        // those disagree and both go; read by arity the zero-argument side agrees with itself,
        // and `3.7.round.` is an `Integer`. Not a guess and not a tier.
        let (mut harness, uri) = with_types("");
        assert_eq!(class_at(&mut harness, &uri, "3.7.round.~"), "Integer");
        // Keywords are not positional arguments on either side of the question.
        assert_eq!(
            class_at(&mut harness, &uri, "3.7.round(half: :up).~"),
            "Integer"
        );
        // And the arm the digit count reaches is a union, which arity does not rescue.
        let digits = class_at(&mut harness, &uri, "3.7.round(1).~");
        assert!(digits.starts_with("(everything"), "{digits}");
        // `Array#first` is the other half, and the half `types.md` had wrong: `() -> E` is a
        // type variable and stays dropped, while `(Integer count) -> Array[E]` is a class and
        // answers now. Generic instantiation would have fixed the first and not the second.
        assert_eq!(class_at(&mut harness, &uri, "[1, 2].first(3).~"), "Array");
        let bare = class_at(&mut harness, &uri, "[1, 2].first.~");
        assert!(bare.starts_with("(everything"), "{bare}");
    }

    #[test]
    fn a_call_the_signature_cannot_accept_falls_back_rather_than_to_the_nearest_arm() {
        // The rule that keeps this a reading of the signature rather than a guess at it, and
        // the one place the split takes an answer away. `scan` has one arm and requires a
        // pattern, so `"hi".scan.` reaches no arm at all rather than answering `Array`. Over a
        // thousand methods in Ruby's own signatures are in that position; `types.md` has the
        // argument.
        let (mut harness, uri) = with_types("");
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".scan(\"a\").~"),
            "Array"
        );
        let bare = class_at(&mut harness, &uri, "\"hi\".scan.~");
        assert!(bare.starts_with("(everything"), "{bare}");
        let too_many = class_at(&mut harness, &uri, "\"hi\".scan(\"a\", 1).~");
        assert!(too_many.starts_with("(everything"), "{too_many}");
        // A call whose arguments cannot be counted gets what every arm agrees on, which is the
        // answer it got before arity was read at all. That is what makes this a partition.
        assert_eq!(
            class_at(&mut harness, &uri, "\"hi\".scan(*args).~"),
            "Array"
        );
    }

    #[test]
    fn a_chain_through_something_the_table_dropped_answers_nothing_exact() {
        // The `Unknown` path at the new tier, which is the half that a green suite hides: the
        // fallback silently absorbing a bug is how this fails without anyone noticing. Each of
        // these has to reach the name-based list rather than a class.
        let (mut harness, uri) = with_types("");
        // `Array#first` returns the type variable `E`. Dropped, so the chain stops here.
        let dropped = class_at(&mut harness, &uri, "\"hi\".scan(\"a\").first.~");
        assert!(dropped.starts_with("(everything"), "{dropped}");
        // `String#sub` declares two arms returning two classes. A union, dropped.
        let union = class_at(&mut harness, &uri, "\"hi\".sub(\"a\").~");
        assert!(union.starts_with("(everything"), "{union}");
        // A method no signature declares at all.
        let absent = class_at(&mut harness, &uri, "\"hi\".nonesuch.~");
        assert!(absent.starts_with("(everything"), "{absent}");
        // And a receiver that was never anything: one `Unknown` ends the chain.
        let nothing = class_at(&mut harness, &uri, "thing.upcase.~");
        assert!(nothing.starts_with("(everything"), "{nothing}");
    }

    #[test]
    fn an_instance_variable_completes_against_what_its_class_assigned_it() {
        // An instance variable, end to end, in the shape a Rails controller is written in: assigned once in
        // one method, read in every other. This is the most common receiver in an application
        // that ya-lsp answered `Unknown` for, and it needs no annotation from anybody.
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

        // Through a chain, which is the three derived rungs composing: the ivar is typed by an
        // assignment,
        // the assignment by a signature, and the cursor sits one link past both.
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
        // The `Unknown` path for an instance variable, and the one that would be a *wrong*
        // answer rather than
        // an absent one: `@seed` in `def self.build` belongs to the class object, and joining
        // it to the instance's `@seed` would offer `String`'s methods for something that has
        // never been a string.
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
    /// The trailing comments are the needles: a hover fixture points at the *start* of what it
    /// searches for, so each row needs a spelling of its own method name that appears once.
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
  end
end

module Counting
  def tally
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
        // `resolve_typed` runs on every call the graph could not resolve, and most of those
        // have no receiver written: a bare `render` is a call on an implicit `self`, whose type
        // is a question about the enclosing class rather than about the text. There is nothing
        // for the new rung to do, so the answer is the name-based one it always was.
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
        // **rubydex records a `Class.new(base) do … end` body as an anonymous class**, so the
        // `self` a cursor inside one stands in is that class rather than the instance the
        // enclosing method was running on. Ruby agrees about the block's own `self` — and says
        // nothing about a local, which closes over the `self` of the line that *wrote* it.
        //
        // `Receiver::SelfObject` carries that line's offset for exactly this. Read against the
        // cursor's scope instead, `held` was the anonymous class: a class with none of the
        // captured instance's members, and one `locator::missed` will not name — so the card
        // said the receiver's type was unknown while `completion`, typing the same receiver the
        // same way, listed the anonymous class's members at the same byte. Two chatwoot
        // positions, found by the audit's check 6 rather than by a test.
        let mut harness = Harness::new();
        let source = "class Thing\n  def go\n    held = self\n    Class.new(Object) do\n      \
                      held.frob\n    end\n  end\n\n  def frob\n  end\nend\n";
        let uri = harness.write("app/thing.rb", source);
        harness.index();

        // `find` takes the first `frob`, which is the one inside the block.
        let markdown = card(&mut harness, &uri, source, "frob");
        assert!(markdown.contains("Thing#frob"), "{markdown}");
        assert!(
            !markdown.contains("Matched on the method name alone"),
            "and exactly, not by the name: {markdown}"
        );

        // The other half of the same fact. `method_receiver` is shared so that the two surfaces
        // cannot disagree about what `held.` is, and the disagreement is what made this visible.
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
        // The constant is not the class, which is the whole of the problem: `ENV` holds an
        // *instance*, so the graph has a constant with no singleton to walk and rubydex has
        // promoted it to a namespace it invented. The signature is the only thing that says
        // what the object is, and it says it as plainly as a `-> String` does.
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

        // **A class the signature names and nothing declares answers nothing extra.** The
        // lookup is by name and an id built from a name nothing declares is a hash that
        // answers nothing, so the rung falls through to the arms that were there before it —
        // here, the name-based list, which has the only `upcase` in this workspace.
        let ghostly = card("upcase\n  end\nend");
        assert!(ghostly.contains("String#upcase"), "{ghostly}");
        assert!(
            ghostly.contains(GUESS_FOOTNOTE),
            "and it is a guess rather than a type: {ghostly}"
        );
    }

    /// The two cursors of the test above, as the text they are found by.
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
    /// A signature typed `ENV` because Ruby ships one. Nothing will ever ship a signature for an
    /// application's own configuration object, and the line that builds it says the same thing
    /// in the other language — in a file the cursor is nowhere near, which is the whole cost of
    /// the rung and the reason the footnote names it.
    ///
    /// The assignment is written **inside** the namespace it belongs to and names its class
    /// unqualified, which is the shape that makes the nesting matter: `Cabinet` means
    /// `Vault::Cabinet` there and means nothing at all where the cursor is.
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

        // **A constant whose assignment names no class this workspace declares falls through.**
        // `Missing.new` is a shape like any other and the lookup behind it is a hash of a name
        // nothing declared, so the rung answers nothing and the arms below it answer as they
        // did before it existed — here the name-based list, which has the only `combination`
        // in the workspace.
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
    /// The assignment is read out of the *buffer* — an initializer being edited types the
    /// constant before it is saved — but the span that finds it came from the graph, so the two
    /// agree only while nothing has been typed. An edit past the name is translated and the
    /// answer stands; an edit **over** the name has no honest translation, and the rung declines
    /// rather than reading whichever constant is at that offset now.
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

        // **A line typed *below* the assignment leaves it exactly where it was.** The name's
        // span is strictly inside the text both copies still agree on, so it translates and the
        // answer is the one it was before anybody started typing.
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

        // **Renaming the constant takes the span with it.** There is no offset in the new text
        // that means what the graph's does, so the rung answers nothing and the name rung
        // answers instead — which is what every other rung here does when it cannot be sure.
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
    /// The other arm of `where_written`, and the reason it is the whole path rather than the
    /// file name: a gem's path is long and mostly a version number, and it is still a thing an
    /// editor can open — which `shouty.rb` on its own is not.
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

    /// Two constants deep, and then two constants in a circle.
    ///
    /// The first half is why the hop limit is not one: `THING = FACTORY.build` over
    /// `FACTORY = Builder.new` is ordinary wiring, and the card shows both rungs it was read
    /// through. The second half is why there is a limit at all — the rung is the one place in
    /// this module that can arrive back at the constant it started from, and **the assertion is
    /// that this test returns**. A stack overflow is not a panic the request bulkhead can
    /// contain; it takes the process with it.
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

        // A circle answers nothing, which leaves the name-based list — the same thing every
        // other rung in this module does when it cannot say anything exact.
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
        // The three tiers side by side, in `GALLERY` shape. Every one of these
        // cards is individually plausible; what has to be legible is the *difference* between
        // them, and a card asserted on its own cannot show that. Read down the right-hand
        // column: nothing, a signature, a line, both, a guess, a name, a signature that
        // declared what a constant holds, the Ruby that assigned one, the three different
        // things a name match can mean, and the one scope a block is read in that the file
        // does not state.
        //
        // `held` and `constant` are the pair worth reading together. Both say a constant holds
        // an object; one names a signature and the other names a file and a line, because those
        // are the two different things a reader would have to go and check.
        //
        // **The bottom tier has four rows and they are not interchangeable.** `guessed` is the
        // only one where the server has nothing: the other three are a receiver it typed and a
        // member that is not on it, and the card says which class so that the claim can be
        // checked. `audit.md` requires every footnote `hover.rs` writes to be pinned here, whole
        // — `answers.GUESSED` matches these four on their shared opening clause, which is what
        // keeps the audit's tier reading true across all of them.
        //
        // The property under test is not the wording. It is that a reader can tell, without
        // leaving the card, which of the three things ya-lsp did to arrive at the answer.
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

  *Type taken from where `HOLDER` is assigned — `app/report.rb` line 74 — and not from what this expression says.*
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
        // The fall-through, both directions, because on its own either half is a bug.
        //
        // A chain whose *shape* is sound and whose *type* nothing states. `published` is a
        // scope this `Story` does not declare, which is what a chain through any class method
        // nobody annotated looks like — and it is deliberately not `Story.where(...).first`,
        // because the model's class side types that one. The chain here genuinely fails even
        // with the class side in place, which is how the fall-through is known to be
        // load-bearing rather than shadowed.
        //
        // Before this, that answered *nothing*, while a `story` with no assignment at all
        // reached the name rung and answered `Story`. Writing the assignment made the answer
        // worse, which is the wrong shape for a system whose whole argument is that its rungs
        // are ordered.
        let source = "story = Story.published.first\nstory.comments\n";
        let (mut harness, _story, uri) = models_project(source);

        let fell_through = card(&mut harness, &uri, source, "comments");
        assert!(
            fell_through.contains("Story#comments"),
            "the failed chain has to end where a bare `story` ends: {fell_through}"
        );
        // Wearing the label of the rung it reached. The fall-through is a *step*, not a sixth
        // rung, so it must not launder a guess into a derivation on the way past.
        assert!(
            fell_through.contains("Type guessed from the name `story` alone"),
            "{fell_through}"
        );

        // The other direction, and it is the one that makes the first safe. `comment` would
        // guess `Comment`, which really does declare `comments` — so if the spelling were
        // asked before the assignment, this card would say `Comment#comments` and be wrong
        // about a chain the code states outright.
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
        // It reaches `types::named`, which is the same pair of rungs a bare name reaches — so
        // `[types] guess_from_names = false` turns it off with no switch of its own. A
        // fall-through that survived the setting would be a guess the user had already
        // declined, arriving by a route they had no name for.
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
        // The block parameter, end to end. `Story::Relation#each` is declared
        // `() { (Story) -> void } -> Story::Relation`, and reading only the return **throws
        // the block half away**: a block parameter was typed only where its own name happened to
        // camelize onto a class, so `.each do |story|` answered by a *guess* and
        // `.each do |instance|` — which is what mastodon writes — answered nothing at all.
        let source = "Story.where(id: 1).each do |instance|\n  instance.title\nend\n";
        let (mut harness, schema, uri) = rails_project(source);
        let author = harness.write(
            "app/models/author.rb",
            "class Author < ApplicationRecord\n  has_many :stories\nend\n",
        );
        harness.watch(&[&author]);

        // The member is found, the card names the column's own file, and the jump lands on the
        // line of the schema that declared it — the same three things asked of a column
        // reached any other way.
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
    fn a_card_on_an_instance_variable_names_the_assignment_it_was_typed_from() {
        // The other half of the same answer, and the reason the two requests read one
        // resolution: `definition` jumps to the assignment and the card says the type came from
        // it. Both of them are the scope walk winning at a span the graph would have answered
        // for — at `@title = Title.new` the graph has `Story#@title`, which names the variable
        // and says nothing about what is in it.
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
        // The common case, pinned. `Story.where(...)` is a chain whose return type nothing
        // declares, so the convention answers nothing for `@stories` — and `@stories` does not
        // spell a class either, so neither does the guess. This is the ceiling on the tier, and it
        // is the annotations rather than the machinery.
        let mut harness = Harness::new();
        rails_app(&harness);
        // The template calls a method the *model* declares, deliberately: an untyped receiver
        // and a receiver typed to the wrong class both answer `Story#title`, and what tells
        // them apart is the footnote. A call to something nothing declares would answer `null`
        // either way and pin nothing.
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
        // The rung ordering, and the one thing that makes the last rung safe to ship.
        // `@user` here is assigned a `Draft` in its own class, and a class called `User` is
        // sitting in the graph waiting to be guessed at — so if the rungs were the other way
        // round, or even merely tried in parallel, this card would say `User`.
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
        // The setting exists because this is the first answer ya-lsp gives that is allowed to
        // be wrong, and a user who wants only checkable answers should be able to have them.
        // What that must *not* do is take the derived tier with it: a controller and a line is
        // an answer somebody can go and check, which is the whole difference.
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

        // The same variable, the same class in the graph, and no controller to reach it
        // through: with the guess off there is nothing left to say.
        let silenced = card(&mut harness, &elsewhere, guessed_source, "title");
        assert!(
            silenced.contains("Matched on the method name alone"),
            "{silenced}"
        );
        assert!(!silenced.contains("guessed from the name"), "{silenced}");
    }

    /// Every place a jump answers with, as `file.rb:line:character`, which is short enough to
    /// assert on whole. The file name rather than the URI, because the harness writes into a
    /// tempdir whose path is different on every run.
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
        // The card has cited a controller and a line since the view↔controller rung shipped,
        // while the jump at the identical cursor answered nothing at all — because the writes
        // were searched for in the buffer the cursor is in, and a template's are in another
        // file by construction. One convention, asked twice, now answers twice.
        let mut harness = Harness::new();
        let view = rails_app(&harness);
        harness.index();

        let source = "<h1><%= @story.title %></h1>\n";
        assert_eq!(
            jumps(&mut harness, &view, source, "@story"),
            ["stories_controller.rb:3:4"]
        );

        // And the origin is the variable the reader is standing on, not the call after it.
        let found = harness.definition_at(&view, source, "@story");
        assert_eq!(found[0]["originSelectionRange"]["start"]["character"], 8);
        assert_eq!(found[0]["originSelectionRange"]["end"]["character"], 14);
    }

    #[test]
    fn a_write_that_types_nothing_is_still_a_place() {
        // The common case in a real controller, and the one the card cannot answer: nothing
        // declares what `Story.where(...)` returns, so `@stories` has no type — and the line it
        // is assigned on is still exactly what somebody pressing go-to-definition wants. A jump
        // built out of `cursor::assignments_in`, which drops that assignment, would answer
        // nothing here; this reads the writes themselves.
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
        // Two writes in two files, and the order is the URI's rather than the graph's so that
        // two runs agree. This is also the shape the ruling was about: several possible writes
        // in several files is a list, and a list is a thing `definition` can say.
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
        // The refusal, and it is the same one the card already makes: `shared/_header.html.erb`
        // names a `SharedController` no file declares, and the honest answer to *which
        // controller assigned this* is that the path does not say. Picking one of the several
        // that render the partial would be a jump the reader cannot see is wrong.
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
        // The controller exists and the convention found it; it simply never writes this one.
        // An empty answer rather than the class, which is a place the reader did not ask for.
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
        // A mailer's views hang off the mailer's own name and not off a controller's:
        // `ActionMailer::Base` derives its view path from the class it is rendering for, so
        // `app/views/user_mailer/welcome.html.erb` is `UserMailer` and there is no
        // `UserMailerController` anywhere. 326 of the 4,407 template instance-variable reads
        // across the six corpora are in one — including every single one of mastodon's 101,
        // because the only `.erb` that application ships are mailer views — and until the rung
        // read the second convention both halves of it stopped at a class nothing declares.
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

        // The card names the class **and the convention**: `UserMailer` is not a controller,
        // and a footnote is read as a claim about the one class in it.
        let markdown = card(&mut harness, &view, source, "title");
        assert!(markdown.contains("Story#title"), "{markdown}");
        assert!(
            markdown.contains("`UserMailer`, line 3 — the mailer Rails renders this template from"),
            "{markdown}"
        );

        // And the jump, through the same function, so the two cannot name different classes.
        assert_eq!(
            jumps(&mut harness, &view, source, "@story"),
            ["user_mailer.rb:3:4"]
        );

        // **A controller of that name wins**, which is Rails' order and not a preference: an
        // application that really writes a `UserMailerController` has said where this template
        // renders from, and the rung must not overrule it. Nothing here decides anything the
        // framework has not decided already.
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
        // **The renderer rung is the one that reads a second document, so it is the one that
        // can hand the card an offset into a file the card does not have.** `@current =
        // @story` makes the receiver a nested `Receiver::Assigned`, whose whole purpose is to
        // carry the offset its assignment is written at — in the *controller*. `hover` and
        // `hints` both turn that offset into a line against the text the cursor is in, which
        // here is the template, so the footnote named a line of markup.
        //
        // Found on lobsters: `@messages` in `app/views/mod_mails/_mail.html.erb` is
        // `@messages = @mod_mail.…` in `mod_mails_controller.rb`, and `@mod_mail`'s offset of
        // 182 in that file is line 7 of the template — `<% if mod_mail.comment_references…`,
        // which assigns nothing to anything. Every other provenance field that names another
        // file carries a line and the file's name for exactly this reason; this one carries an
        // offset because every other rung stays inside one document.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  def show\n    @story = Story.new\n    @current = \
             @story\n  end\nend\n",
        );
        // Long enough that the controller's offsets are lines in it, which is what makes the
        // wrong footnote look plausible rather than absurd.
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
        // And **not** a line in this file. The controller's offset is not a place here.
        assert!(
            !markdown.contains("Type taken from the assignment on line"),
            "{markdown}"
        );
    }

    #[test]
    fn a_view_directory_that_names_no_mailer_renders_nothing() {
        // The gate, and the reason the mailer half is not simply the controller half without a
        // suffix. `rails::controller_of` produces a name nothing but a controller is ever
        // called; `rails::mailer_of` produces whatever the directory happens to spell, so
        // `app/views/report/` spells `Report` — and an application with a `Report` class that
        // writes an `@story` would otherwise get a card and a jump invented out of a directory
        // name. The gate is the application's own superclass table, which is the same list
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

        // `@story` still spells a class, so the guess answers — and the card says so, which is
        // the difference between this and the convention having answered.
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
        // One gate for both halves of the rung. A project that turned the convention off
        // because its `app/views/` is not Rails' must not get a jump the card is refusing.
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
}
