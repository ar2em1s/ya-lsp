//! The RBS this crate writes itself, and where each declaration in it was really written.
//!
//! Every generator ends at *RBS text*, because
//! [`indexing::index_source`](rubydex::indexing::index_source) and
//! [`Types::harvest`](crate::analysis::types::Types::harvest) both take a `&str`. This module is
//! that boundary as a type: every generator (the schema, the model macros, hand-written
//! annotations) ends at a [`Facts`], and only
//! [`analysis::synthesized`](crate::analysis::synthesized) reads what a [`Facts`] renders to.
//!
//! It sits at the crate root because it belongs to neither layer: `workspace::rails` produces one
//! without seeing the graph, and `analysis::annotations` produces one because reading a comment's
//! claim is analysis.
//!
//! # A table, not a string builder
//!
//! A string builder cannot do two things an ordinary Rails application needs:
//!
//! - **Ask what another generator declared.** `delegate :name, to: :user` needs
//!   `Story#user -> User` and then `User#name -> String`, both written in the same pass into other
//!   documents. [`Facts::returns`] answers that, because the facts exist before any is spelled.
//! - **Stop two generators from making a silent overload.** A column `status` plus an
//!   `enum :status` is ordinary. Two `def status:` lines in one document are legal RBS and an
//!   overload set that is wrong in a way nothing reports, so [`Source`]'s rank settles the
//!   collision here, before a byte is written.
//!
//! # A generated name may not introduce its own namespace
//!
//! **A joined RBS name introduces every segment above the last**: `class Reports::Registry::Metric`
//! introduces `Reports::Registry` itself. Where nothing declares `Reports`, that costs
//! `Reports::Registry` its own singleton members, on itself and every subclass. So
//! [`Facts::render`] takes every name the application writes `module` for, and when an owner's
//! **immediate parent** is one of them, opens that parent as its own body so the joined name
//! introduces nothing.
//!
//! **Writing the whole name out is not the fix.** An explicit `class Api` wrapper *declares* a
//! kind, where a joined name only implies one, and this crate cannot know the kind of `Api`,
//! `Accounts`, `ActiveStorage` or `ActionMailer` (the application does not declare them; a gem may
//! or may not). So everything else keeps its spelling, and a generator that cannot spell its owner
//! safely **declines**. [`Namespaces::spellable`] is that test, asked by
//! `analysis::structs::Reader` for a `Struct.new` and by `knowledge::rails` for a nested model's
//! columns.
//!
//! # No span is deliberate, and it is the safe half
//!
//! [`Declared::at`] is an `Option`. `None` means no line of the user's code declares this text,
//! such as a relation class ya-lsp invented so `has_many` chains. No span means no
//! [`Mapping`](crate::analysis::synthesized::Mapping), which means
//! [`Origin::Unknown`](crate::analysis::synthesized::Origin::Unknown): the declaration types a
//! chain and is never offered as a jump target. **No mapping means no place, never a guess.**
//!
//! # A member is not the only thing a file can declare
//!
//! Every span here belongs to a member except one. [`Facts::namespace`] takes an `at` too, because
//! a namespace Zeitwerk conjures from a directory *holds* no member: the body is the whole
//! declaration, and a directory is not a line. Its place is the `class Mod::FlaggedController` that
//! confirmed the directory, the same rule the members obey: **the source this generator read**. Any
//! other body (a wrapper [`Declarations::open`] introduced to spell a name, a route-helper host, a
//! relation class) has no span and cannot be a place, because no file wrote those lines.

use std::collections::{BTreeMap, BTreeSet};

/// Who a declaration hangs on.
///
/// The two halves of a class are separate owners because they are separate methods: `def name` and
/// `def self.name` never collide in RBS or Ruby, so a `scope :recent` and a `belongs_to :recent`
/// are two facts.
///
/// [`Owner::Module`] makes a concern's macros and the route-helper module possible: both declare on
/// a `module` and have no class to hang on.
///
/// [`Owner::ModuleSingleton`] is **not** [`Owner::Singleton`] with a module's name. The render key
/// is `(is_module, name)`, so `Singleton("Devise")` would open `class Devise` while the
/// `mattr_accessor`'s instance half opened `module Devise`: two declarations of one constant, which
/// RBS refuses. Both halves of a `mattr_accessor` must land in one body, separated only by the
/// `self.` on the `def`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Owner {
    /// `class X`, and an instance method on it.
    Instance(String),
    /// `class X`, and a `def self.` on it.
    Singleton(String),
    /// `module X`, and an instance method on it — reached through whatever includes it.
    Module(String),
    /// `module X`, and a `def self.` on it, such as `Devise.pam_authentication`.
    ///
    /// Reached on the module itself, never through an `include`, as in Ruby: `mattr_accessor`
    /// writes `def self.x` on the module, and an includer gets only the *instance* half.
    ModuleSingleton(String),
}

impl Owner {
    /// The type's name, whichever side of it this is.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Instance(name)
            | Self::Singleton(name)
            | Self::Module(name)
            | Self::ModuleSingleton(name) => name,
        }
    }

    /// Whether the RBS keyword that opens it is `module` rather than `class`.
    ///
    /// Part of the render key: `class Storyish` and `module Storyish` are two declarations of one
    /// constant, and RBS refuses to hold both.
    #[must_use]
    fn is_module(&self) -> bool {
        matches!(self, Self::Module(_) | Self::ModuleSingleton(_))
    }

    /// How the body this hangs on is named, where a name is what a document is filed under.
    ///
    /// The render key spelled out: [`Owner::Instance`] and [`Owner::Singleton`] of one class are
    /// **one** body (one `class X`), and [`Owner::Module`] of the same name is another, because
    /// `class X` and `module X` cannot coexist in RBS. [`Facts::split`] cuts on exactly this, so a
    /// part never holds half a body.
    #[must_use]
    pub fn body(&self) -> String {
        let keyword = if self.is_module() { "module" } else { "class" };
        format!("{keyword}:{}", self.name())
    }

    /// What a `def` written on this side starts with.
    fn prefix(&self) -> &'static str {
        match self {
            Self::Singleton(_) | Self::ModuleSingleton(_) => "self.",
            Self::Instance(_) | Self::Module(_) => "",
        }
    }
}

/// Which generator said a member exists, and so how much it is worth.
///
/// The whole precedence table is [`Source::rank`]. Each rank outranks the next because it is *less
/// derived*. The table is written out whole, because ranks added one at a time make a table nobody
/// can review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    /// A Sorbet `sig` or a YARD `@return`. A human wrote the type down on purpose.
    Annotated,
    /// A member of a `Struct.new` or a `Data.define`, named by a symbol literal in the call.
    ///
    /// Just below the annotation and above everything else, because the call is the whole evidence:
    /// a name in that list *is* a method, with no convention or inflection between. The one
    /// collision it can meet is the one it is ranked against: a `sig` above a `def` that overrides
    /// a struct's reader, where a human states what this reader can only call `untyped`.
    Struct,
    /// A method `define_method(:x)` makes, named by a symbol literal in the call: the call is the
    /// whole evidence, as a struct's is, and ranks beside it. Ruby keeps it over the method a column
    /// or a macro installs, which live in modules the class includes.
    Defined,
    /// `enum`. It names a column's values, and *refining* the column means winning over it.
    ///
    /// **Above** the column: `story.status` is the label (a `String`), while the column stores an
    /// `Integer`, so letting the column win would answer with the storage.
    Enum,
    /// A column in a `db/*schema.rb`. What the database *is*, not what code claims.
    Column,
    /// `belongs_to`, `has_one`, `has_many`, `scope`. `class_name:` is explicit, and the
    /// name-derived case is bounded by the classes the application itself defines.
    Association,
    /// A mailer's action or a job's `perform`: a framework convention with no macro. The `def` is
    /// really in the file and the class method is really installed, but the *type* comes from this
    /// table, not the file, so it ranks below everything that was told one.
    Convention,
    /// `attribute`. A declared type, but on a class with no schema behind it.
    Attribute,
    /// `alias_attribute`, `store_accessor` and the rest of the long tail. Derived from
    /// something in rank 2–5.
    Derived,
    /// `delegate`. Derived from another class entirely, and often `untyped`.
    Delegated,
    /// The members ya-lsp writes itself: ActiveRecord's query interface (`where`, `first`, `find`)
    /// and the fixed half of a `Struct` or a `Data`.
    ///
    /// **Below every other rank.** Every other rank is something a file says; no file says this, so
    /// anything a file says about the same name is better. A `scope :first` is the ordinary case.
    /// `Struct#each` is here for the same reason: no line of code declares it, so it has no span
    /// and can never be a place.
    Interface,
    /// ActiveRecord's query interface (`where`, `first`, `find`): [`Self::Interface`] with one more
    /// thing said about it.
    ///
    /// Same rank, same argument: no file in the project declares `Story.where`. The difference is
    /// that a file in the **bundle** does, and this tag tells [`Declarations::named`] which members
    /// to look for. Ranked beside `Interface`, not below it, because where the two could collide
    /// they are equally weak evidence.
    Query,
}

impl Source {
    /// Whether a member this generator declared displaces one `other` declared.
    ///
    /// The precedence table asked from **outside** one document. [`Facts::declare`] settles a
    /// collision inside one. A `delegate :title` and a `t.string "title"` are in two (the model's
    /// generated document and the schema's), where two `def title:` lines are a silent overload set
    /// and the type is whichever was harvested last. So the loser declines, as an `enum` does
    /// against a column.
    #[must_use]
    pub fn outranks(self, other: Self) -> bool {
        self.rank() < other.rank()
    }

    /// Lower is more specific, and more specific wins.
    #[must_use]
    fn rank(self) -> u8 {
        match self {
            Self::Annotated => 1,
            Self::Struct | Self::Defined => 2,
            Self::Enum => 3,
            Self::Column => 4,
            Self::Association => 5,
            Self::Convention => 6,
            Self::Attribute => 7,
            Self::Derived => 8,
            Self::Delegated => 9,
            Self::Interface | Self::Query => 10,
        }
    }
}

/// One member some generator says exists, before anything has been spelled as RBS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    pub owner: Owner,
    pub name: String,
    /// The RBS return type: `String`, `Comment::Relation`, `untyped`.
    pub returns: String,
    /// The RBS parameter list, parentheses and block included: `()`, `(*untyped)`,
    /// `() { (Comment) -> void }`.
    pub parameters: String,
    /// Every arm after the first, as `(parameters, returns)`: an RBS overload set.
    ///
    /// Four names in ActiveRecord's query interface answer differently depending on the call:
    /// `Story.first` is a `Story?` and `Story.first(3)` an `Array[Story]`; `Story.select(:id)` is a
    /// relation and `Story.select { }` an `Array`. [`Types`](crate::analysis::types::Types) tells
    /// those apart at the *call site* (positional argument count, block or not), and this is how
    /// the fact table writes them down.
    ///
    /// Empty for every other generator, which is normal: a column, an association and a `delegate`
    /// each say one thing. A member with any arm here has **no single return type**, so
    /// [`Facts::returns`] declines it.
    pub overloads: Vec<(String, String)>,
    /// The provenance comment written above the `def`, or empty for none.
    ///
    /// It reaches a hover card as the declaration's documentation, like RDoc above a `def`, so no
    /// module outside the generators has to learn words like "table" or "association".
    pub because: String,
    /// `(the whole declaration, the name inside it)` in the source that declared it.
    ///
    /// `None` is not a failure: it marks text this crate invented, which must type a chain without
    /// ever becoming a jump target.
    pub at: Option<At>,
    /// Which generator said so. The precedence table's key.
    pub from: Source,
    /// Written `private def`: a member the source keeps private.
    ///
    /// **Set only where the source is private.** rubydex takes a method's visibility from the last
    /// of its definitions (`Graph::visibility`), and which one that is depends on the order they
    /// were resolved in. A public signature over a private `def` would then be a public method in
    /// some orders. rubydex reads `private` in RBS (`rbs_indexer.rs`), so with it the two agree in
    /// every order.
    pub private: bool,
}

