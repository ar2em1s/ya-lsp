//! The RBS this crate writes itself, and where each declaration in it was really written.
//!
//! Every generator reduces to *producing RBS text*, because
//! [`indexing::index_source`](rubydex::indexing::index_source) and
//! [`Types::harvest`](crate::analysis::types::Types::harvest) both take a `&str`. This module is
//! that boundary as a type: every generator — the schema, the model macros, the annotations
//! somebody wrote by hand — ends at a [`Facts`], and
//! [`analysis::synthesized`](crate::analysis::synthesized) is the only thing that reads what a
//! [`Facts`] renders to.
//!
//! It sits at the crate root because it belongs to neither layer: `workspace::rails` produces one
//! without ever seeing the graph, and `analysis::annotations` produces one because reading what a
//! comment claims is analysis.
//!
//! # Why this is a table and not a string builder
//!
//! Two things a string builder cannot do, both reachable in an ordinary Rails application:
//!
//! - **No generator could ask what another declared.** `delegate :name, to: :user` needs
//!   `Story#user -> User` and then `User#name -> String`, both written in the same pass into
//!   other documents. [`Facts::returns`] is that question, answerable because the facts exist
//!   before any of them has been spelled.
//! - **Two generators declaring one member would silently be an overload.** A column named
//!   `status` and an `enum :status` make that ordinary. Two `def status:` lines in one document
//!   is legal RBS and produces an overload set that is wrong in a way nothing reports, so the
//!   collision is resolved here by [`Source`]'s rank, before a byte is written.
//!
//! # A generated name may not introduce the namespace it hangs off
//!
//! **A joined RBS name introduces every segment above the last**, so
//! `class Reports::Registry::Metric` introduces `Reports::Registry` itself — and where nothing
//! declares `Reports`, that costs `Reports::Registry` its own singleton members, on itself and on
//! every subclass. [`Facts::render`] therefore takes every name the application writes `module`
//! for, and does one narrow thing with it: when the owner's **immediate parent** is such a name,
//! that parent is opened as a body of its own so the joined name introduces nothing.
//!
//! **Writing the whole name out is not the fix.** An explicit `class Api` wrapper *declares* a
//! kind where a joined name only implies one, and `Api`, `Accounts`, `ActiveStorage` and
//! `ActionMailer` are namespaces this crate has no way to know the kind of — the application does
//! not declare them and a gem may or may not. So everything outside the case above keeps the
//! spelling it has, and a generator that cannot spell its owner safely **declines**:
//! [`Namespaces::spellable`] is that test, asked by `analysis::structs::Reader` for a
//! `Struct.new` and by `Analysis::model_tables` for a nested model's columns.
//!
//! # A declaration with no span is deliberate, and it is the safe half
//!
//! [`Declared::at`] is an `Option`, and `None` is the answer for text no line of the user's code
//! declares — a relation class ya-lsp invented so that `has_many` chains. No span means no
//! [`Mapping`](crate::analysis::synthesized::Mapping), which means
//! [`Origin::Unknown`](crate::analysis::synthesized::Origin::Unknown), which means the declaration
//! types a chain and is never offered as a place to jump to. **No mapping means no place, never a
//! guess**, and it costs one `Option` here.

use std::collections::{BTreeMap, BTreeSet};

/// Who a declaration hangs on.
///
/// The two halves of a class are separate owners because they are separate methods: `def name`
/// and `def self.name` collide with each other in neither RBS nor Ruby, so a `scope :recent`
/// and a `belongs_to :recent` are two facts and not one.
///
/// [`Owner::Module`] is what makes a concern's macros and the route-helper module possible at
/// all: both declare on a `module` and neither has a class to hang on.
///
/// [`Owner::ModuleSingleton`] is **not** the same thing as [`Owner::Singleton`] with a module's
/// name in it. The render key is `(is_module, name)`, so a
/// `Singleton("Devise")` would open `class Devise` while the `mattr_accessor`'s instance half
/// opened `module Devise` — two declarations of one constant, which RBS refuses to hold. Both
/// halves of a `mattr_accessor` have to land in one body, and the only thing that separates them
/// there is the `self.` on the `def`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Owner {
    /// `class X`, and an instance method on it.
    Instance(String),
    /// `class X`, and a `def self.` on it.
    Singleton(String),
    /// `module X`, and an instance method on it — reached through whatever includes it.
    Module(String),
    /// `module X`, and a `def self.` on it — `Devise.pam_authentication`.
    ///
    /// Reached on the module itself and never through an `include`, which is what Ruby does:
    /// `mattr_accessor` writes `def self.x` on the module, and an includer gets the *instance*
    /// half and not this one.
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
    /// Part of the render key: `class Storyish` and `module Storyish` are two different
    /// declarations of one constant and RBS refuses to hold both.
    #[must_use]
    fn is_module(&self) -> bool {
        matches!(self, Self::Module(_) | Self::ModuleSingleton(_))
    }

    /// What a `def` written on this side starts with.
    fn prefix(&self) -> &'static str {
        match self {
            Self::Singleton(_) | Self::ModuleSingleton(_) => "self.",
            Self::Instance(_) | Self::Module(_) => "",
        }
    }
}

