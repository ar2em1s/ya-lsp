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
//! [`tests::rubydex_spells_a_signature_the_way_this_keys_it`] against a real graph: get it wrong
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
//! 3. **The controller a template's path names.** `app/views/stories/show.html.erb` is rendered
//!    by `StoriesController`, so `@story` is whatever that controller assigns it. A convention
//!    rather than a fact, shippable because it names the class and the line it read them from: a
//!    reader who thinks Rails renders this template from elsewhere can go and look.
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
    erb, locator, views,
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
/// is neither. It never reaches a reader — no hover card prints a return type — and it never
/// reaches the graph either, because nothing declares a class of this name: a lookup that
/// somehow escaped [`class_of`] would answer `None` and stop the chain rather than name a
/// class that is not there.
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

    fn insert(&mut self, method: &str, returns: Overloads) {
        self.returns.insert(DeclarationId::from(method), returns);
    }

    fn insert_yield(&mut self, method: &str, yields: Box<[Option<Return>]>) {
        self.yields.insert(DeclarationId::from(method), yields);
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
            self_id: self_of(graph, namespace.is_some(), method, nesting),
        })
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
fn self_of(
    graph: &Graph,
    in_namespace: bool,
    method: Option<&Definition>,
    nesting: NameId,
) -> Option<DeclarationId> {
    let Some(Definition::Method(method)) = method else {
        return in_namespace
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
pub struct Sources<'a> {
    pub graph: &'a Graph,
    /// What RBS says each method returns. Beside the graph rather than inside it — see the
    /// module docs.
    pub types: &'a Types,
    /// Another document's text, by the URI rubydex filed it under.
    ///
    /// A closure because the reading belongs to [`analysis`](super): an open buffer is
    /// authoritative over the file on disk, and only the server knows which buffers are open —
    /// a controller being edited types the template it renders before it is saved. `None` for a
    /// document with no readable text, which is an answer and not an error.
    pub read: &'a dyn Fn(&str) -> Option<String>,
    /// What a template's implicit receiver can answer.
    ///
    /// Beside the graph rather than in it for the type table's reason, and consulted by
    /// [`locator::resolve_typed`] and by `completion` rather than from here: it answers a
    /// *member* and not a receiver's type, which is the one thing on this struct that no rung
    /// in this module reads. It travels here because it travels with the other three, and
    /// because both of its readers already take a `Sources`.
    pub views: &'a views::Views,
    /// Whether the name-based guess may answer at all.
    ///
    /// Off is a supported configuration and the reason the tier is shippable: every other
    /// answer ya-lsp gives is defensible when it is wrong, and this one is not. A user who
    /// wants only checkable answers can have them.
    pub guess: bool,
}

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
    /// The controller a *template's* instance variable was typed from, when the view↔controller
    /// convention answered.
    pub controller: Option<FromController>,
    /// The receiver's own spelling, when nothing but it was left and the last rung answered.
    ///
    /// The name rather than the class, because the class is already the answer on the card: what
    /// a reader needs told is that `@user` being a `User` is something ya-lsp inferred from six
    /// letters and not something the code says.
    pub guess: Option<String>,
    /// How a **bare** name in a template was reached, when the view-context rung answered it.
    ///
    /// The one field here that is not about a receiver's type, and it is on this struct because
    /// it is the same kind of fact as the other four: nothing in the template says that
    /// `app/helpers` is in scope or which class renders it, so the answer is *derived* and the
    /// card has to say through which of the two conventions.
    pub view: Option<views::InView>,
}

/// Where a template's instance variable came from: the controller Rails' path convention names,
/// and the line its assignment is on **in that file**.
///
/// The line is carried rather than the offset, which every other provenance field does the
/// other way round. An offset is only a line once you have the text it indexes, and the caller
/// that draws the card has the *template's* text — the assignment is in a file it never reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromController {
    pub controller: String,
    pub line: u32,
}

/// The declaration whose members can be written after a `.` on `receiver`.
///
/// The one place a [`Receiver`] becomes a thing in the graph, shared by completion and by
/// navigation so that the two cannot disagree about what `person.` is. `scope` is where the
/// cursor is written — the `self` it can call private methods on, and the nesting a guessed
/// constant is resolved in — which is the only part of the answer that comes from the enclosing
/// code rather than from the receiver.
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
    let caller = scope.caller(graph);
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
        Receiver::Constant(offset) => {
            let constant = constant_at(graph, uri_id, *offset)?;
            if is_todo(graph, constant) {
                return None;
            }
            plain(singleton_of(graph, constant)?)
        }
        Receiver::SelfObject => plain(caller?),
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
    if let Some(typed) = from_controller(sources, uri_id, name) {
        return Some(typed);
    }
    if !sources.guess {
        return None;
    }
    guessed(sources.graph, name, scope)
}

/// A template's instance variable, typed by the controller its path names.
///
/// `app/views/stories/show.html.erb` is rendered by `StoriesController`, and the instance
/// variables the template reads are the ones that controller assigns. A template has no
/// enclosing class of its own, so there is nothing in the file to walk; this is
/// [`cursor::assignments_in`] pointed at a class the file never names.
///
/// **Bounded to the controller the path names, and not its ancestors.** `@user` in a real Rails
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
fn from_controller(sources: &Sources<'_>, uri_id: UriId, name: &str) -> Option<Typed> {
    let graph = sources.graph;
    // A local and a receiverless call are not instance variables, and only an instance
    // variable is what a controller hands a template.
    if !name.starts_with('@') {
        return None;
    }
    let path = DocUri::from_uri_str(graph.documents().get(&uri_id)?.uri())?.to_path()?;
    if !erb::is_template(&path) {
        return None;
    }
    let controller = rails::controller_of(&path)?;
    // The name and nothing like it. A template whose controller does not exist answers nothing
    // rather than reaching for a class that happens to be spelled similarly.
    let declaration = declared(graph, &controller)?;

    // A class reopened across files is answered from whichever declares the variable, in a
    // stable order — sorted rather than the graph's, so the answer does not depend on the order
    // the walk happened to index in.
    let mut documents: Vec<(String, UriId)> = locator::definitions_of(graph, declaration)
        .iter()
        .filter_map(|definition| {
            let id = *definition.uri_id();
            Some((graph.documents().get(&id)?.uri().to_owned(), id))
        })
        .collect();
    documents.sort();
    documents.dedup();

    for (uri, controller_uri_id) in documents {
        let Some(source) = (sources.read)(&uri) else {
            continue;
        };
        for (at, receiver) in cursor::assignments_in(&source, &controller, name)
            .iter()
            .rev()
        {
            // The scope the *assignment* is written in, in the controller's own file: `@x =
            // self` means the controller, and a constant on the right-hand side resolves
            // against the controller's nesting rather than the template's.
            let scope = Scope::at(graph, controller_uri_id, *at);
            let Some(mut typed) = method_receiver(sources, controller_uri_id, receiver, &scope)
            else {
                continue;
            };
            typed.derivation.controller = Some(FromController {
                controller: controller.clone(),
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
}