/// A return type as a method type may hold it: a union in brackets.
///
/// RBS reads `() -> A | B` as two overloads, the second of which is no method type, so the whole
/// document fails to parse and every declaration in it is lost. A generator writes the union it
/// means, and this writes it the way RBS can read it.
fn enclosed(returns: &str) -> std::borrow::Cow<'_, str> {
    let mut depth = 0_usize;
    let mut union = false;
    for character in returns.chars() {
        match character {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            '|' if depth == 0 => union = true,
            _ => {}
        }
    }
    if union {
        format!("({returns})").into()
    } else {
        returns.into()
    }
}

/// A writer's one positional, named: `def title=: (untyped value) -> untyped`.
///
/// RBS lets a parameter go unnamed, and rubydex then calls it `arg0`, which a hover card prints
/// (decided 2026-09-29). Only a list that is exactly one bare type: one a generator named, or wrote
/// with anything else in it, is left as written.
fn named_value<'p>(name: &str, parameters: &'p str) -> std::borrow::Cow<'p, str> {
    let setter = name
        .strip_suffix('=')
        .is_some_and(|stem| !stem.is_empty() && !stem.ends_with(['=', '<', '>', '!']));
    let Some(one) = parameters
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return parameters.into();
    };
    let named = [
        one.is_empty(),
        one.starts_with(['?', '*', '&']),
        one.contains([' ', ',', '(', '{']),
    ];
    if setter && !named.contains(&true) {
        format!("({one} value)").into()
    } else {
        parameters.into()
    }
}

impl Declared {
    /// The `def` line, without its indentation.
    fn signature(&self) -> String {
        let mut line = format!(
            "{}def {}{}: {} -> {}",
            if self.private { "private " } else { "" },
            self.owner.prefix(),
            self.name,
            named_value(&self.name, &self.parameters),
            enclosed(&self.returns)
        );
        // RBS writes an overload set as `|`-separated method types and accepts them on one line.
        // Kept on one line on purpose: a `Span` is a byte range into this text, and a declaration
        // spanning a newline is one more thing every consumer of `spans` would have to get right,
        // for no gain.
        for (parameters, returns) in &self.overloads {
            line.push_str(" | ");
            line.push_str(parameters);
            line.push_str(" -> ");
            line.push_str(&enclosed(returns));
        }
        line
    }

    /// Whether this says anything about the type at all.
    ///
    /// The second half of the precedence rule: at equal rank, a typed declaration beats an
    /// `untyped` one, so a `delegate` phase two resolved is never displaced by one it could not.
    ///
    /// An overloaded member is typed when any arm is: `def pick: (*untyped) -> untyped` says
    /// nothing, and `def first: () -> Story? | (Integer) -> Array[Story]` says two things.
    fn typed(&self) -> bool {
        self.returns != "untyped"
            || self
                .overloads
                .iter()
                .any(|(_, returns)| returns != "untyped")
    }
}

/// Everything the generators have said, before any of it is text.
///
/// Insertion order is render order, and a collision is resolved *in place*: the winner takes the
/// position the first speaker claimed. So the output depends only on what was said and in what
/// order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Facts {
    members: Vec<Declared>,
    /// Where each `(owner, name)` lives in `members`, so a collision costs a lookup, not a scan,
    /// and [`Facts::returns`] is not linear in project size.
    at: BTreeMap<(Owner, String), usize>,
    /// A comment on a type rather than on a member, written when its body opens.
    ///
    /// One producer: the relation class. The whole *class* is ya-lsp's invention, so its name
    /// already tells a reader nobody wrote it. A note per member would say that ten times.
    notes: Vec<(Owner, String)>,
    /// The modules a body `include`s, in the order they were said.
    ///
    /// Being able to *spell* a `module` is not the same as reaching it. A concern is a module the
    /// user already `include`s; a module ya-lsp invents (the route helpers) has no such `include`,
    /// so this writes one. Not a [`Declared`]: an `include` names no member, has no return type and
    /// cannot collide, so a list is enough.
    mixins: Vec<(Owner, String)>,
    /// The modules a body `extend`s, in the order they were said.
    ///
    /// Only for a body this document declares, or one declared nowhere else: rubydex links an
    /// `extend` only where it is indexed with the namespace (`navigation.md`), so one written onto
    /// somebody else's class is never read.
    extensions: Vec<(Owner, String)>,
    /// The modules a body `prepend`s, in the order they were said, for [`Facts::mixins`]' reason.
    ///
    /// Written onto somebody else's class, like an `include`: rubydex links a late `prepend` as it
    /// links a late `include` (`Paperclip::Attachment.prepend(…)`, `workspace::mixins`).
    prepends: Vec<(Owner, String)>,
    /// The superclass a generated `class` opens with, for the bodies that have one.
    ///
    /// This is what lets ActiveRecord's query interface be written once per project, not once per
    /// model. Not a [`Declared`], for [`Facts::mixins`]' reason. One producer: the relation class,
    /// which this pass invents whole, and the only shape this is safe on. **A generated superclass
    /// on a class the user's own file already gives one is silently ignored**, so this may only be
    /// written on a name nothing else declares.
    supers: Vec<(Owner, String)>,
    /// The type parameters a generated `class` opens with (`[R]`), for the bodies that have them.
    ///
    /// What lets a value carry a class across a call that Ruby's own classes cannot name: a
    /// signature returning `WhereChain[Story::Relation]` hands `Story::Relation` to a member
    /// declared `-> R`. Not a [`Declared`], for [`Facts::mixins`]' reason.
    generics: Vec<(Owner, String)>,
    /// The bodies that exist and hold nothing: a namespace, and nothing it contains.
    ///
    /// A map, not a list: one document states a namespace once per *name*, while the chain above a
    /// deeply nested class states three, and name order is the only order there is (no member
    /// position to carry one). One producer: the autoloaded namespace.
    ///
    /// The value is the line that implied it, in the file this [`Facts`] belongs to. It is the one
    /// span here that is **not** a member's. A conjured namespace has no `def` to hang a place on,
    /// so the place is the `Api` of the `class Api::V1::Foo` that confirmed the directory: the same
    /// rule as every span here, *the source this generator read*. `None` keeps the no-place
    /// behaviour for a body nothing confirmed.
    bodies: BTreeMap<Owner, Option<At>>,
    /// The blocks whose `self` a generator knows: `(the body it is filed with, where the call the
    /// block is passed to starts in the source, what it runs as)` ([`Facts::runs`]).
    ran: Vec<(Owner, u32, Runs)>,
    /// Whether every body goes into one document ([`Facts::whole`]).
    whole: bool,
}

impl Facts {
    /// Say that a member exists, or lose to whatever already said so.
    ///
    /// Ranks decide. At equal rank, typed beats `untyped`. At equal rank and typedness, the first
    /// speaker keeps the position. That last clause is not arbitrary: it makes two
    /// `has_many :comments` in one file, or a schema read twice, produce the same document both
    /// times.
    pub fn declare(&mut self, declared: Declared) {
        let key = (declared.owner.clone(), declared.name.clone());
        match self.at.get(&key) {
            Some(&index) => {
                let held = &self.members[index];
                let better = declared.from.rank() < held.from.rank()
                    || (declared.from.rank() == held.from.rank()
                        && declared.typed()
                        && !held.typed());
                if better {
                    self.members[index] = declared;
                }
            }
            None => {
                self.at.insert(key, self.members.len());
                self.members.push(declared);
            }
        }
    }

    /// Write a comment on the type itself, shown once when its body opens.
    pub fn note(&mut self, owner: Owner, text: String) {
        self.notes.push((owner, text));
    }

    /// Say that a generated `class` opens with a superclass.
    ///
    /// The owner is a body, not a member, as in [`Facts::mixin`]; only its name and kind are read.
    /// Said twice for one body, the first is kept: a class has one superclass, and two would be RBS
    /// that does not parse.
    pub fn inherits(&mut self, owner: Owner, superclass: String) {
        let key = (owner.is_module(), owner.name().to_owned());
        if self
            .supers
            .iter()
            .any(|(held, _)| (held.is_module(), held.name().to_owned()) == key)
        {
            return;
        }
        self.supers.push((owner, superclass));
    }

    /// Say that a generated `class` opens with type parameters, written with their brackets.
    ///
    /// Said twice for one body, the first is kept, as with [`Facts::inherits`].
    pub fn generic(&mut self, owner: Owner, parameters: String) {
        let key = (owner.is_module(), owner.name().to_owned());
        if self
            .generics
            .iter()
            .any(|(held, _)| (held.is_module(), held.name().to_owned()) == key)
        {
            return;
        }
        self.generics.push((owner, parameters));
    }

    /// Say that a type exists, and nothing about what is in it.
    ///
    /// The one fact with no member and no type, and both are the point. Zeitwerk defines
    /// `User::Policy` because a directory is named `policy/`; what is *in* it are the classes the
    /// files below it write, which rubydex already holds. So this declares the constant and stops.
    ///
    /// **`at` is the line that confirmed the directory**, which makes this the one body here that
    /// can be a place. A directory is not a line, but it conjures nothing until a file in it writes
    /// the directory's name plus exactly one segment. So the `Api` of that file's
    /// `class Api::V1::Foo` is the source this generator read. `None` keeps the no-place behaviour
    /// for a body nothing confirmed.
    pub fn namespace(&mut self, owner: Owner, at: Option<At>) {
        // `or_insert`, not `insert`: two calls for one name in one document are the same namespace
        // seen twice (`class Api::V1::Foo` states `Api`, and so does a `class Api::Bar` beside it).
        // The first span is the one the document reads earliest, which is where a reader sent here
        // expects to land.
        self.bodies.entry(owner).or_insert(at);
    }

    /// Say that a body `include`s a module.
    ///
    /// The owner is a body, not a member, so [`Owner::Singleton`] means nothing here and renders as
    /// the instance side: `include` inside `class X` is what RBS has, and `extend` would be a
    /// different keyword with a different meaning.
    pub fn mixin(&mut self, owner: Owner, module: String) {
        self.mixins.push((owner, module));
    }

    /// Say that a body `extend`s a module, which only a body this document declares may: see
    /// [`Facts::extensions`]. The owner is a body, as in [`Facts::mixin`].
    pub fn extension(&mut self, owner: Owner, module: String) {
        self.extensions.push((owner, module));
    }

    /// Say that a body `prepend`s a module. The owner is a body, as in [`Facts::mixin`].
    pub fn prepend(&mut self, owner: Owner, module: String) {
        self.prepends.push((owner, module));
    }

    /// Say what the block passed to the call starting at `call` in the source runs as ([`Runs`]).
    ///
    /// A class some call makes and then evaluates a block in (`Class.new(parent) { … }` is Ruby's
    /// own) has no name the text writes, so the generator that names it is the only one who knows
    /// which class a block's `self` is. Not RBS: a signature says what `self` is for **every** call
    /// of a method, and two calls of one method here make two classes. It travels beside the text
    /// with the mappings, in the source's own coordinates.
    ///
    /// [`Runs::Refused`] is a refusal, not silence: a block whose `self` is a class some *other*
    /// call decides must not read as the class around it.
    ///
    /// `owner` only says which body the fact is filed with, as [`Facts::mixin`]'s does.
    pub fn runs(&mut self, owner: Owner, call: u32, runs: Runs) {
        self.ran.push((owner, call, runs));
    }