/// Which generator said a member exists, and therefore how much it is worth.
///
/// The whole of the precedence table is [`Source::rank`], and the ordering is an argument
/// rather than a preference: each rank outranks the next because it is *less derived*. The
/// table is written out whole rather than grown a rank at a time, because a table whose ranks
/// arrive one at a time is a table nobody can review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    /// A Sorbet `sig` or a YARD `@return`. A human wrote the type down on purpose.
    Annotated,
    /// A member of a `Struct.new` or a `Data.define`, named by a symbol literal in the call.
    ///
    /// Directly below the annotation and above everything else, because the call is the whole
    /// of the evidence: a name in that list *is* a method, with no convention and no inflection
    /// between the two. The one collision it can reach is the one it is ranked against — a
    /// `sig` written above a `def` that overrides a struct's reader, which is a human saying
    /// what this reader can only say `untyped` about.
    Struct,
    /// `enum`. It names a column's values, and *refining* the column means winning over it.
    ///
    /// It sits **above** the column and not below it: `story.status` is the label — a `String`
    /// — and the column it is stored in is an `Integer`, so a reader that let the column win
    /// would answer with the storage.
    Enum,
    /// A column in a `db/*schema.rb`. What the database *is*, not what code claims.
    Column,
    /// `belongs_to`, `has_one`, `has_many`, `scope`. `class_name:` is explicit, and the
    /// name-derived case is bounded by the classes the application itself defines.
    Association,
    /// A mailer's action or a job's `perform`. A framework convention with no macro at all:
    /// the `def` is really in the file and the class method is really installed, but the
    /// *type* is this table's rather than the file's, which is why it sits below everything
    /// that was told one.
    Convention,
    /// `attribute`. A declared type, but on a class with no schema behind it.
    Attribute,
    /// `alias_attribute`, `store_accessor` and the rest of the long tail. Derived from
    /// something in rank 2–5.
    Derived,
    /// `delegate`. Derived from another class entirely, and often `untyped`.
    Delegated,
    /// The members ya-lsp writes itself: ActiveRecord's query interface — `where`, `first`,
    /// `find` — and the fixed half of a `Struct` or a `Data`.
    ///
    /// **Below every other rank.** Every one of those is a thing a file says; this is the one
    /// thing in the table no file says at all, so anything a file does say about the same name
    /// is better. A `scope :first` is the case, and it is reachable in ordinary code.
    /// `Struct#each` is here for the same sentence rather than for a second reason: no line of anybody's
    /// code declares it, so it carries no span and can never become a place.
    Interface,
}

impl Source {
    /// Whether a member this generator declared displaces one `other` declared.
    ///
    /// The precedence table asked from **outside** one document. [`Facts::declare`] settles a
    /// collision inside one; a `delegate :title` and a `t.string "title"` are in two — the
    /// model's generated document and the schema's — where two `def title:` lines are a silent
    /// overload set and the type is whichever was harvested last. So the rank is spent by the
    /// loser declining, exactly as an `enum` spends it against a column.
    #[must_use]
    pub fn outranks(self, other: Self) -> bool {
        self.rank() < other.rank()
    }