    /// Say that everything here renders as **one** document, not one per body ([`Facts::split`]).
    ///
    /// For a source that implies many small bodies nobody else writes onto: a spec file's example
    /// groups. One document per body is the right unit where a keystroke changes one body of many
    /// (a column, a table), and the wrong one where the bodies are one file's own and a file of
    /// forty groups would be forty documents to index.
    pub fn whole(&mut self) {
        self.whole = true;
    }

    /// Take everything `other` said, subject to the same precedence.
    ///
    /// This is where precedence is really enforced: a file that feeds two generators (a model with
    /// a `has_many` and a `@return` tag) merges here, and a member both name is decided, not
    /// written twice.
    pub fn extend(&mut self, other: Self) {
        // Into nothing, `other` is already what the loop below would build: one member per key in
        // the order it was said, and every list deduped by its own writer. Taken whole, because
        // declaring a schema's thousands of members again cost a large app milliseconds a settle.
        if *self == Self::default() {
            *self = other;
            return;
        }
        for member in other.members {
            self.declare(member);
        }
        self.notes.extend(other.notes);
        self.mixins.extend(other.mixins);
        self.extensions.extend(other.extensions);
        self.prepends.extend(other.prepends);
        for (owner, at) in other.bodies {
            self.bodies.entry(owner).or_insert(at);
        }
        for (owner, superclass) in other.supers {
            self.inherits(owner, superclass);
        }
        for (owner, parameters) in other.generics {
            self.generic(owner, parameters);
        }
        self.ran.extend(other.ran);
        self.whole |= other.whole;
    }

    /// Take what `other` said about its **members**, and nothing it said about a type.
    ///
    /// The union that `delegate`'s second phase queries, and deliberately not [`Facts::extend`]:
    /// nothing renders this one, so a copied note or `include` would be text nobody writes. It
    /// borrows because each merged document is still rendered on its own afterwards; the union
    /// exists only to be asked [`Facts::returns`] twice per `delegate`.
    pub fn absorb(&mut self, other: &Self) {
        for member in &other.members {
            self.declare(member.clone());
        }
    }

    /// What a member returns, after precedence.
    ///
    /// The question a string builder could not answer, and the reason for a second phase: a
    /// `delegate` asks this of the whole project's facts, twice, and gets a type before anything is
    /// rendered or indexed.
    ///
    /// **An overloaded member has no answer here, honestly.** The query interface writes
    /// `def first: () -> Story? | (Integer) -> Array[Story]`, and what `delegate :first` returns
    /// depends on the delegator's call, which this phase cannot see. The name is still declared;
    /// only the type declines, and [`Types::harvest`](crate::analysis::types::Types::harvest) drops
    /// it.
    #[must_use]
    ///
    /// **A sentinel is no answer here either** ([`ELEMENT`], [`COLLECTION`], [`WRITTEN`],
    /// [`SHARED`], [`HELD`], [`DEFINED`], [`FORWARDED`], [`BLOCK`], [`OWN_DEF`], [`SENT`]). Each is read against the member it was
    /// written on: copied onto a delegator, it would name the delegator's element, writer, `def` or
    /// block.
    pub fn returns(&self, owner: &Owner, name: &str) -> Option<&str> {
        let held = self.held(owner, name)?;
        // Anywhere in it, not only as the head: `Rails.logger`'s `(… | WrittenByItsWriter)` names
        // `Rails`' writer, which a delegator copying it would read as its own.
        let sentinel = [
            ELEMENT, COLLECTION, WRITTEN, SHARED, HELD, DEFINED, FORWARDED, BLOCK, OWN_DEF, SENT,
        ]
        .iter()
        .any(|sentinel| held.returns.contains(sentinel));
        (held.overloads.is_empty() && !sentinel).then_some(held.returns.as_str())
    }

    /// Every `(owner, name)` that survived precedence.
    ///
    /// [`Facts::source`] the other way round: that one takes a name and says who said it; this one
    /// hands over the names. A generator needs it when the collision it must lose is in **another
    /// document** and it does not know which names to ask about: an untyped `attribute :note` and
    /// the `t.string "note"` it would shadow are in the model's document and the schema's.
    pub fn declared(&self) -> impl Iterator<Item = (&Owner, &str)> {
        self.at.keys().map(|(owner, name)| (owner, name.as_str()))
    }

    /// Which generator's word a member currently is: the other half of the same question.
    ///
    /// Asked by a generator that must defer to a *better* one in another document, which is
    /// [`Source::outranks`]' whole purpose.
    #[must_use]
    pub fn source(&self, owner: &Owner, name: &str) -> Option<Source> {
        Some(self.held(owner, name)?.from)
    }

    /// Whatever survived precedence for one `(owner, name)`.
    fn held(&self, owner: &Owner, name: &str) -> Option<&Declared> {
        self.members
            .get(*self.at.get(&(owner.clone(), name.to_owned()))?)
    }

    /// Whether anything was said at all. A generator that says nothing is not recorded.
    ///
    /// A `Facts` holding only `include`s is **not** empty: a route-helper host is a document of
    /// nothing else, and a `merge` that dropped it would leave the helpers in the graph with
    /// nothing reaching them. A `Facts` holding only an empty body is not empty either: that body
    /// *is* everything its generator had to say.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
            && self.mixins.is_empty()
            && self.extensions.is_empty()
            && self.prepends.is_empty()
            && self.supers.is_empty()
            && self.generics.is_empty()
            && self.bodies.is_empty()
            && self.ran.is_empty()
    }

    /// How many members survived precedence. What a generator reports it declared.
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Cut this into one [`Facts`] per **body**, which is one generated document each.
    ///
    /// **This splits the rendering, never the generation.** Every generator has spoken and
    /// [`Facts::declare`] has settled every collision before this runs, so nothing here re-decides
    /// a rank, and [`Facts::returns`], [`Facts::declared`] and [`Facts::source`] were all asked of
    /// the whole file's facts first. That is what makes splitting safe.
    ///
    /// **A collision cannot cross a part.** Collisions are on `(owner, name)` and the owner picks
    /// the part, so anything [`Facts::declare`] settled lands in one part. Every field is keyed by
    /// an [`Owner`], so the split is total: nothing is left for a caller to remember.
    ///
    /// Parts come back in key order, not statement order: each part is its own document, and a
    /// stable document order is worth more than the order things were said in.
    ///
    /// Why split at all: [`Synthesized::record`](crate::analysis::synthesized::Synthesized::record)
    /// pays per declaration it re-indexes, so a document is the unit of invalidation. One document
    /// per file means a column that changed type re-indexes every column in the schema, which
    /// dominated keystroke latency on a large schema. One document per body re-indexes one table.
    ///
    /// **A whole [`Facts`] is one part** ([`Facts::whole`]), filed under [`WHOLE`].
    #[must_use]
    pub fn split(self) -> Vec<(String, Self)> {
        if self.whole {
            return vec![(WHOLE.to_owned(), self)];
        }
        let mut parts: BTreeMap<String, Self> = BTreeMap::new();
        for member in self.members {
            parts
                .entry(member.owner.body())
                .or_default()
                .declare(member);
        }
        for (owner, text) in self.notes {
            parts
                .entry(owner.body())
                .or_default()
                .notes
                .push((owner, text));
        }
        for (owner, module) in self.mixins {
            parts
                .entry(owner.body())
                .or_default()
                .mixins
                .push((owner, module));
        }
        for (owner, module) in self.extensions {
            parts
                .entry(owner.body())
                .or_default()
                .extensions
                .push((owner, module));
        }
        for (owner, module) in self.prepends {
            parts
                .entry(owner.body())
                .or_default()
                .prepends
                .push((owner, module));
        }
        for (owner, superclass) in self.supers {
            parts
                .entry(owner.body())
                .or_default()
                .inherits(owner, superclass);
        }
        for (owner, parameters) in self.generics {
            parts
                .entry(owner.body())
                .or_default()
                .generic(owner, parameters);
        }
        for (owner, at) in self.bodies {
            parts
                .entry(owner.body())
                .or_default()
                .bodies
                .entry(owner)
                .or_insert(at);
        }
        for (owner, call, runs) in self.ran {
            parts
                .entry(owner.body())
                .or_default()
                .ran
                .push((owner, call, runs));
        }
        parts.into_iter().collect()
    }

    /// Spell all of it as RBS, and record where each declaration really came from.
    ///
    /// A body opens whenever the owning type changes and closes when it changes again, so the
    /// document reads in the order the facts were stated. Spans are computed here and only here:
    /// computing them per generator per file would shift one generator's spans by the length of
    /// another's, and open the wrong line confidently.
    ///
    /// `namespaces` says what may be spelled around an owner. It is the one thing the facts cannot
    /// say, and it is asked here, not in each generator, because introducing a namespace is a
    /// property of *names*: any generator could reach that shape. See [`Declarations::open`].
    #[must_use]
    pub fn render(&self, namespaces: &Namespaces) -> Declarations {
        let mut out = Declarations::default();
        let mut noted: BTreeSet<(bool, &str)> = BTreeSet::new();
        let mut open: Option<(bool, &str)> = None;
        let mut wrappers = 0;
        for member in &self.members {
            let key = (member.owner.is_module(), member.owner.name());
            if open != Some(key) {
                if open.is_some() {
                    out.close(wrappers);
                }
                wrappers = out.open(key, self.opening(key), namespaces).0;
                open = Some(key);
                // Only above the first body of a type: a note is about the type, and a type whose
                // members were stated in two runs is still one type.
                if noted.insert(key) {
                    self.head(&mut out, key);
                }
            }
            if !member.because.is_empty() {
                out.comment(&member.because);
            }
            // Read before the `def` is written and used only after, because `declare` records the
            // same offset for a `Span`, so the two lists cannot disagree about where a declaration
            // starts.
            let start = out.rbs.len() as u32;
            out.declare(&member.signature(), member.at);
            if member.at.is_none() && member.from == Source::Query {
                out.named.push(Named {
                    generated: (start, out.rbs.len() as u32),
                    singleton: matches!(
                        member.owner,
                        Owner::Singleton(_) | Owner::ModuleSingleton(_)
                    ),
                    name: member.name.clone(),
                });
            }
        }
        if open.is_some() {
            out.close(wrappers);
        }
        // A body that declares nothing has no member to open it. Three shapes: a route-helper host
        // (a controller gets one `include`, which is not a `def`), a relation class (a superclass
        // line and nothing else), and a conjured namespace (the empty body itself). Written after
        // the members, so the document still follows the order the facts were stated in for
        // everything that has one.
        for (owner, at) in self
            .mixins
            .iter()
            .chain(&self.extensions)
            .chain(&self.prepends)
            .chain(&self.supers)
            .chain(&self.generics)
            .map(|(owner, _)| (owner, None))
            .chain(self.bodies.iter().map(|(owner, at)| (owner, *at)))
        {
            let key = (owner.is_module(), owner.name());
            if !noted.insert(key) {
                continue;
            }
            let (wrappers, line) = out.open(key, self.opening(key), namespaces);
            // The one span in this function that is not a member's. It covers the body's own line,
            // not anything inside it: a conjured namespace holds nothing, so the line *is* the
            // declaration. A body reached through `mixins` or `supers` has no span and keeps the
            // no-place rule, which a route-helper host and a relation class need: no file wrote
            // either.
            if let Some((declared, selection)) = at {
                out.spans.push(Span {
                    generated: line,
                    declared,
                    selection,
                });
            }
            self.head(&mut out, key);
            out.close(wrappers);
        }
        out.ran = self
            .ran
            .iter()
            .map(|(_, call, runs)| (*call, runs.clone()))
            .collect();
        out
    }

    /// What goes at the top of a body: its note, then every `include`, `extend` and `prepend`
    /// written on it.
    fn head(&self, out: &mut Declarations, key: (bool, &str)) {
        if let Some((_, text)) = self
            .notes
            .iter()
            .find(|(owner, _)| (owner.is_module(), owner.name()) == key)
        {
            out.comment(text);
        }
        for (_, module) in self
            .mixins
            .iter()
            .filter(|(owner, _)| (owner.is_module(), owner.name()) == key)
        {
            out.include(module);
        }
        for (_, module) in self
            .extensions
            .iter()
            .filter(|(owner, _)| (owner.is_module(), owner.name()) == key)
        {
            out.extension(module);
        }
        for (_, module) in self
            .prepends
            .iter()
            .filter(|(owner, _)| (owner.is_module(), owner.name()) == key)
        {
            out.prepend(module);
        }
    }

    /// What one body's line says after its name: the type parameters [`Facts::generic`] was told
    /// of, then the superclass [`Facts::inherits`] was.
    fn opening(&self, key: (bool, &str)) -> Opening<'_> {
        Opening {
            parameters: said_of(&self.generics, key),
            superclass: said_of(&self.supers, key),
        }
    }
}

/// What one of [`Facts`]' per-body lists says about the body `key` names.
fn said_of<'f>(said: &'f [(Owner, String)], key: (bool, &str)) -> Option<&'f str> {
    said.iter()
        .find(|(owner, _)| (owner.is_module(), owner.name()) == key)
        .map(|(_, text)| text.as_str())
}

/// What a body's opening line holds after its name ([`Facts::opening`]).
#[derive(Default)]
struct Opening<'f> {
    parameters: Option<&'f str>,
    superclass: Option<&'f str>,
}

/// RBS text, and where in the file that implied it each declaration was really written.
///
/// Only [`Facts::render`] builds one. It is what
/// [`Synthesized::record`](crate::analysis::synthesized::Synthesized::record) takes, and it is a
/// type only because the spans and the text must travel together.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Declarations {
    /// The RBS. Empty when the generator found nothing it was willing to say.
    pub rbs: String,
    /// One per *mapped* declaration in `rbs`, in writing order. Shorter than the number of `def`s
    /// whenever a generator wrote something no file declares.
    pub spans: Vec<Span>,
    /// How many bodies `rbs` opens, and how many `def`s it writes. Counted while writing, not by
    /// reading the text back, so the log line that reports them is free.
    ///
    /// `methods` is not `spans.len()`: a declaration no file declares is a method with no span, so
    /// the difference is how much of a generator's output this crate invented.
    pub classes: usize,
    pub methods: usize,
    /// The members whose place is somebody else's file, for a caller that can go and look.
    ///
    /// Disjoint from `spans` by construction, because the two say different things. A span is *the
    /// source this generator read*, which the generator knows; this is *the name Rails gave the
    /// same method*, which only the graph can turn into a file.
    pub named: Vec<Named>,
    /// `(where a call starts in the source, what its block runs as)`, as [`Facts::runs`] was told.
    /// Not text, so nothing about it is rendered.
    pub ran: Vec<(u32, Runs)>,
}

/// What a block runs as, where a generator knows and no signature can say ([`Facts::runs`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runs {
    /// The class object of a class some call makes at run time and the generator names: two calls
    /// of one method make two classes, which no signature can say.
    Made(String),
    /// The class object of **each** class that includes `of`, whichever runs the block: a block
    /// handed to every includer. Every includer is one this project writes.
    Each { of: String, classes: Vec<String> },
    /// A `self` nothing here can name: a class some other file decides, none of which is known.
    Refused,
}

/// A generated declaration whose real definition is a name, not a span.
///
/// That is the whole difference from [`Span`]: a column's place is a line in the `db/schema.rb`
/// this generator just read, while a query method's place is a `def` in a gem nobody read. One is
/// an offset in hand; the other is a question for the graph, so this carries a name and a side and
/// no file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    /// The declaration's range in [`Declarations::rbs`], end exclusive: a [`Span::generated`] for a
    /// declaration that has no `Span`.
    pub generated: (u32, u32),
    /// Whether it was written on a class object's side, which decides which ancestry answers.
    pub singleton: bool,
    /// The member's own name, as it was declared.
    pub name: String,
}

/// `(the whole construct, the name inside it)` in the file that really declared something.
///
/// What `locator::spans` produces, the two spans a `LocationLink` needs, and the shape every
/// generator here hands back: a column's `t.string "title"` and its `title`, an association's macro
/// call and its symbol, the `class Api::V1::Foo` a directory was believed on and its `Api`. Named
/// because it nests three deep in two tables, where four bare `u32`s say nothing.
pub type At = ((u32, u32), (u32, u32));

/// One generated declaration, and the bytes of the source that declared it.
///
/// Byte offsets on both sides and no URI: a generator knows the *text* it read, and which file that
/// came from is the caller's fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// The declaration's range in [`Declarations::rbs`], end exclusive.
    pub generated: (u32, u32),
    /// What an editor should show as the target: the whole `t.string "title", null: false`, or
    /// the whole `belongs_to :user, optional: true`.
    pub declared: (u32, u32),
    /// What it should select inside that: the column's or the association's name, unquoted.
    pub selection: (u32, u32),
}

/// Which names a generated one may be spelled around, and which of those may be opened.
///
/// **Two questions, never one.** A segment may be *joined* onto a generated name when something
/// declares it, because then the joined name introduces nothing. A *body* may be opened for it only
/// when its declaration writes the word `module`, because a wrapper states a kind.
///
/// The first source is the application's own code, where every name answers both questions;
/// `Analysis::walk` fills it from the same walk that collects everything else. The second is **the
/// bundle**, and deliberately a different set: it answers only for a namespace *above* a name the
/// application already writes, the one place a generated name could introduce an undeclared
/// segment. It is not widened to what a **macro** may name: `has_many :objects` in an application
/// with no `Object` would then be typed as Ruby's `Object`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Namespaces {
    /// Every name something declares, so a generated one may be joined onto it.
    declared: BTreeSet<String>,
    /// The subset written `module`, so a body may be opened for it.
    modules: BTreeSet<String>,
    /// The subset written `class`, so a directory's `module` may not join it.
    classes: BTreeSet<String>,
}

impl Namespaces {
    /// Record what a `class` or `module` line said.
    ///
    /// Two lines disagreeing about one name leave it **openable**: a file writing `module Foo` is
    /// evidence a body may be opened, and a `class Foo` elsewhere in the same application is a
    /// reopening its author meant. The **bundle** follows the opposite rule and settles its own
    /// disagreements before reaching here (see `Analysis::bundle_namespaces`), because there the
    /// two lines are in two projects whose authors never saw each other.
    pub fn declare(&mut self, name: String, module: bool) {
        if module {
            self.modules.insert(name.clone());
        } else {
            self.classes.insert(name.clone());
        }
        self.declared.insert(name);
    }

    /// Whether anything declares this constant, so a generated name may be joined onto it.
    #[must_use]
    pub fn declares(&self, name: &str) -> bool {
        self.declared.contains(name)
    }

    /// Whether what declares it writes `module`, so a body may be opened for it.
    #[must_use]
    pub fn opens(&self, name: &str) -> bool {
        self.modules.contains(name)
    }

    /// Whether nothing declares it, or only `module` lines do: a name a directory's generated
    /// `module` may declare too. A `class` beside it would make the two kinds a coin toss, since
    /// rubydex holds one declaration per constant.
    #[must_use]
    pub fn admits_a_module(&self, name: &str) -> bool {
        !self.classes.contains(name)
    }

    /// Whether a generated declaration on `name` can be written without introducing a namespace.
    ///
    /// Exactly two spellings are safe, and this accepts either:
    ///
    /// - **every segment above the name is declared**, so the joined name introduces nothing; or
    /// - the name's **immediate parent is a `module` the application writes**, which [`nesting`]
    ///   opens as its own body, so nothing is introduced either.
    ///
    /// Anything else is declined by the generator that wanted it. It cannot be *spelled* around
    /// instead: an explicit `class Api` wrapper **declares** a kind, unlike the namespace a joined
    /// name implies, and this crate cannot know the kind of `Api`, `ActiveStorage` or
    /// `ActionMailer`.
    ///
    /// It lives here, not in one generator, because it and [`nesting`] ask one question and must
    /// not answer it differently.
    ///
    /// The first clause is about the name itself: a name **rubydex invented** is not a constant
    /// path and cannot be written at all. See [`is_constant_path`].
    #[must_use]
    pub fn spellable(&self, name: &str) -> bool {
        if !is_constant_path(name) {
            return false;
        }
        let Some((parent, _)) = name.rsplit_once("::") else {
            return true;
        };
        self.opens(parent)
            || name
                .match_indices("::")
                .all(|(at, _)| self.declares(&name[..at]))
    }
}

/// Which names a constant written inside `owner` could mean, innermost first and bare last.
///
/// **Ruby's own lexical lookup, not a framework's**, which is why it lives beside the fact table
/// and not in `workspace/rails/`. `class Spree::LineItem` naming `Adjustment` asks for
/// `Spree::LineItem::Adjustment`, then `Spree::Adjustment`, then `Adjustment`. Rails modelled
/// `compute_type`'s search on this, so an association's `class_name` and an `include`'s spelling
/// get one answer from one function.
///
/// `ActiveRecord::Inheritance#compute_type` is
/// ``name.scan(/::|$/) { candidates.unshift "#{$`}::#{type_name}" }`` followed by
/// `candidates << type_name`, so the walk is over the *joined* name of the body the reference is
/// in, not the lexical nesting that produced it: `class Spree::LineItem` at top level asks the
/// same three questions as `module Spree; class LineItem`.
///
/// The empty prefix is **not** included: a top-level `Story` asks for `Story::Adjustment` and then
/// the bare name, two candidates. Nothing checks that a candidate could be a constant, and nothing
/// needs to: prefixing a non-constant leaves a non-constant, and the caller's `known` declines it.
pub fn candidates(owner: &str, name: &str) -> Vec<String> {
    let mut candidates = vec![format!("{owner}::{name}")];
    candidates.extend(
        owner
            .rmatch_indices("::")
            .map(|(at, _)| format!("{}::{name}", &owner[..at])),
    );
    candidates.push(name.to_owned());
    candidates
}

/// Whether every segment of `name` is a constant somebody could have written.
///
/// **rubydex names things a person cannot**, and a generated declaration on such a name is RBS that
/// does not parse. That costs the whole document, not one declaration, because
/// [`Synthesized::record`](crate::analysis::synthesized::Synthesized::record) refuses a document it
/// cannot parse. An anonymous `Class.new` is `15613248007104500482:144<anonymous>`, and a
/// `class << self` is `Foo::<Foo>`.
///
/// **Only real projects have ever caught this, never a test.** A spec's `Class.new(Spree::Base)` is
/// an ActiveRecord subclass by every rule here, and one as a route-helper host or a model cost
/// whole documents while every test stayed green. This is the one place the rule is written;
/// [`rails::hosts_routes`](crate::workspace::rails::hosts_routes) asks it here instead of keeping a
/// copy.
#[must_use]
pub fn is_constant_path(name: &str) -> bool {
    name.split("::").all(|segment| {
        segment.starts_with(|first: char| first.is_ascii_uppercase())
            && segment.chars().all(|c| c.is_alphanumeric() || c == '_')
    })
}

/// How a generated signature says "the element this receiver is a collection of".
///
/// **Part of the text contract between a generator and the types table**, so it lives here, beside
/// the other names both sides share, and neither side imports the other for it.
///
/// - **A made-up class name, because RBS has no keyword for it.** `self` is the receiver and
///   `instance` is the declaring class; a query interface written once for every collection needs
///   neither.
/// - **It never reaches the graph.** Nothing declares this class, so a lookup that escaped
///   `types::class_of` answers `None` and stops the chain.
/// - **It never reaches a reader.** A margin draws a `def`'s return only for a class the graph
///   holds, and this is none.
pub const ELEMENT: &str = "ActiveRecordElement";