    /// Lower is more specific, and more specific wins.
    #[must_use]
    fn rank(self) -> u8 {
        match self {
            Self::Annotated => 1,
            Self::Struct => 2,
            Self::Enum => 3,
            Self::Column => 4,
            Self::Association => 5,
            Self::Convention => 6,
            Self::Attribute => 7,
            Self::Derived => 8,
            Self::Delegated => 9,
            Self::Interface => 10,
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
    /// Every arm after the first, as `(parameters, returns)` — an RBS overload set.
    ///
    /// It exists because four names in ActiveRecord's query interface answer two
    /// different things depending on how the call was written: `Story.first` is a `Story?` and
    /// `Story.first(3)` an `Array[Story]`, `Story.select(:id)` is a relation and
    /// `Story.select { }` an `Array`, and [`Types`](crate::analysis::types::Types) already reads
    /// read those apart from the *call site* — how many positional arguments, whether a block —
    /// and until this the fact table had no way to write one down.
    ///
    /// Empty for every other generator, and that is the normal case: a column, an association
    /// and a `delegate` each say one thing. A member with any arm here has **no single return
    /// type**, which is why [`Facts::returns`] declines it — see there.
    pub overloads: Vec<(String, String)>,
    /// The provenance comment written above the `def`, or empty for none.
    ///
    /// It reaches a hover card as the declaration's documentation, the way RDoc above a `def`
    /// does, so no module outside the generators has to learn the word "table" or
    /// "association".
    pub because: String,
    /// `(the whole declaration, the name inside it)` in the source that declared it.
    ///
    /// `None` is not a failure: it is text this crate invented, which must type a chain without
    /// ever becoming a jump target.
    pub at: Option<((u32, u32), (u32, u32))>,
    /// Which generator said so. The precedence table's key.
    pub from: Source,
}

impl Declared {
    /// The `def` line, without its indentation.
    fn signature(&self) -> String {
        let mut line = format!(
            "def {}{}: {} -> {}",
            self.owner.prefix(),
            self.name,
            self.parameters,
            self.returns
        );
        // RBS writes an overload set as `|`-separated method types, and accepts them on one
        // line. Kept on one line on purpose: a `Span` is a byte range into this text, and a
        // declaration that spans a newline is one more thing every consumer of `spans` would
        // have to be right about for no gain.
        for (parameters, returns) in &self.overloads {
            line.push_str(" | ");
            line.push_str(parameters);
            line.push_str(" -> ");
            line.push_str(returns);
        }
        line
    }

    /// Whether this says anything about the type at all.
    ///
    /// The second half of the precedence rule: at equal rank a typed declaration beats an
    /// `untyped` one, so a `delegate` phase two resolved is never displaced by one it could
    /// not.
    /// An overloaded member is typed when any arm of it is: `def pick: (*untyped) -> untyped`
    /// says nothing, and `def first: () -> Story? | (Integer) -> Array[Story]` says two things.
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
/// Insertion order is render order, and a collision is resolved *in place* — the winner takes
/// the position the first of them claimed. That makes the output a function of what was said
/// and the order it was said in, and nothing else.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Facts {
    members: Vec<Declared>,
    /// Where each `(owner, name)` lives in `members`, so a collision costs a lookup rather than
    /// a scan and [`Facts::returns`] is not linear in the size of the project.
    at: BTreeMap<(Owner, String), usize>,
    /// A comment on a type rather than on a member, written the moment its body opens.
    ///
    /// One producer: the relation class, whose whole *class* is what ya-lsp invented, so a
    /// reader who reaches any member of it has already been told by its name that nobody wrote
    /// it. A note per member would say it ten times.
    notes: Vec<(Owner, String)>,
    /// The modules a body `include`s, in the order they were said.
    ///
    /// Being able to *spell* a `module` is not the same as reaching it. A concern is a module
    /// the user already wrote an `include` for; a module ya-lsp invents — the route helpers —
    /// has no such `include` to borrow, so this writes one. It is deliberately not a
    /// [`Declared`]: an `include` names no member, has no return type and can never collide, so
    /// it is a list rather than a table.
    mixins: Vec<(Owner, String)>,
    /// The superclass a generated `class` is opened with, for the bodies that have one.
    ///
    /// What lets ActiveRecord's query interface be written once for the whole project rather
    /// than once per model. Deliberately not a [`Declared`] for [`Facts::mixins`]' reason: a
    /// superclass names no member, has no return type and cannot
    /// collide. One producer — the relation class, whose whole *class* this pass invents, which
    /// is also the only shape it is safe on. **A generated superclass on a class the user's own
    /// file already gives one is silently ignored**, so this may only ever be written on a name
    /// nothing else declares.
    supers: Vec<(Owner, String)>,
}

impl Facts {
    /// Say that a member exists, or lose to whatever already said so.
    ///
    /// Ranks decide; at equal rank a typed declaration beats an `untyped` one; and at equal
    /// rank and equal typedness the first to speak keeps the position. The last clause is not
    /// arbitrary — it is what makes two `has_many :comments` in one file, or a schema read
    /// twice, produce the same document both times.
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
    /// The owner is a body rather than a member, exactly as [`Facts::mixin`]'s is, and only its
    /// name and kind are read. Saying it twice for one body keeps the first, because a class has
    /// one superclass and the alternative is RBS that does not parse.
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

    /// Say that a body `include`s a module.
    ///
    /// The owner is a body and not a member, so [`Owner::Singleton`] is meaningless here and is
    /// rendered as the instance side: `include` inside `class X` is what RBS has, and an
    /// `extend` would be a different keyword with a different meaning. The query interface needs one and
    /// measured why it may not have it — `synthesized.md`.
    pub fn mixin(&mut self, owner: Owner, module: String) {
        self.mixins.push((owner, module));
    }

    /// Take everything `other` said, subject to the same precedence.
    ///
    /// This is where precedence is actually enforced: a file that feeds two generators — a
    /// model with a `has_many` and a `@return` tag — merges here, and a member both of them
    /// name is decided rather than written twice.
    pub fn extend(&mut self, other: Self) {
        for member in other.members {
            self.declare(member);
        }
        self.notes.extend(other.notes);
        self.mixins.extend(other.mixins);
        for (owner, superclass) in other.supers {
            self.inherits(owner, superclass);
        }
    }

    /// Take what `other` said about its **members**, and nothing it said about a type.
    ///
    /// The union `delegate`'s second phase queries, and deliberately not [`Facts::extend`]:
    /// nothing ever renders this one, so a note or an `include` copied into it would be text nobody
    /// writes. It borrows because the documents it merges are still needed afterwards — each of
    /// them is rendered on its own, and the union exists only to be asked
    /// [`Facts::returns`] twice per `delegate`.
    pub fn absorb(&mut self, other: &Self) {
        for member in &other.members {
            self.declare(member.clone());
        }
    }

    /// What a member returns, after precedence.
    ///
    /// The one thing a string builder could not answer, and the whole reason there is a second
    /// phase: a `delegate` asks this of the whole project's facts, twice, and gets a type
    /// without anything having been rendered or indexed.
    ///
    /// **An overloaded member has no answer here, and that is the honest one.** The query
    /// interface writes `def first: () -> Story? | (Integer) -> Array[Story]`, and what
    /// `delegate :first` returns is a question about the call the delegator writes, which this
    /// phase cannot see. The name is still declared and only the type declines, which
    /// [`Types::harvest`](crate::analysis::types::Types::harvest) drops.
    #[must_use]
    pub fn returns(&self, owner: &Owner, name: &str) -> Option<&str> {
        let held = self.held(owner, name)?;
        held.overloads.is_empty().then_some(held.returns.as_str())
    }

    /// Every `(owner, name)` that survived precedence.
    ///
    /// [`Facts::source`] asked the other way round: that one is handed a
    /// name and answers who said it, and this one hands over the names. A generator needs it
    /// where the collision it has to lose is in **another document** and it does not know which
    /// names to ask about — an untyped `attribute :note` and the `t.string "note"` it would
    /// shadow are in the model's document and the schema's.
    pub fn declared(&self) -> impl Iterator<Item = (&Owner, &str)> {
        self.at.keys().map(|(owner, name)| (owner, name.as_str()))
    }

    /// Which generator's word a member currently is — the other half of the same question.
    ///
    /// Asked by a generator that has to decline to a *better* one in another document, which is
    /// [`Source::outranks`]' whole reason to exist.
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
    /// A `Facts` holding nothing but `include`s is **not** empty: a route-helper host is a
    /// document of nothing else, and a `merge` that dropped them would put the helpers in the
    /// graph with nothing reaching them.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty() && self.mixins.is_empty() && self.supers.is_empty()
    }

    /// How many members survived precedence. What a generator reports it declared.
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Spell all of it as RBS, and record where each declaration really came from.
    ///
    /// A body opens whenever the owning type changes and closes when it changes again, so the
    /// document reads in the order the facts were stated. Spans are computed here and only
    /// here: computing them once per generator per file is what shifts one generator's spans by
    /// the length of another's, which opens the wrong line confidently.
    ///
    /// `namespaces` is what may be spelled around an owner. It is the one thing this table
    /// cannot know from the facts, and it is asked here rather than in any one generator
    /// because introducing a namespace is a property of *names* — every generator writes one and
    /// any of them could reach the shape. See [`Declarations::open`].
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
                wrappers = out.open(key.1, key.0, self.superclass(key), namespaces);
                open = Some(key);
                // Only above the first body of a type: a note is about the type, and a type
                // whose members were stated in two runs is still one type.
                if noted.insert(key) {
                    self.head(&mut out, key);
                }
            }
            if !member.because.is_empty() {
                out.comment(&member.because);
            }
            out.declare(&member.signature(), member.at);
        }
        if open.is_some() {
            out.close(wrappers);
        }
        // A body that declares nothing has no member to open it, and two shapes are all of
        // that: a route-helper host, where a controller gets one `include` and it is not a
        // `def`, and a relation class, which is a superclass line and nothing else. Written after the members so that the order of the document is still the order
        // the facts were stated in for everything that states one.
        for owner in self
            .mixins
            .iter()
            .chain(&self.supers)
            .map(|(owner, _)| owner)
        {
            let key = (owner.is_module(), owner.name());
            if !noted.insert(key) {
                continue;
            }
            let wrappers = out.open(key.1, key.0, self.superclass(key), namespaces);
            self.head(&mut out, key);
            out.close(wrappers);
        }
        out
    }

    /// What goes at the top of a body: its note, then every `include` written on it.
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
    }

    /// The superclass one body opens with, when [`Facts::inherits`] was told of one.
    fn superclass(&self, key: (bool, &str)) -> Option<&str> {
        self.supers
            .iter()
            .find(|(owner, _)| (owner.is_module(), owner.name()) == key)
            .map(|(_, superclass)| superclass.as_str())
    }
}