/// How a generated signature says "the collection of this receiver's element". See [`ELEMENT`].
pub const COLLECTION: &str = "ActiveRecordCollection";

/// How a generated signature says "whatever this accessor's writer is given, or `nil`".
///
/// For a reader whose storage only its writer beside it fills, so every value it can hand back is
/// one some call of that writer passed: `thread_mattr_accessor :account` on a module nothing
/// includes. The types table answers it by reading every such call ([`ELEMENT`]'s contract: a name
/// no file declares, which a lookup that escaped `types::class_of` finds nothing for).
pub const WRITTEN: &str = "WrittenByItsWriter";

/// How a generated signature says "whatever the writer of this name is given on any instance of
/// the class that declares it, or a descendant".
///
/// For a reader whose storage every instance shares, which only the writer beside it fills:
/// `Rails::Railtie::Configuration` keeps an application's own `config.dispatcher = …` in one class
/// variable, so the application's, an engine's and a railtie's `config` all hold it. The types
/// table reads every call of the writer, and one whose receiver it cannot type refuses
/// ([`ELEMENT`]'s contract otherwise).
pub const SHARED: &str = "WrittenOnAnyInstance";

/// How a generated signature says "whatever the writer of this name is given on the receiver's
/// class object or its one instance, or `nil`".
///
/// For an `ActiveSupport::CurrentAttributes` attribute: the class object hands its reader and
/// writer to its per-thread `instance`, so both write one store, which each class keeps apart from
/// its subclasses'. The types table reads every call of the writer, and `set(x: v)`, on either.
pub const HELD: &str = "WrittenOnItsObjectOrClass";

/// How a generated signature says "what the Ruby `def` of this name in that module returns, read
/// as the receiver's own method", with the module as its one type argument:
/// `AnsweredByItsDef[::Account::Finder]`.
///
/// For a member Ruby installs by extending a module onto the receiver, which no file writes down:
/// a concern's `class_methods do` and `module ClassMethods`. The `def` is real and indexed, as the
/// module's; the types table reads its body with the receiver as `self`, and a lookup of this name
/// finds nothing ([`ELEMENT`]'s contract).
pub const DEFINED: &str = "AnsweredByItsDef";

/// How a generated signature says "what the second name answers on what the first answers, both
/// asked of the receiver", as two string literals: `ForwardedToItsTarget["order", "line_items"]`.
///
/// For a member that hands its call on: `delegate :line_items, to: :order` is Rails'
/// `_ = order; _.line_items(...)`. The first is a method name or a constant; the types table makes
/// both calls where the member is called ([`ELEMENT`]'s contract otherwise).
pub const FORWARDED: &str = "ForwardedToItsTarget";

/// How a generated signature says "what the block passed to the call that declared this member
/// hands back", with no arguments: `def user: () -> ReturnedByItsBlock`.
///
/// For a member a call defines from its block, as Ruby's `define_method(:user) { … }` does: RSpec's
/// `let(:user) { create(:user) }` is a method whose body is that block. The call is the member's
/// own place (its mapping), so the sentinel names nothing: the types table finds the block there
/// and reads it the way it reads a `def`'s body ([`ELEMENT`]'s contract otherwise).
pub const BLOCK: &str = "ReturnedByItsBlock";

/// How a generated signature says "what the Ruby `def` written at this member's own place returns",
/// with no arguments: `def helper: () -> ReturnedByItsOwnDef`.
///
/// For a `def` Ruby files on a class nothing names: one written in an RSpec group's block is a
/// method of that group's class, and rubydex, reading a block, files every such `def` of one name
/// as one `Object` method. The member's mapping names the `def`, whose body the types table reads
/// alone, as it reads any `def`'s ([`ELEMENT`]'s contract otherwise).
pub const OWN_DEF: &str = "ReturnedByItsOwnDef";

/// How a generated signature says "what the method the call's first argument names answers, called
/// publicly on the receiver with the arguments after it", with no arguments:
/// `def try: (*untyped) -> SentByItsSymbol`.
///
/// For a member that calls another by name, as ActiveSupport's `try(:title)` does: which method is
/// written at the call, not in the signature, so the types table makes that call where the member is
/// called ([`ELEMENT`]'s contract otherwise).
pub const SENT: &str = "SentByItsSymbol";

/// How a generated signature says "what the lambda passed to the call that declared this member
/// hands back, where that is truthy", beside what the member hands back otherwise:
/// `def self.recent: (*untyped) -> Story::Relation | ScopedByItsLambda`.
///
/// For a member a call defines from a lambda it is passed, whose value stands unless it is `nil` or
/// `false`: ActiveRecord's `scope :recent, -> { … }` is `instance_exec(&body) || self` on the
/// relation. The call is the member's own place, so the sentinel names nothing: the types table
/// finds the lambda there, reads it, and keeps the rest of the union for its falsy half, or for a
/// lambda it cannot read ([`ELEMENT`]'s contract otherwise).
pub const SCOPED: &str = "ScopedByItsLambda";

/// How a generated signature says "what the literal key the call passes first names, in a table a
/// body of knowledge keeps", with no arguments: `def t: (*untyped) -> NamedByItsKey`.
///
/// For a member whose value is looked up by a key written as a literal, as i18n's `t("a.b")` looks
/// up a translation: which key is written at the call, and what it holds is in the
/// module's table, not in RBS. The types table asks the registry at the call
/// ([`crate::knowledge::Knowledge::keyed_type`]; [`ELEMENT`]'s contract otherwise).
pub const KEYED: &str = "NamedByItsKey";

/// The name a [`Facts::whole`] document is filed under, in place of a body's.
pub const WHOLE: &str = "whole";

/// The class ya-lsp writes for a collection of `element`: [`element_of`] read forwards.
///
/// Nested under the element (`Comment::Relation`, not `CommentRelation`), for three reasons, most
/// important first:
///
/// 1. The name is *scoped*, so it cannot collide with an unrelated top-level constant.
/// 2. It reads right where a user meets it: a hover card saying `Comment::Relation#first`.
/// 3. A project that already has a `Comment::Relation` meant something by it, so a collision makes
///    the pass emit nothing instead of shadowing it.
#[must_use]
pub fn collection_of(element: &str) -> String {
    format!("{element}::{COLLECTION_CLASS}")
}

/// The relation `group` hands back for `element`: `Story::Grouped`, a subclass of
/// [`collection_of`]'s whose calculations are a `Hash` by group.
#[must_use]
pub fn grouped_of(element: &str) -> String {
    format!("{element}::{GROUPED_CLASS}")
}

/// Whether `name` is a relation [`grouped_of`] builds: a chain on it stays grouped.
#[must_use]
pub fn is_grouped(name: &str) -> bool {
    name.strip_suffix(GROUPED_CLASS)
        .is_some_and(|element| element.ends_with("::"))
}

/// The class a collection holds: [`collection_of`] read backwards.
///
/// Needed at *lookup* time, not generation time: `Story::Relation#first` is declared once for the
/// whole project, so only the receiver's name says which element the answer is about.
///
/// A name that is not a collection answers `None`, not itself: the caller's next question is "what
/// is that element's collection", and a wrong answer here would invent a class.
#[must_use]
pub fn element_of(collection: &str) -> Option<&str> {
    collection
        .strip_suffix(COLLECTION_CLASS)
        .or_else(|| collection.strip_suffix(GROUPED_CLASS))?
        .strip_suffix("::")
}

/// The last segment of the name [`collection_of`] builds, and the one [`element_of`] takes off.
const COLLECTION_CLASS: &str = "Relation";

/// The last segment of the name [`grouped_of`] builds, which [`element_of`] takes off too.
const GROUPED_CLASS: &str = "Grouped";

/// The one namespace of a name that becomes its own body, and what is left to spell joined inside
/// it: [`Declarations::open`]'s half of the rule.
///
/// **Only the immediate parent, and only when a file writes `module` for it.** A generated
/// `class Reports::Registry::Metric` introduces `Reports::Registry`, which costs that module its
/// own members where nothing declares `Reports`. Opening `module Reports::Registry` and writing
/// `class Metric` inside introduces nothing. Every other name stays as it was: a namespace declared
/// as a class needs no help, and an undeclared one cannot be helped, because a wrapper would have
/// to guess `class` or `module`.
fn nesting<'a>(name: &'a str, namespaces: &Namespaces) -> (Option<&'a str>, &'a str) {
    match name.rsplit_once("::") {
        Some((parent, last)) if namespaces.opens(parent) => (Some(parent), last),
        _ => (None, name),
    }
}

impl Declarations {
    /// Open a class or module body (inside a `module` for its immediate parent, where the
    /// application writes one), and say whether that wrapper was opened.
    ///
    /// A generated `class Reports::Registry::Metric` introduces `Reports::Registry` itself. Where
    /// an application wrote `module Reports::Registry` under a Zeitwerk-conjured `Reports` that no
    /// file declares, rubydex holds only one of the two declarations of that constant, and the
    /// module silently loses its **own** singleton members, on itself and every subclass. Opening
    /// it as a body keeps them.
    ///
    /// The wrapper is always a `module`, never a `class`, and that is not a guess: it opens only
    /// where an application file wrote that exact word for that exact name. A namespace declared as
    /// a class needs nothing, and an undeclared one gets no wrapper, because `class Api` or
    /// `class ActiveStorage` around a generated body would declare a kind this crate cannot know.
    /// The generator declines those instead; see [`Namespaces::spellable`].
    ///
    /// A wrapper declares no member, so it records no span and can never be a jump target: the
    /// no-mapping-no-place rule, doing its usual job.
    fn open(
        &mut self,
        (module, name): (bool, &str),
        opening: Opening<'_>,
        namespaces: &Namespaces,
    ) -> (usize, (u32, u32)) {
        let (wrapper, inner) = nesting(name, namespaces);
        if let Some(wrapper) = wrapper {
            self.rbs.push_str("module ");
            self.rbs.push_str(wrapper);
            self.rbs.push('\n');
            self.classes += 1;
        }
        // Read after the wrapper and before the declaration, so the range is the *inner* line
        // alone. No file wrote the wrapper, so a span over it would send a reader to a line that
        // does not exist.
        let start = self.rbs.len() as u32;
        self.rbs.push_str(if module { "module " } else { "class " });
        self.rbs.push_str(inner);
        if let Some(parameters) = opening.parameters {
            self.rbs.push_str(parameters);
        }
        // A `module` cannot have one, and [`Facts::inherits`] is only ever told about a class.
        if let Some(superclass) = opening.superclass.filter(|_| !module) {
            self.rbs.push_str(" < ");
            self.rbs.push_str(superclass);
        }
        self.rbs.push('\n');
        self.classes += 1;
        (
            usize::from(wrapper.is_some()),
            (start, self.rbs.len() as u32),
        )
    }

    /// Write an `include`, which names no member and so records no span.
    fn include(&mut self, module: &str) {
        self.rbs.push_str("  include ");
        self.rbs.push_str(module);
        self.rbs.push('\n');
    }

    /// Write an `extend`, for [`Self::include`]'s reason.
    fn extension(&mut self, module: &str) {
        self.rbs.push_str("  extend ");
        self.rbs.push_str(module);
        self.rbs.push('\n');
    }

    /// Write a `prepend`, for [`Self::include`]'s reason.
    fn prepend(&mut self, module: &str) {
        self.rbs.push_str("  prepend ");
        self.rbs.push_str(module);
        self.rbs.push('\n');
    }