/// RBS text, and where in the file that implied it each declaration was really written.
///
/// The *output* of [`Facts::render`] and nothing else builds one: it is what
/// [`Synthesized::record`](crate::analysis::synthesized::Synthesized::record) takes, and the
/// only reason it is still a type of its own is that the spans and the text have to travel
/// together.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Declarations {
    /// The RBS. Empty when the generator found nothing it was willing to say.
    pub rbs: String,
    /// One per *mapped* declaration in `rbs`, in the order they were written. Shorter than the
    /// number of `def`s whenever a generator wrote something no file declares.
    pub spans: Vec<Span>,
    /// How many bodies `rbs` opens, and how many `def`s it writes. Counted while writing
    /// rather than by reading the text back, so the log line that reports them costs nothing.
    ///
    /// `methods` is not `spans.len()`: a generator that wrote a declaration no file declares
    /// wrote a method and no span, and the difference between the two numbers is exactly how
    /// much of a generator's output is text this crate invented.
    pub classes: usize,
    pub methods: usize,
}

/// One generated declaration, and the bytes of the source that declared it.
///
/// Byte offsets on both sides and no URI: a generator knows the *text* it read, and which file
/// that text came out of is the caller's fact rather than the generator's.
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
/// **Two questions and never one.** A segment may be *joined* onto a generated name when
/// something declares it, because then the joined name introduces nothing; a *body* may be
/// opened for it only when what declares it writes the word `module`, because a wrapper spells
/// a kind out loud.
///
/// The first source is the application's own code, and every name in it answers both questions
/// — [`Analysis::walk`](crate::analysis::Analysis) fills it from the same walk that collects
/// everything else. The second is **the bundle**, and it is deliberately not the same set: it
/// answers only for a namespace *above* a name the application already writes, which is the one
/// place a generated name can introduce a segment nobody declared. Widening what a **macro** may
/// name was measured over six corpora and is not done — `has_many :objects` in a class whose
/// application has no `Object` would otherwise be typed as Ruby's.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Namespaces {
    /// Every name something declares, so a generated one may be joined onto it.
    declared: BTreeSet<String>,
    /// The subset written `module`, so a body may be opened for it.
    modules: BTreeSet<String>,
}

impl Namespaces {
    /// Record what a `class` or `module` line said.
    ///
    /// Two lines disagreeing about one name leaves it **openable**, which is `Context::modules`.
    /// rule: a file writing `module Foo` is evidence a body may be opened, and a `class Foo`
    /// elsewhere in the same application is a reopening the author meant. The **bundle** is held to
    /// the opposite rule and resolves its own disagreements before it gets here — see
    /// `Analysis::bundle_namespaces` — because there the two lines are in two projects and neither
    /// author has seen the other.
    pub fn declare(&mut self, name: String, module: bool) {
        if module {
            self.modules.insert(name.clone());
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

    /// Whether a generated declaration on `name` can be written without introducing a namespace.
    ///
    /// There are exactly two safe spellings and this asks for either:
    ///
    /// - **every segment above the name is declared**, so the joined name introduces nothing;
    /// - or the name's **immediate parent is a `module` the application writes down**, which
    ///   [`nesting`] opens as a body of its own so nothing is introduced either.
    ///
    /// Anything else is declined by the generator that wanted it, and it cannot instead be
    /// *spelled* around: an explicit `class Api` wrapper is not the same thing as the namespace a
    /// joined name implies — it **declares** a kind, and `Api`, `ActiveStorage` and
    /// `ActionMailer` are all names this crate has no way to know the kind of. Writing them out
    /// costs more positions than it gains.
    ///
    /// It lives here rather than in one generator because it and [`nesting`] ask one question
    /// and must not be able to answer it differently.
    ///
    /// The first clause is about the name rather than its namespace: a name **rubydex
    /// invented** is not a constant path and cannot be written down at all. See
    /// [`is_constant_path`] for what that costs when it is missed.
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

/// Whether every segment of `name` is a constant somebody could have written.
///
/// **rubydex names things a person cannot**, and a generated declaration on one of those names
/// is RBS that does not parse — which costs the whole document rather than the one declaration,
/// because [`Synthesized::record`](crate::analysis::synthesized::Synthesized::record) refuses a
/// document it cannot parse rather than letting two consumers fail independently and silently.
/// An anonymous `Class.new` is `15613248007104500482:144<anonymous>`, and a `class << self` is
/// `Foo::<Foo>`.
///
/// **It has only ever been found by measurement, never by a test.** As a route-helper host it
/// cost forem's whole `include` document while every test stayed green; as a **model** it cost
/// solidus 38 documents, one per spec whose `Class.new(Spree::Base)` is an ActiveRecord subclass
/// by every rule this crate has. This is the one place the rule is written, and
/// [`rails::hosts_routes`](crate::workspace::rails::hosts_routes) asks it here rather than
/// keeping its own copy.
#[must_use]
pub fn is_constant_path(name: &str) -> bool {
    name.split("::").all(|segment| {
        segment.starts_with(|first: char| first.is_ascii_uppercase())
            && segment.chars().all(|c| c.is_alphanumeric() || c == '_')
    })
}

/// The one namespace of a name that becomes a body of its own, and what is left to spell joined
/// inside it — [`Declarations::open`]'s half of the rule.
///
/// **Only the immediate parent, and only when a file writes `module` for it.** A generated
/// `class Reports::Registry::Metric` introduces `Reports::Registry`, which costs that module its
/// own members where nothing declares `Reports`; opening `module Reports::Registry` and writing
/// `class Metric` inside it introduces nothing at all. Every other name is left exactly as it
/// was — a namespace declared as a class needs no help, and one nothing declares at all cannot
/// be helped, because a wrapper would have to guess between `class` and `module`.
fn nesting<'a>(name: &'a str, namespaces: &Namespaces) -> (Option<&'a str>, &'a str) {
    match name.rsplit_once("::") {
        Some((parent, last)) if namespaces.opens(parent) => (Some(parent), last),
        _ => (None, name),
    }
}

impl Declarations {
    /// Open a class or module body — inside a `module` for its immediate parent where the
    /// application writes one — and say whether that wrapper was opened.
    ///
    /// A generated `class Reports::Registry::Metric` introduces `Reports::Registry` itself, and where a
    /// Rails application wrote `module Reports::Registry` under a `Reports` Zeitwerk conjures
    /// and no file declares, rubydex 0.2.5 holds one declaration of that constant or the other.
    /// The module loses its **own** singleton members, on itself and on every subclass,
    /// silently. Opening it as a body keeps them.
    ///
    /// The wrapper is a `module` and never a `class`, and that is not a guess — it is opened
    /// only where a file in the application wrote that exact word for that exact name. A
    /// namespace it declares as a class needs nothing, and one it does not declare cannot be
    /// given a wrapper at all: writing `class Api` or `class ActiveStorage` around a generated
    /// body declares a kind this crate cannot know. What is left over is declined by the
    /// generator instead — see [`Namespaces::spellable`].
    ///
    /// A wrapper declares no member, so it records no span and can never become a place to jump
    /// to, which is the no-mapping-no-place rule doing its job rather than a new one.
    fn open(
        &mut self,
        name: &str,
        module: bool,
        superclass: Option<&str>,
        namespaces: &Namespaces,
    ) -> usize {
        let (wrapper, inner) = nesting(name, namespaces);
        if let Some(wrapper) = wrapper {
            self.rbs.push_str("module ");
            self.rbs.push_str(wrapper);
            self.rbs.push('\n');
            self.classes += 1;
        }
        self.rbs.push_str(if module { "module " } else { "class " });
        self.rbs.push_str(inner);
        // A `module` cannot have one, and [`Facts::inherits`] is only ever told about a class.
        if let Some(superclass) = superclass.filter(|_| !module) {
            self.rbs.push_str(" < ");
            self.rbs.push_str(superclass);
        }
        self.rbs.push('\n');
        self.classes += 1;
        usize::from(wrapper.is_some())
    }

    /// Write an `include`, which names no member and so records no span.
    fn include(&mut self, module: &str) {
        self.rbs.push_str("  include ");
        self.rbs.push_str(module);
        self.rbs.push('\n');
    }

    /// Write an indented line that is not a declaration — a provenance comment.
    fn comment(&mut self, text: &str) {
        self.rbs.push_str("  # ");
        self.rbs.push_str(text);
        self.rbs.push('\n');
    }

    /// Write one `def`, and record the source that declared it when a source did.
    fn declare(&mut self, text: &str, at: Option<((u32, u32), (u32, u32))>) {
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

/// The names an application declares, for a test that has to say which — [`Facts::render`]'s
/// finding-54 rule is about the namespace of an owner, so every test that renders one has to
/// state what its fixture's files declare. Every name is a `module`, which is what the rule
/// asks about; [`declaring_kinds`] is for the tests that need the other answer too.
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
        // An anonymous `Class.new`, which is what a spec writes and what cost solidus 38 whole
        // generated documents: the first segment starts with a digit.
        assert!(!is_constant_path("15613248007104500482:144<anonymous>"));
        // A `class << self`, which starts uppercase and is not alphanumeric.
        assert!(!is_constant_path("Story::<Story>"));
        // Both halves are load-bearing on their own: a lower-case first character with nothing
        // else wrong, and an upper-case one with a character RBS cannot hold.
        assert!(!is_constant_path("story"));
        assert!(!is_constant_path("Story::Rela-tion"));

        // And `spellable` refuses it before it asks anything about the namespace, so a name
        // whose every segment *is* declared is still declined.
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
        }
    }

    /// A member that answers two things, and the three places one arm is not enough.
    ///
    /// The rendering, the precedence and the query are asserted together because they are one
    /// decision read three ways — a `Declared` with arms is *not* a `Declared` whose return type
    /// happens to be longer.
    #[test]
    fn a_member_with_more_than_one_arm() {
        let overloaded = |returns: &str, second: &str, from| Declared {
            overloads: vec![("(Integer)".to_owned(), second.to_owned())],
            ..said(Owner::Instance("Story".to_owned()), "first", returns, from)
        };

        // One `def`, `|`-separated, on one line — a `Span` is a byte range and a declaration
        // that crossed a newline would be one more thing every consumer of `spans` has to be
        // right about.
        let mut facts = Facts::default();
        facts.declare(overloaded("Story?", "Array[Story]", Source::Interface));
        assert_eq!(
            facts.render(&declaring(&[])).rbs,
            "class Story\n  def first: () -> Story? | (Integer) -> Array[Story]\nend\n"
        );

        // **No single return type, so `Facts::returns` declines.** What `delegate :first` hands
        // back is a question about the call the delegator writes, which phase two cannot see.
        // The name is still declared and only the type declines.
        assert_eq!(
            facts.returns(&Owner::Instance("Story".to_owned()), "first"),
            None
        );
        assert_eq!(
            facts.returns(&Owner::Instance("Story".to_owned()), "absent"),
            None,
            "and a member nobody declared is the same answer for a different reason"
        );

        // Typedness is a property of the *set* of arms: an arm that says something is enough to
        // beat an `untyped` of the same rank, and arms that all decline are still `untyped`.
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
    /// Both orders is the whole test: a table that only held when the winner spoke first would
    /// be a statement about generator ordering, and `synthesize` reorders its generators
    /// whenever an item is added. What must be true is that the *pair* has one answer.
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
    /// The case is a `delegate` phase two resolved against one it could not — both rank 7, and
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

    /// At equal rank and equal typedness the first to speak keeps both the answer and the place.
    ///
    /// Not arbitrary: it is what makes two `has_many :comments` in one file, or a schema read
    /// twice, render the same document both times.
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

    /// The two-hop question, answered without any generator ordering: both ways round.
    ///
    /// `delegate :name, to: :user` on `Story` needs `Story#user -> User` — written by the
    /// association generator into `app/models/story.rb`'s document — and then
    /// `User#name -> String`, written by the schema generator into `db/schema.rb`'s. Neither
    /// has been rendered or indexed, and the union is the same either way round, which is what
    /// An `include` on a body that also declares, and one on a body that declares nothing.
    ///
    /// The two shapes are rendered by two different loops and the second is the one a
    /// route-helper host needs: a controller gets one line and it is not a `def`, so no member
    /// opens its body.
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
        // A `Facts` of nothing but `include`s is not empty, or `merge` would drop the document
        // that carries them and the helpers would be in the graph with nothing reaching them.
        let mut only = Facts::default();
        only.mixin(Owner::Instance("A".to_owned()), "B".to_owned());
        assert!(!only.is_empty());
        assert_eq!(only.len(), 0, "an `include` is not a member");
        // …and `extend` carries them, which is what makes the hosts and the helpers one document.
        let mut into = Facts::default();
        into.extend(only);
        assert_eq!(
            into.render(&declaring(&[])).rbs,
            "class A\n  include B\nend\n"
        );
    }

    /// A generated `class` that opens with a superclass — what a relation class is.
    ///
    /// Three shapes, and the middle one is the one that needs the trailing loop: a body with a superclass and
    /// **no member at all**, which nothing opens unless the trailing loop reaches it. The note
    /// goes with it, because the class is what ya-lsp invented and a reader who reaches any
    /// member of it has already been told that by the name.
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
        });
        // A `module` cannot have one, whatever it is told.
        facts.inherits(Owner::Module("Helpers".to_owned()), "Base".to_owned());

        assert_eq!(
            facts.render(&declaring(&[])).rbs,
            "class Widget < Base\n  def label: () -> String\nend\n\
             class Story::Relation < ActiveRecordRelation\n  # A collection of `Story`.\nend\n\
             module Helpers\nend\n"
        );
        // A `Facts` holding nothing but a superclass is not empty, for the same reason one
        // holding nothing but an `include` is not: the document has to be kept.
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

    /// makes phase two a phase rather than a running order.
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

    /// `class Storyish` and `module Storyish` are two declarations of one constant, and RBS
    /// will not hold both — so they are two bodies here and never one.
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

    /// A body opens when the owner changes, closes when it changes again, and a note is written
    /// once — above the first body of its type and never above a later one.
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
    /// The one thing a wrong answer here is silent about: an off-by-one opens a real file at a
    /// confidently wrong line, which the whole mapping exists to prevent.
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

    /// The namespace rule as a table: the one that becomes a body of its own.
    ///
    /// Every arm of [`nesting`]. A generated name is spelled joined, exactly as it always was —
    /// **except** where its immediate parent is a name the application writes `module` for, and
    /// then that parent is opened as a body so the joined name introduces nothing. There is no
    /// third case: an explicit wrapper *declares* a kind where a joined name only implies one, and
    /// `Api`, `ActiveStorage` and `ActionMailer` are all namespaces this crate has no way to know
    /// the kind of. [`Namespaces::spellable`] has the argument.
    #[test]
    fn which_namespace_becomes_a_body_of_its_own() {
        let modules = declaring(&["Admin", "Reports::Registry"]);
        for (name, wrapper, inner) in [
            // Nothing above it to ask about.
            ("Story", None, "Story"),
            // A class, and a namespace only a gem declares: both joined, and for one reason —
            // no file here writes `module` for either.
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
            // The *parent* and only the parent: a module further up is not enough, because the
            // name left inside the wrapper would still introduce whatever it joined.
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
        // Three bodies and one `def`: a wrapper is a body opened, never a method declared, and
        // never a span — so no wrapper can become a place to jump to.
        assert_eq!((out.classes, out.methods, out.spans.len()), (2, 1, 0));
    }
}