    /// Write an indented line that is not a declaration: a provenance comment.
    fn comment(&mut self, text: &str) {
        self.rbs.push_str("  # ");
        self.rbs.push_str(text);
        self.rbs.push('\n');
    }

    /// Write one `def`, and record the source that declared it when a source did.
    fn declare(&mut self, text: &str, at: Option<At>) {
        self.methods += 1;
        let start = self.rbs.len() as u32;
        self.rbs.push_str("  ");
        self.rbs.push_str(text);
        self.rbs.push('\n');
        if let Some((declared, selection)) = at {
            self.spans.push(Span {
                generated: (start, self.rbs.len() as u32),
                declared,
                selection,
            });
        }
    }

    /// Close the body opened by [`Self::open`], and the `wrappers` it was opened inside.
    fn close(&mut self, wrappers: usize) {
        for _ in 0..=wrappers {
            self.rbs.push_str("end\n");
        }
    }
}

/// The names an application declares, for a test that must say which: [`Facts::render`]'s namespace
/// rule depends on what the fixture's files declare. Every name here is a `module`, which is what
/// the rule asks about; [`declaring_kinds`] is for tests that need the other answer too.
#[cfg(test)]
pub(crate) fn declaring(names: &[&str]) -> Namespaces {
    declaring_kinds(&[], names)
}

/// The same, for a fixture whose namespaces are not all modules.
#[cfg(test)]
pub(crate) fn declaring_kinds(classes: &[&str], modules: &[&str]) -> Namespaces {
    let mut namespaces = Namespaces::default();
    for name in classes {
        namespaces.declare((*name).to_owned(), false);
    }
    for name in modules {
        namespaces.declare((*name).to_owned(), true);
    }
    namespaces
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// A relation and its grouped twin name one element.
    #[test]
    fn a_grouped_relation_names_its_element() {
        assert_eq!(grouped_of("Story"), "Story::Grouped");
        assert!(is_grouped("Story::Grouped"));
        assert!(!is_grouped("Grouped"));
        assert!(!is_grouped("Story::Relation"));
        assert_eq!(element_of("Story::Grouped"), Some("Story"));
        assert_eq!(element_of(&collection_of("Story")), Some("Story"));
        assert_eq!(element_of("Story"), None);
    }

    /// A writer's lone parameter is named, and nothing else is touched.
    #[test]
    fn a_writer_s_one_bare_parameter_is_named_value() {
        assert_eq!(named_value("title=", "(untyped)"), "(untyped value)");
        assert_eq!(named_value("user=", "(::User?)"), "(::User? value)");
        // Not a writer: a reader, an operator ending in `=`, and a bare `=`.
        assert_eq!(named_value("title", "(untyped)"), "(untyped)");
        assert_eq!(named_value("==", "(untyped)"), "(untyped)");
        assert_eq!(named_value("<=", "(untyped)"), "(untyped)");
        assert_eq!(named_value("=", "(untyped)"), "(untyped)");
        // A list that is not exactly one bare type, or not a list at all.
        assert_eq!(named_value("x=", "()"), "()");
        assert_eq!(named_value("x=", "(*untyped)"), "(*untyped)");
        assert_eq!(named_value("x=", "(untyped given)"), "(untyped given)");
        assert_eq!(named_value("x=", "(A, B)"), "(A, B)");
        assert_eq!(named_value("x=", ""), "");
    }

    /// [`collection_of`] and [`element_of`] are one mapping, and it must be invertible.
    ///
    /// The query interface is declared once per project, so only the receiver's **name** says which
    /// element an answer is about. A non-collection name answers `None`, not itself, because the
    /// caller's next question would build a class out of it.
    #[test]
    fn a_collection_names_its_element_and_nothing_else_does() {
        assert_eq!(collection_of("Comment"), "Comment::Relation");
        assert_eq!(collection_of("Admin::Setting"), "Admin::Setting::Relation");
        for element in ["Story", "Spree::Order", "A::B::C"] {
            assert_eq!(element_of(&collection_of(element)), Some(element));
        }
        // Everything that is not one: a bare model, the last segment on its own, a name that
        // merely ends in the letters, and nothing at all.
        for other in ["Story", "Relation", "StoryRelation", ""] {
            assert_eq!(element_of(other), None, "{other}");
        }
    }

    /// Every rank, in order. The table itself, so a row added below has to be added here.
    const RANKS: [Source; 10] = [
        Source::Annotated,
        Source::Struct,
        Source::Enum,
        Source::Column,
        Source::Association,
        Source::Convention,
        Source::Attribute,
        Source::Derived,
        Source::Delegated,
        Source::Interface,
    ];

    fn story() -> Owner {
        Owner::Instance("Story".to_owned())
    }

    /// Which names can be written down at all, and the two ways rubydex invents one.
    #[test]
    fn a_name_rubydex_invented_is_not_a_name_a_generator_may_declare_on() {
        assert!(is_constant_path("Story"));
        assert!(is_constant_path("Spree::LineItem::Relation"));
        assert!(is_constant_path("V2Api"));
        // An anonymous `Class.new`, which specs write: the first segment starts with a digit.
        assert!(!is_constant_path("15613248007104500482:144<anonymous>"));
        // A `class << self`, which starts uppercase and is not alphanumeric.
        assert!(!is_constant_path("Story::<Story>"));
        // Each half matters on its own: a lower-case first character with nothing else wrong, and
        // an upper-case one with a character RBS cannot hold.
        assert!(!is_constant_path("story"));
        assert!(!is_constant_path("Story::Rela-tion"));

        // And `spellable` refuses it before asking anything about the namespace, so a name whose
        // every segment *is* declared is still declined.
        let namespaces = declaring(&["Spree"]);
        assert!(namespaces.spellable("Spree::Order"));
        assert!(!namespaces.spellable("Spree::<Spree>"));
    }

    fn said(owner: Owner, name: &str, returns: &str, from: Source) -> Declared {
        Declared {
            owner,
            name: name.to_owned(),
            returns: returns.to_owned(),
            parameters: "()".to_owned(),
            because: String::new(),
            at: None,
            from,
            overloads: Vec::new(),
            private: false,
        }
    }

    /// Extending an empty table takes the other one whole, which is what merging it member by
    /// member rebuilt: it already holds one member per key, and lists its own writers deduped. A
    /// table holding anything at all, even only a comment [`Facts::is_empty`] does not count, still
    /// merges.
    #[test]
    fn extending_nothing_takes_the_other_table_whole() {
        let mut other = Facts::default();
        other.declare(said(story(), "title", "untyped", Source::Interface));
        other.declare(said(story(), "title", "String", Source::Column));
        other.declare(said(story(), "body", "String", Source::Column));
        other.inherits(story(), "Base".to_owned());
        other.mixin(story(), "Tagged".to_owned());
        other.whole();
        let mut into = Facts::default();
        into.extend(other.clone());
        assert_eq!(into, other);

        let mut noted = Facts::default();
        noted.note(story(), "Written by ya-lsp.".to_owned());
        assert!(noted.is_empty());
        noted.extend(other.clone());
        assert_ne!(noted, other, "the comment is kept");
        let rbs = noted.render(&declaring(&[])).rbs;
        assert!(rbs.contains("Written by ya-lsp."), "{rbs}");
        assert!(rbs.contains("def title: () -> String"), "{rbs}");
        assert_eq!(noted.len(), 2);
    }

    /// A whole `Facts` is one document, and the two facts that are not RBS go with their body: an
    /// `extend` is written at its head, and which class a block runs as is carried beside the text.
    #[test]
    fn a_whole_facts_is_one_part_and_carries_its_blocks_and_extends() {
        let namespaces = declaring(&["Story"]);
        let mut facts = Facts::default();
        assert!(facts.is_empty());
        facts.runs(story(), 10, Runs::Made("Story".to_owned()));
        assert!(!facts.is_empty(), "a block's class is something said");
        let mut extended = Facts::default();
        extended.extension(story(), "::Tagged".to_owned());
        assert!(!extended.is_empty());
        facts.extend(extended);
        facts.declare(said(story(), "title", "String", Source::Column));
        facts.declare(said(
            Owner::Instance("Widget".to_owned()),
            "name",
            BLOCK,
            Source::Convention,
        ));
        facts.runs(Owner::Instance("Widget".to_owned()), 20, Runs::Refused);
        assert_eq!(
            facts.returns(&Owner::Instance("Widget".to_owned()), "name"),
            None
        );

        let apart = facts.clone().split();
        assert_eq!(
            apart.len(),
            2,
            "by body, where nobody asked for one document"
        );
        assert_eq!(
            apart[0].1.render(&namespaces).ran,
            [(10, Runs::Made("Story".to_owned()))]
        );
        assert_eq!(apart[1].1.render(&namespaces).ran, [(20, Runs::Refused)]);
        let mut whole = Facts::default();
        whole.whole();
        whole.extend(facts);
        let parts = whole.split();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].0, WHOLE);
        let rendered = parts[0].1.render(&namespaces);
        assert!(
            rendered.rbs.contains("class Story\n  extend ::Tagged\n"),
            "{}",
            rendered.rbs
        );
        assert_eq!(
            rendered.ran,
            [(10, Runs::Made("Story".to_owned())), (20, Runs::Refused)]
        );
        // An `extend` alone still opens its body.
        let mut only = Facts::default();
        only.extension(Owner::Module("Storyish".to_owned()), "::Tagged".to_owned());
        assert!(
            only.render(&namespaces)
                .rbs
                .contains("module Storyish\n  extend ::Tagged\n")
        );
    }

    /// One `Facts` per body, and every field goes with its owner.
    ///
    /// What makes the split safe: each part renders what the whole would have rendered for that
    /// body, so a document is a body and never half of one.
    #[test]
    fn splitting_puts_every_body_in_its_own_document_and_leaves_nothing_behind() {
        let namespaces = declaring(&["Story", "Widget", "Storyish"]);
        let mut facts = Facts::default();
        facts.declare(said(story(), "title", "String", Source::Column));
        facts.declare(said(
            Owner::Instance("Widget".to_owned()),
            "name",
            "String",
            Source::Column,
        ));
        // Interleaved on purpose: the split is by owner, not by arrival order, and a generator
        // returning to a body it already spoke about is the ordinary case.
        facts.declare(said(
            Owner::Singleton("Story".to_owned()),
            "recent",
            "untyped",
            Source::Association,
        ));
        facts.note(story(), "ya-lsp writes this.".to_owned());
        facts.mixin(story(), "Storyish".to_owned());
        facts.inherits(Owner::Instance("Widget".to_owned()), "Object".to_owned());
        facts.namespace(Owner::Module("Storyish".to_owned()), None);

        let parts = facts.clone().split();
        assert_eq!(
            parts
                .iter()
                .map(|(body, _)| body.as_str())
                .collect::<Vec<_>>(),
            vec!["class:Story", "class:Widget", "module:Storyish"],
            "one part per render key, in a stable order"
        );

        // `class Story` and `def self.recent` are one body, because the render key is
        // `(is_module, name)`, not the `Owner`.
        let first = &parts[0].1;
        assert_eq!(first.len(), 2);
        assert!(
            first
                .returns(&Owner::Instance("Story".to_owned()), "title")
                .is_some()
        );
        // And what the part renders is what the whole rendered for that body, note, `include`
        // and all.
        let whole = facts.render(&namespaces).rbs;
        for (_, part) in &parts {
            let rendered = part.render(&namespaces).rbs;
            for line in rendered.lines() {
                assert!(whole.contains(line), "{line} is not in {whole}");
            }
        }
        // Nothing was dropped on the way: every declaration the whole made is in some part.
        assert_eq!(
            parts.iter().map(|(_, part)| part.len()).sum::<usize>(),
            facts.len()
        );
    }

    /// A member that answers two things, and the three places one arm is not enough.
    ///
    /// Rendering, precedence and the query are asserted together because they are one decision read
    /// three ways: a `Declared` with arms is *not* a `Declared` whose return type happens to be
    /// longer.
    #[test]
    fn a_member_with_more_than_one_arm() {
        let overloaded = |returns: &str, second: &str, from| Declared {
            overloads: vec![("(Integer)".to_owned(), second.to_owned())],
            ..said(Owner::Instance("Story".to_owned()), "first", returns, from)
        };

        // One `def`, `|`-separated, on one line: a `Span` is a byte range, and a declaration
        // crossing a newline would be one more thing every consumer of `spans` must get right.
        let mut facts = Facts::default();
        facts.declare(overloaded("Story?", "Array[Story]", Source::Interface));
        assert_eq!(
            facts.render(&declaring(&[])).rbs,
            "class Story\n  def first: () -> Story? | (Integer) -> Array[Story]\nend\n"
        );

        // **No single return type, so `Facts::returns` declines.** What `delegate :first` returns
        // depends on the delegator's call, which phase two cannot see. The name is still declared;
        // only the type declines.
        assert_eq!(
            facts.returns(&Owner::Instance("Story".to_owned()), "first"),
            None
        );
        assert_eq!(
            facts.returns(&Owner::Instance("Story".to_owned()), "absent"),
            None,
            "and a member nobody declared is the same answer for a different reason"
        );

        // Typedness belongs to the *set* of arms: one arm that says something beats an `untyped` of
        // the same rank, and arms that all decline are still `untyped`.
        let mut wins = Facts::default();
        wins.declare(said(
            Owner::Instance("Story".to_owned()),
            "first",
            "untyped",
            Source::Interface,
        ));
        wins.declare(overloaded("untyped", "Array[Story]", Source::Interface));
        assert!(
            wins.render(&declaring(&[])).rbs.contains("Array[Story]"),
            "an arm that says something displaces an `untyped` at the same rank"
        );
        let mut loses = Facts::default();
        loses.declare(said(
            Owner::Instance("Story".to_owned()),
            "first",
            "Story",
            Source::Interface,
        ));
        loses.declare(overloaded("untyped", "untyped", Source::Interface));
        assert!(
            loses
                .render(&declaring(&[]))
                .rbs
                .contains("def first: () -> Story\n"),
            "and arms that all decline do not displace one that does not"
        );
    }

    /// One collision per rank, both ways round, and the more specific one survives.
    ///
    /// Both orders are the whole test: a table that held only when the winner spoke first would
    /// depend on generator order, which changes whenever a generator is added. The *pair* must have
    /// one answer.
    #[test]
    fn which_of_two_colliding_declarations_survives() {
        for pair in RANKS.windows(2) {
            let (better, worse) = (pair[0], pair[1]);
            for (first, second) in [(better, worse), (worse, better)] {
                let mut facts = Facts::default();
                facts.declare(said(story(), "status", &format!("{first:?}"), first));
                facts.declare(said(story(), "status", &format!("{second:?}"), second));
                assert_eq!(facts.len(), 1, "{first:?} against {second:?}");
                assert_eq!(
                    facts.returns(&story(), "status"),
                    Some(format!("{better:?}").as_str()),
                    "{first:?} spoke first, against {second:?}"
                );
            }
        }
    }

    /// Rank is not the whole rule: at equal rank, saying something beats saying nothing.
    ///
    /// The case is a `delegate` phase two resolved against one it could not, both the same rank:
    /// the one that found a type must not be displaced by the one that gave up.
    #[test]
    fn a_typed_declaration_beats_an_untyped_one_of_the_same_rank() {
        for (first, second) in [("untyped", "String"), ("String", "untyped")] {
            let mut facts = Facts::default();
            facts.declare(said(story(), "name", first, Source::Delegated));
            facts.declare(said(story(), "name", second, Source::Delegated));
            assert_eq!(facts.returns(&story(), "name"), Some("String"));
        }
    }

    /// At equal rank and typedness, the first to speak keeps both the answer and the place.
    ///
    /// Not arbitrary: it makes two `has_many :comments` in one file, or a schema read twice, render
    /// the same document both times.
    #[test]
    fn the_first_to_speak_keeps_the_position() {
        let mut facts = Facts::default();
        facts.declare(said(story(), "user", "User", Source::Association));
        facts.declare(said(story(), "title", "String", Source::Association));
        facts.declare(said(story(), "user", "Author", Source::Association));
        assert_eq!(facts.returns(&story(), "user"), Some("User"));
        assert_eq!(
            facts.render(&declaring(&[])).rbs,
            "class Story\n  def user: () -> User\n  def title: () -> String\nend\n"
        );
    }

    /// The two sides of a class are two members, and a member nobody declared is nothing.
    #[test]
    fn what_is_not_a_collision() {
        let mut facts = Facts::default();
        facts.declare(said(story(), "recent", "Story", Source::Association));
        facts.declare(said(
            Owner::Singleton("Story".to_owned()),
            "recent",
            "Story::Relation",
            Source::Association,
        ));
        facts.declare(said(
            Owner::Module("Storyish".to_owned()),
            "recent",
            "untyped",
            Source::Association,
        ));
        assert_eq!(facts.len(), 3);
        assert_eq!(facts.returns(&story(), "missing"), None);
        assert_eq!(
            facts.returns(&Owner::Instance("Gone".to_owned()), "recent"),
            None
        );
    }

    /// An `include` on a body that also declares, and one on a body that declares nothing.
    ///
    /// Two different loops render these, and the second is what a route-helper host needs: a
    /// controller gets one line that is not a `def`, so no member opens its body.
    #[test]
    fn a_body_may_include_a_module_and_may_be_nothing_else() {
        let mut facts = Facts::default();
        facts.declare(Declared {
            owner: Owner::Module("RouteHelpers".to_owned()),
            name: "story_path".to_owned(),
            returns: "String".to_owned(),
            parameters: "(*untyped)".to_owned(),
            because: String::new(),
            at: None,
            from: Source::Convention,
            overloads: Vec::new(),
            private: false,
        });
        facts.mixin(
            Owner::Module("RouteHelpers".to_owned()),
            "Enumerable".to_owned(),
        );
        facts.mixin(
            Owner::Instance("StoriesController".to_owned()),
            "RouteHelpers".to_owned(),
        );
        facts.mixin(
            Owner::Instance("StoriesController".to_owned()),
            "Enumerable".to_owned(),
        );
        // A singleton owner is the same *body*, so it does not open a second one.
        facts.mixin(
            Owner::Singleton("StoriesController".to_owned()),
            "Comparable".to_owned(),
        );

        assert_eq!(
            facts.render(&declaring(&[])).rbs,
            "module RouteHelpers\n  include Enumerable\n  def story_path: (*untyped) -> String\nend\n\
             class StoriesController\n  include RouteHelpers\n  include Enumerable\n  \
             include Comparable\nend\n"
        );
        // A `Facts` of only `include`s is not empty, or `merge` would drop the document carrying
        // them and leave the helpers in the graph with nothing reaching them.
        let mut only = Facts::default();
        only.mixin(Owner::Instance("A".to_owned()), "B".to_owned());
        assert!(!only.is_empty());
        assert_eq!(only.len(), 0, "an `include` is not a member");
        // …and `extend` carries them, which keeps the hosts and the helpers in one document.
        let mut into = Facts::default();
        into.extend(only);
        assert_eq!(
            into.render(&declaring(&[])).rbs,
            "class A\n  include B\nend\n"
        );
    }

    /// A generated `class` that opens with a superclass: what a relation class is.
    ///
    /// Three shapes. The middle one needs the trailing loop: a body with a superclass and **no
    /// member at all**, which nothing else opens. The note goes with it, because the class is
    /// ya-lsp's invention and its name already says so.
    #[test]
    fn a_generated_class_may_open_with_a_superclass_and_declare_nothing() {
        let mut facts = Facts::default();
        facts.note(
            Owner::Instance("Story::Relation".to_owned()),
            "A collection of `Story`.".to_owned(),
        );
        facts.inherits(
            Owner::Instance("Story::Relation".to_owned()),
            "ActiveRecordRelation".to_owned(),
        );
        // Said twice keeps the first, because a class has one superclass and two `<` clauses
        // are RBS that does not parse.
        facts.inherits(
            Owner::Singleton("Story::Relation".to_owned()),
            "SomethingElse".to_owned(),
        );
        // A body that declares as well as inheriting takes the superclass on the same line.
        facts.inherits(Owner::Instance("Widget".to_owned()), "Base".to_owned());
        facts.declare(Declared {
            owner: Owner::Instance("Widget".to_owned()),
            name: "label".to_owned(),
            returns: "String".to_owned(),
            parameters: "()".to_owned(),
            because: String::new(),
            at: None,
            from: Source::Column,
            overloads: Vec::new(),
            private: false,
        });
        // A `module` cannot have one, whatever it is told.
        facts.inherits(Owner::Module("Helpers".to_owned()), "Base".to_owned());

        assert_eq!(
            facts.render(&declaring(&[])).rbs,
            "class Widget < Base\n  def label: () -> String\nend\n\
             class Story::Relation < ActiveRecordRelation\n  # A collection of `Story`.\nend\n\
             module Helpers\nend\n"
        );
        // A `Facts` holding only a superclass is not empty, for the same reason one holding only an
        // `include` is not: the document must be kept.
        let mut only = Facts::default();
        only.inherits(Owner::Instance("A".to_owned()), "B".to_owned());
        assert!(!only.is_empty());
        assert_eq!(only.len(), 0, "a superclass is not a member");
        // …and `extend` carries it, subject to the same one-superclass rule.
        let mut into = Facts::default();
        into.inherits(Owner::Instance("A".to_owned()), "C".to_owned());
        into.extend(only);
        assert_eq!(into.render(&declaring(&[])).rbs, "class A < C\nend\n");
    }

    /// A union return is written in brackets, in every arm, since RBS reads a bare `|` after a
    /// return as the next overload. A `|` inside a type argument or a block is already enclosed.
    #[test]
    fn a_union_return_is_bracketed_so_rbs_reads_one_overload() {
        let mut facts = Facts::default();
        for (name, returns, overloads) in [
            ("either", "Integer | Float", vec![]),
            ("held", "Array[Integer | Float]", vec![]),
            (
                "arms",
                "String",
                vec![("(Integer)".to_owned(), "Symbol | nil".to_owned())],
            ),
        ] {
            facts.declare(Declared {
                owner: Owner::Instance("Widget".to_owned()),
                name: name.to_owned(),
                returns: returns.to_owned(),
                parameters: "()".to_owned(),
                because: String::new(),
                at: None,
                from: Source::Interface,
                overloads,
                private: false,
            });
        }
        assert_eq!(
            facts.render(&declaring(&[])).rbs,
            "class Widget\n  def either: () -> (Integer | Float)\n  \
             def held: () -> Array[Integer | Float]\n  \
             def arms: () -> String | (Integer) -> (Symbol | nil)\nend\n"
        );
    }

    /// A generated class may open with type parameters, before its superclass, and the first said
    /// is kept, as for a superclass. `extend` and `split` carry them like one.
    #[test]
    fn a_generated_class_may_open_with_type_parameters() {
        let chain = || Owner::Instance("Chain".to_owned());
        let mut facts = Facts::default();
        facts.generic(chain(), "[R]".to_owned());
        facts.generic(Owner::Singleton("Chain".to_owned()), "[S, T]".to_owned());
        facts.inherits(chain(), "Base".to_owned());
        facts.declare(Declared {
            owner: chain(),
            name: "not".to_owned(),
            returns: "R".to_owned(),
            parameters: "(*untyped)".to_owned(),
            because: String::new(),
            at: None,
            from: Source::Interface,
            overloads: Vec::new(),
            private: false,
        });
        assert_eq!(
            facts.render(&declaring(&[])).rbs,
            "class Chain[R] < Base\n  def not: (*untyped) -> R\nend\n"
        );

        // A body holding only its parameters is still written, and still a document.
        let mut only = Facts::default();
        only.generic(chain(), "[R]".to_owned());
        assert!(!only.is_empty());
        let mut into = Facts::default();
        into.extend(only);
        assert_eq!(into.render(&declaring(&[])).rbs, "class Chain[R]\nend\n");
        let parts = into.split();
        assert_eq!(parts.len(), 1);
        assert_eq!(
            parts[0].1.render(&declaring(&[])).rbs,
            "class Chain[R]\nend\n"
        );
    }

    /// The two-hop question, answered without any generator ordering: both ways round.
    ///
    /// `delegate :name, to: :user` on `Story` needs `Story#user -> User`, written by the
    /// association generator into `app/models/story.rb`'s document, and then
    /// `User#name -> String`, written by the schema generator into `db/schema.rb`'s. Neither has
    /// been rendered or indexed, and the union is the same either way round, which is what makes
    /// phase two a phase rather than a running order.
    #[test]
    fn two_hops_through_the_facts() {
        let mut schema = Facts::default();
        schema.declare(said(
            Owner::Instance("User".to_owned()),
            "name",
            "String",
            Source::Column,
        ));
        let mut model = Facts::default();
        model.declare(said(story(), "user", "User", Source::Association));

        for (first, second) in [
            (schema.clone(), model.clone()),
            (model.clone(), schema.clone()),
        ] {
            let mut known = first;
            known.extend(second);
            let hop = known
                .returns(&story(), "user")
                .expect("the association's own type");
            assert_eq!(hop, "User");
            assert_eq!(
                known.returns(&Owner::Instance(hop.to_owned()), "name"),
                Some("String")
            );
        }
    }

    /// A `module` is a different thing to open, and a concern's macros need it.
    #[test]
    fn what_a_module_renders_as() {
        let mut facts = Facts::default();
        facts.declare(said(
            Owner::Module("Storyish".to_owned()),
            "comments",
            "Comment::Relation",
            Source::Association,
        ));
        assert_eq!(
            facts.render(&declaring(&[])).rbs,
            "module Storyish\n  def comments: () -> Comment::Relation\nend\n"
        );
    }

    /// `class Storyish` and `module Storyish` are two declarations of one constant, and RBS will
    /// not hold both, so they are two bodies here.
    #[test]
    fn a_class_and_a_module_of_one_name_are_two_bodies() {
        let mut facts = Facts::default();
        facts.declare(said(
            Owner::Instance("Storyish".to_owned()),
            "one",
            "Integer",
            Source::Column,
        ));
        facts.declare(said(
            Owner::Module("Storyish".to_owned()),
            "two",
            "Integer",
            Source::Column,
        ));
        assert_eq!(facts.render(&declaring(&[])).classes, 2);
    }

    /// A body opens when the owner changes and closes when it changes again, and a note is written
    /// once, above the first body of its type only.
    #[test]
    fn a_body_opens_when_the_owner_changes() {
        let mut facts = Facts::default();
        facts.note(story(), "written by ya-lsp".to_owned());
        facts.declare(said(story(), "title", "String", Source::Column));
        facts.declare(said(
            Owner::Instance("Comment".to_owned()),
            "body",
            "String",
            Source::Column,
        ));
        facts.declare(said(story(), "id", "Integer", Source::Column));
        let out = facts.render(&declaring(&[]));
        assert_eq!(
            out.rbs,
            "class Story\n  # written by ya-lsp\n  def title: () -> String\nend\n\
             class Comment\n  def body: () -> String\nend\n\
             class Story\n  def id: () -> Integer\nend\n"
        );
        assert_eq!((out.classes, out.methods, out.spans.len()), (3, 3, 0));
    }

    /// Every span covers exactly the `def` line it was recorded for, comment excluded.
    ///
    /// A wrong answer here is silent: an off-by-one opens a real file at a confidently wrong line,
    /// which the whole mapping exists to prevent.
    #[test]
    fn a_span_covers_the_def_it_was_recorded_for() {
        let mut facts = Facts::default();
        facts.declare(Declared {
            because: "From `db/schema.rb`.".to_owned(),
            at: Some(((10, 30), (12, 17))),
            ..said(story(), "title", "String", Source::Column)
        });
        facts.declare(said(story(), "unmapped", "Integer", Source::Interface));
        let out = facts.render(&declaring(&[]));
        assert_eq!(out.spans.len(), 1);
        let span = out.spans[0];
        assert_eq!(
            &out.rbs[span.generated.0 as usize..span.generated.1 as usize],
            "  def title: () -> String\n"
        );
        assert_eq!((span.declared, span.selection), ((10, 30), (12, 17)));
    }

    /// Nothing said is nothing written, and a note on its own is still nothing said.
    #[test]
    fn a_generator_that_says_nothing() {
        let mut facts = Facts::default();
        assert!(facts.is_empty());
        facts.note(story(), "nobody wrote this".to_owned());
        assert!(facts.is_empty());
        assert_eq!(facts.render(&declaring(&[])), Declarations::default());
    }

    /// A body with nothing in it is still a body, and renders as its two lines.
    ///
    /// The empty `Facts` above and this one are two halves of one question. A note says something
    /// *about* a type and declares nothing, so it is nothing said. A namespace says the type
    /// **exists**, which is everything its generator has to say. So one renders an empty document
    /// and the other a declaration.
    ///
    /// Told of no line, it records no span: the no-place rule for a body nothing confirmed, like
    /// every other placeless body here.
    #[test]
    fn a_body_that_holds_nothing_is_still_declared() {
        let mut facts = Facts::default();
        assert!(facts.is_empty());
        facts.namespace(Owner::Module("User::Policy".to_owned()), None);
        assert!(!facts.is_empty());

        let out = facts.render(&declaring(&[]));
        assert_eq!(out.rbs, "module User::Policy\nend\n");
        assert!(out.spans.is_empty());
        assert_eq!(out.methods, 0);
        assert_eq!(out.classes, 1);
    }

    /// A namespace told where it was confirmed is a place, and the span is the body's own line.
    ///
    /// The one body here that can be a place. The span must cover the `module` line, not anything
    /// inside (there is nothing inside), because `Synthesized::origin` looks the mapping up by the
    /// offset of the definition it is asked about. A member's span is its `def`; here the
    /// declaration is the whole body.
    #[test]
    fn a_namespace_that_was_confirmed_somewhere_is_a_place() {
        let mut facts = Facts::default();
        facts.namespace(Owner::Module("Mod".to_owned()), Some(((6, 28), (6, 9))));

        let out = facts.render(&declaring(&[]));
        assert_eq!(out.rbs, "module Mod\nend\n");
        assert_eq!(out.spans.len(), 1);
        let span = out.spans[0];
        assert_eq!(
            &out.rbs[span.generated.0 as usize..span.generated.1 as usize],
            "module Mod\n"
        );
        assert_eq!((span.declared, span.selection), ((6, 28), (6, 9)));
    }

    /// The wrapper a generated name is spelled inside is never the place.
    ///
    /// `Api::V1` is written `module Api` / `module V1` when the application declares `Api` as a
    /// module, and only the inner line is this fact's declaration. A span over the wrapper would
    /// send a reader to `class Api::V1::Foo` and call it `Api`'s declaration, which is a different
    /// constant.
    #[test]
    fn the_wrapper_a_name_is_spelled_inside_is_never_the_place() {
        let mut facts = Facts::default();
        facts.namespace(
            Owner::Module("Api::V1".to_owned()),
            Some(((6, 30), (11, 13))),
        );

        let out = facts.render(&declaring(&["Api"]));
        assert_eq!(out.rbs, "module Api\nmodule V1\nend\nend\n");
        assert_eq!(out.spans.len(), 1);
        let span = out.spans[0];
        assert_eq!(
            &out.rbs[span.generated.0 as usize..span.generated.1 as usize],
            "module V1\n"
        );
    }

    /// Said twice is said once, and `extend` carries it like everything else.
    ///
    /// One file writes `class User::Policy::A` and `class User::Policy::B`, so the map keeps its
    /// document from opening the namespace twice. A `Facts` merged into another must bring it, or a
    /// file feeding a second generator would lose the declaration in the merge. **The first line
    /// said is kept**, through merges too: a reader is sent where the file says the name earliest,
    /// and generator order is not a fact about the file.
    #[test]
    fn a_body_that_holds_nothing_is_declared_once_however_often_it_is_said() {
        let mut facts = Facts::default();
        facts.namespace(
            Owner::Module("User::Policy".to_owned()),
            Some(((6, 24), (6, 10))),
        );
        facts.namespace(
            Owner::Module("User::Policy".to_owned()),
            Some(((60, 78), (60, 64))),
        );
        let mut into = Facts::default();
        into.extend(facts);
        into.namespace(
            Owner::Module("User::Policy".to_owned()),
            Some(((90, 99), (90, 94))),
        );

        let out = into.render(&declaring(&[]));
        assert_eq!(out.rbs, "module User::Policy\nend\n");
        assert_eq!(out.spans.len(), 1);
        assert_eq!(out.spans[0].selection, (6, 10));
    }

    /// The namespace rule as a table: the one namespace that becomes its own body.
    ///
    /// Every arm of [`nesting`]. A generated name is spelled joined, **except** where its immediate
    /// parent is a name the application writes `module` for; then that parent opens as a body, so
    /// the joined name introduces nothing. No third case: an explicit wrapper *declares* a kind a
    /// joined name only implies, and this crate cannot know the kind of `Api`, `ActiveStorage` or
    /// `ActionMailer`. [`Namespaces::spellable`] has the argument.
    #[test]
    fn which_namespace_becomes_a_body_of_its_own() {
        let modules = declaring(&["Admin", "Reports::Registry"]);
        for (name, wrapper, inner) in [
            // Nothing above it to ask about.
            ("Story", None, "Story"),
            // A class, and a namespace only a gem declares: both joined, for one reason: no file
            // here writes `module` for either.
            ("Comment::Relation", None, "Comment::Relation"),
            (
                "ConnectionPool::SharedConnectionPool",
                None,
                "ConnectionPool::SharedConnectionPool",
            ),
            // The parent is a module the application writes down.
            ("Admin::Setting", Some("Admin"), "Setting"),
            (
                "Reports::Registry::Metric",
                Some("Reports::Registry"),
                "Metric",
            ),
            // The *parent* and only the parent: a module further up is not enough, because the name
            // left inside the wrapper would still introduce what it joined.
            ("Admin::Setting::Metric", None, "Admin::Setting::Metric"),
        ] {
            assert_eq!(nesting(name, &modules), (wrapper, inner), "{name}");
        }
    }

    /// The wrappers are counted, closed, and their bodies declare nothing.
    #[test]
    fn a_wrapper_is_a_body_that_declares_nothing() {
        let mut facts = Facts::default();
        facts.declare(said(
            Owner::Instance("Reports::Registry::Metric".to_owned()),
            "name",
            "String",
            Source::Struct,
        ));
        let out = facts.render(&declaring(&["Reports::Registry"]));
        assert_eq!(
            out.rbs,
            "module Reports::Registry\nclass Metric\n  def name: () -> String\nend\nend\n"
        );
        // Three bodies and one `def`: a wrapper is a body opened, never a method declared and never
        // a span, so no wrapper can become a jump target.
        assert_eq!((out.classes, out.methods, out.spans.len()), (2, 1, 0));
    }
}
