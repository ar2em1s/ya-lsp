//! The model macros, the relation class they need, and the query interface both sides share.
//!
//! [`read_model`] reads every
//! macro in [`super::MACROS`] — and only where they are **statements of a class or module body**,
//! which is the bounding rule `synthesized.md` states as a safety property: the body of an
//! `included do`, of a `with_options`, of a `def` and of an `if` are all Ruby that only runs.
//!
//! The collections are monomorphic. `has_many :comments` returns a `Comment::Relation` this
//! crate writes, one per element type rather than one per association, and **nothing in it is
//! mapped** — no line of anybody's code declares `Comment::Relation#first`, and a relation four
//! models share could only be pointed at an arbitrary one of them.

use std::collections::{BTreeMap, BTreeSet};

use ruby_prism::{CallNode, Node, StatementsNode};

use super::ASSOCIATIONS;
use super::attributes::{self, Attribute};
use super::delegates::{self, Delegate};
use super::enums::{self, Enum};
use super::inflect::{camelize, singularize};
use super::syntax::{
    self, block_parameter, candidates, constant_spelling, first_symbol_or_string, header,
    inherited, reads_local, string_literal, symbol_or_string,
};
use super::tail::{self, Host, Tail};
use crate::analysis::types::{COLLECTION, ELEMENT};
use crate::generated::{Declared, Facts, Owner, Source};

/// What a model macro returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// `belongs_to :user` — one record, and Rails 5 made it non-`nil` unless `optional: true`.
    One,
    /// `has_one :profile` — one record or none, and nothing in the file says which.
    Maybe,
    /// `has_many :comments` — a relation, which is a class this pass generates.
    Many,
    /// `scope :recent, -> { ... }` — a *class* method returning a relation of its own class.
    Scope,
}

/// One macro call, read.
#[derive(Debug)]
struct Association {
    /// The macro as it was written. Five names reach four [`Kind`]s, so the provenance line
    /// cannot be recovered from the kind: `has_and_belongs_to_many` is a `Kind::Many` and is not
    /// a `has_many`.
    spelled: &'static str,
    /// The member's name: `user`, `comments`, `recent`.
    name: String,
    /// Every class this macro could name, innermost first and the bare name last.
    ///
    /// The candidate list is Rails' own rather than a refinement of this reader's.
    /// `ActiveRecord::Inheritance#compute_type` resolves an association's class against the
    /// **module nesting of the class the macro is written on**: `Spree::LineItem` naming
    /// `Adjustment` asks for `Spree::LineItem::Adjustment`, then `Spree::Adjustment`, and the
    /// bare `Adjustment` **last**. [`Association::resolved`] is the other end of it — the first
    /// candidate the application defines wins, and a macro naming none of them declares nothing
    /// exactly as one naming no class at all always has.
    ///
    /// **One entry and no walk** for the two spellings that name a class outright: a
    /// `class_name: "::Order"` is Rails' own absolute-reference branch, and a `scope` returns a
    /// relation of the class it is written on, which is a name and never a guess.
    candidates: Vec<String>,
    kind: Kind,
    optional: bool,
    /// The association this one reads through, when it is a `has_many :through`.
    through: Option<String>,
    at: (u32, u32),
    name_at: (u32, u32),
}

/// One class in a model file, and the macros in its body.
#[derive(Debug)]
struct ModelClass {
    /// Spelled with its lexical nesting, exactly as rubydex would spell it.
    name: String,
    /// Whether the body is a `module` rather than a `class` — a concern.
    ///
    /// A concern's
    /// macros belong to whichever class includes it, which this pass cannot know, so the
    /// instance-side ones are declared on the **module** and reach an includer through the
    /// `include` the user already wrote. The class-side ones cannot be: `scope :expired` in a
    /// concern returns a relation of the *includer*, which is a different type for each of
    /// them, so there is no one type to write down.
    module: bool,
    associations: Vec<Association>,
    enums: Vec<Enum>,
    /// The narrowest reader in the directory: a call that did not name a cast
    /// type is not here at all, because two gems spell a macro `attribute` and neither of them
    /// defines a method. A **module** may own one on the same terms a class does:
    /// `attribute :foo, :string` names one member whoever includes the concern, which is why it
    /// is not declined the way a `scope` is.
    attributes: Vec<Attribute>,
    /// The only ones this file reads and cannot declare in one pass: a
    /// `delegate`'s *name* is here and its *type* is a fact some other file's generator writes
    /// in this same run. [`Model::derived`] is the second phase that asks.
    delegates: Vec<Delegate>,
    /// The widest list here by macro count: twenty-nine names in one table, of
    /// which four are read and declare nothing. One of the families —`alias_attribute` — is
    /// phase two's like a `delegate`, which is why [`Tail::derived`] is asked rather than
    /// assumed.
    tail: Vec<Tail>,
    /// The only list in this struct that no generator reads: the names this body
    /// handed the **view context** with `helper_method`. Nothing about them is declared — the
    /// `def` they name is already in the graph, on this very class — so what is recorded is the
    /// permission and not the member. `analysis::views` is what asks.
    exports: Vec<String>,
    /// The other half of that: the modules this body put into its own view context with `helper`.
    ///
    /// Every controller already has every `app/helpers` module, so what this is *for* is the
    /// case where that default does not apply — `ActionMailer::Base` has no
    /// `include_all_helpers`, and 20 of the 26 `helper` calls the six corpora write under `app/`
    /// are in a mailer.
    /// The six in a controller name a module a gem ships, which is the other thing the default
    /// does not reach.
    helpers: Vec<String>,
    /// Every `def` this body writes itself, by `(is a def self., name)`.
    ///
    /// Read by [`super::LONG_TAIL`] and by nothing else. A macro in it installs its members
    /// either into a module the class includes or by `define_method` on the class at the moment
    /// the macro runs, and a `def` in the class body wins over both — always over the first, and
    /// over the second whenever it is written after the macro, which is where every occurrence
    /// in the corpus writes it. So declaring one the body also defines is declaring a method
    /// that does not exist, and the symptom is a hover card that says "Defined in 2 places"
    /// where one of the two is a line of Rails' the user's own `def` replaced.
    ///
    /// **It is not asked of the other readers**, and the difference is what each contributes.
    /// An `attribute`, an `enum` and a column say what a member *returns*, which a `def` with no
    /// annotation does not — so there the second declaration is carrying the type and the two
    /// places are both real. Everything the long tail declares is either `untyped`, a `bool` that
    /// follows from the shape, or a type derived from elsewhere, so the `def` is never worse.
    defined: BTreeSet<(bool, String)>,
}

/// One Ruby file, read for the macros above. Text in, no graph and no I/O.
#[derive(Debug)]
pub struct Model {
    classes: Vec<ModelClass>,
    /// Every class in this file that says it is **abstract**, fully spelled.
    ///
    /// A second list rather than a field of [`ModelClass`] because a class
    /// that says nothing else is not a `ModelClass` at all: `application_record.rb` holds
    /// `self.abstract_class = true` and no macro, and the whole point of the list is to name
    /// exactly that file's class.
    abstract_classes: BTreeSet<String>,
}

/// Read every association and scope `source` declares.
///
/// Only calls written **as statements of a class or module body** are read, and two blocks count
/// as part of that body: `included do`, which
/// `ActiveSupport::Concern` `class_eval`s on the including class, and `with_options`, which
/// merges its own keywords into every macro inside it. Everything else is still Ruby that only
/// runs — the body of a `def`, of an `if`, of a `class_methods do` and of any other block
/// declares nothing.
///
/// **The reader is not required to inherit from anything and never has been** — it reads every
/// class and module body it is given, because it is text in and text out and knows nothing about
/// the project. Which of those bodies may *declare* is the host test's question, asked in
/// [`Association::declare`] against the `models` this file is handed.
///
/// The host test is deliberately **not** a superclass test. `< ApplicationRecord` is the
/// convention, `< ActiveRecord::Base` is the older spelling, and a concern has neither, so a
/// superclass test would decline more real models than the macro names ever mis-claim. Instead a
/// `module` passes without one, and a class inheriting from a gem passes on the other half of the
/// union — being a collection element of some model that does inherit. What the macro names alone
/// would mis-claim is a class of serializers.
#[must_use]
pub fn read_model(source: &str) -> Model {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut models = Models {
        source,
        nesting: Vec::new(),
        classes: Vec::new(),
        abstract_classes: BTreeSet::new(),
    };
    models.walk(
        parsed
            .node()
            .as_program_node()
            .map(|program| program.statements().as_node()),
        false,
    );
    Model {
        classes: models.classes,
        abstract_classes: models.abstract_classes,
    }
}

/// Whether a class writing this superclass is an ActiveRecord model, with no walk left to do.
///
/// The end of the chain `Analysis::Context::is_model` climbs, and the two spellings
/// are the only two an application writes: Rails has generated an
/// `ApplicationRecord` since 5.0 and everything older says `ActiveRecord::Base`. It is a
/// **suffix** test for the first, because `Spree::ApplicationRecord` is one and a lexical
/// nesting the reference does not carry would only make the string longer — which is finding
/// 44's own rule for a mailer, asked here about a model.
///
/// A chain rather than one hop, and solidus is why: 101 of its models are two hops from the
/// base and 15 are four or five. The direction of a miss is the direction of every miss in this
/// file — a class this does not recognise claims no table, and a table nobody claims declares
/// nothing.
/// ActiveRecord's own base class, which is where Rails puts the query interface.
///
/// The query interface reaches it only when the **bundle** declares it — see
/// `Analysis::model_declarations` — so this names a class somebody else wrote rather than one
/// this crate invents, exactly as [`MESSAGE_DELIVERY`](crate::workspace::rails::MESSAGE_DELIVERY)
/// does.
pub const RECORD_BASE: &str = "ActiveRecord::Base";

#[must_use]
pub fn is_record_base(superclass: &str) -> bool {
    superclass == "ActiveRecord::Base"
        || superclass
            .rsplit("::")
            .next()
            .is_some_and(|last| last == "ApplicationRecord")
}

/// The class ya-lsp writes for a collection of `class`.
///
/// Nested under the model rather than beside it — `Comment::Relation`, not `CommentRelation` —
/// Three reasons, in the order they
/// matter: the name is *scoped*, so it cannot collide with an unrelated top-level constant the
/// way a made-up top-level name can; it reads correctly in the one place a user meets it, a
/// hover card saying `Comment::Relation#first`; and a project that already has a
/// `Comment::Relation` is exactly the project that meant something by it, which is why a
/// collision makes the pass emit nothing rather than shadow it.
#[must_use]
pub fn relation_of(class: &str) -> String {
    format!("{class}::{RELATION}")
}

/// The class a relation is a collection of — [`relation_of`] read backwards.
///
/// The inverse exists because the query interface needs it at *lookup* time and not at generation time:
/// `Story::Relation#first` is declared once for the whole project, so the only thing that says
/// which model the answer is about is the receiver's own name. See
/// [`Return::Element`](crate::analysis::types::Return::Element).
///
/// A name that is not a relation answers `None` rather than itself, because the caller's next
/// question is "and what is that model's relation" and a wrong answer to this one would invent
/// a class.
#[must_use]
pub fn element_of(relation: &str) -> Option<&str> {
    relation.strip_suffix(RELATION)?.strip_suffix("::")
}

/// The last segment of the name [`relation_of`] builds, and the one [`element_of`] takes off.
const RELATION: &str = "Relation";

impl Model {
    /// Every class some macro in this file could name, in the order Rails tries them.
    ///
    /// A list per macro rather than a name per macro, and the order is the
    /// whole feature: a bare name and a nested name may both exist, and Rails takes the nested
    /// one. [`Association::resolved`] is what turns one of these lists into an answer.
    pub fn targets(&self) -> impl Iterator<Item = &str> {
        self.classes
            .iter()
            .flat_map(|class| class.associations.iter())
            .flat_map(|association| association.candidates.iter().map(String::as_str))
    }

    /// Every body here that handed a name to the view context, and the names it handed over.
    ///
    /// The only thing this reader produces that is not a member: the `def` a
    /// `helper_method` names is already in the graph on the class the macro is written in, so
    /// what a template needs is not a declaration but the **permission** — which of a
    /// controller's methods Rails puts on `_helpers`, and which stay private to the request.
    /// `analysis::views` is the consumer and the ancestor walk is what makes a concern's export
    /// reach the controllers that include it.
    ///
    /// A body that exports nothing is not listed. A body that exports a name it does not define
    /// **is** — `helper_method :current_user` in `ApplicationController` above a `def
    /// current_user` in a concern it includes is 8 of the six corpora's 60 — because the two
    /// halves of that pair are two files and this reader sees one of them.
    pub fn exports(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.classes
            .iter()
            .filter(|class| !class.exports.is_empty())
            .map(|class| (class.name.as_str(), class.exports.as_slice()))
    }

    /// Every body here that put a module into its own view context, and which modules.
    ///
    /// Beside [`Model::exports`] and read by the same consumer. A **controller**
    /// writing one is usually saying nothing new — every `app/helpers` module is in its view
    /// context already — and a **mailer** writing one is saying the only thing that puts an
    /// application helper in front of a mailer template at all.
    pub fn helper_modules(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.classes
            .iter()
            .filter(|class| !class.helpers.is_empty())
            .map(|class| (class.name.as_str(), class.helpers.as_slice()))
    }

    /// Every class a *collection* here needs a relation for.
    ///
    /// The element of each `has_many`, and — the two that are easy to miss — the class each
    /// `scope` is written on, because `Story.recent` returns a relation of `Story`, and the
    /// class each `enum` is written on, because `enum`'s class-side pair *is* a `scope`: Rails
    /// installs it by calling `klass.scope`, so an `enum` reuses the relation class rather than
    /// growing one of its own.
    ///
    /// A **module** asks for neither of the last two, and asks for the first exactly as a class
    /// does. `has_many :comments` in a concern is still a collection of `Comment`, whoever
    /// includes it; a `scope` in one is a class method of the includer, so a `Storyish::Relation`
    /// would be a relation of a thing that has no records at all.
    ///
    /// That is unchanged by the includer fan-out, which declares the concern's `scope` on each
    /// includer: the relation it returns is the *includer's*, and every includer that may have
    /// one already does — every model owns a relation class, and this list is what gives one to
    /// every collection element. So the fan-out needs nothing from here, and asking for a
    /// relation per includer would only widen this list with names it already contains.
    ///
    /// `known` is here for the nesting walk and for nothing else: which class a `has_many` names is
    /// question about the nesting it is written in, and the relation this asks for has to be a
    /// relation of the class the macro will actually declare. Asking the two questions with two
    /// different answers is how `Spree::LineItem` would get a `Spree::Adjustment` member typed
    /// `Adjustment::Relation`.
    pub fn collections<'a>(&'a self, known: &'a BTreeSet<String>) -> impl Iterator<Item = &'a str> {
        self.classes.iter().flat_map(move |class| {
            class
                .associations
                .iter()
                .filter(|association| match association.kind {
                    Kind::Many => true,
                    Kind::Scope => !class.module,
                    Kind::One | Kind::Maybe => false,
                })
                .filter_map(|association| association.resolved(known))
                .chain(
                    (!class.module && class.enums.iter().any(Enum::scoped))
                        .then_some(class.name.as_str()),
                )
        })
    }

    /// Every `(class, column)` a macro here re-types, so the schema can decline the column.
    ///
    /// The one thing a generator in this directory tells another, and it is deliberately an
    /// *input* rather than a fact: a column is declared into `db/schema.rb`'s generated document
    /// and the macro into the model's, so `Facts`' precedence — which is per document — can
    /// never see the pair. The rank has to be spent by the loser declining.
    ///
    /// **Three macros re-type a column and Rails documents all three.** `story.status` is the
    /// label an `enum` names and the column holds the integer it is stored as; `attribute` "will
    /// override the type of existing attributes if needed", which is `attributes.rb`'s own
    /// sentence; and a `serialize` replaces the type of the column its coder round-trips
    /// through, which is the one of the three that is right even when what replaces it is
    /// `untyped` — a `text` column carrying YAML answers a `Hash` in Ruby and never a `String`.
    /// Every `attribute` this file holds named a cast type — that is the only shape
    /// [`attributes::read`] admits — so every one of them re-types its column.
    ///
    /// A **module** is not asked. A concern claims no table, and the concern's own includers —
    /// which are the classes whose columns its `attribute` really does re-type — are not
    /// something this pass can see; leaving the column standing is the direction that keeps an
    /// answer rather than losing one.
    pub fn retyped_columns(&self) -> impl Iterator<Item = (&str, &str)> {
        self.classes
            .iter()
            .filter(|class| !class.module)
            .flat_map(|class| {
                class
                    .enums
                    .iter()
                    .map(|declared| (class.name.as_str(), declared.attribute()))
                    .chain(
                        class.attributes.iter().filter_map(|declared| {
                            Some((class.name.as_str(), declared.retypes()?))
                        }),
                    )
                    .chain(
                        class
                            .tail
                            .iter()
                            .flat_map(|declared| declared.retypes())
                            .map(|name| (class.name.as_str(), name)),
                    )
            })
    }

    /// The RBS this file's macros declare.
    ///
    #[must_use]
    pub fn signatures(&self, file: &str, elsewhere: &Elsewhere<'_>) -> Facts {
        let Elsewhere {
            known,
            framework,
            models,
            relations,
            emit,
            bases,
            ..
        } = *elsewhere;
        let mut facts = Facts::default();
        for class in &self.classes {
            for association in &class.associations {
                association.declare(&mut facts, file, class, elsewhere);
            }
            for declared in &class.enums {
                declared.declare(
                    &mut facts,
                    file,
                    &class.name,
                    relations.contains(&class.name),
                );
            }
            // A concern's rule, and `attribute` meets it where a `scope` does not: the member is
            // one type whoever includes the concern, so a module owns it exactly as a class
            // does. It is also the last generator to speak for this class, which is `Source`'s
            // rank 6 doing its work — an `enum` or an association of the same name has already
            // claimed the position and keeps it. Nothing in six corpora writes that pair.
            let owner = if class.module {
                Owner::Module(class.name.clone())
            } else {
                Owner::Instance(class.name.clone())
            };
            // `attribute` asks the same host test, being the second reader that needs one.
            let admitted = class.module || models.contains(&class.name);
            for declared in &class.attributes {
                let shadowed = elsewhere
                    .columns
                    .contains(&(class.name.clone(), declared.name().to_owned()));
                declared.declare(&mut facts, file, &owner, admitted && !shadowed);
            }
            // The seventeen long-tail families, and the three gates they need: `known` for a class
            // this application defines, `framework` for one a gem does, and `defined` for a name
            // the body's own `def` already answers. Last of the four, which is `Source::Derived`
            // doing its work — everything a file says about a name more directly has already
            // claimed the position and keeps it.
            let host = Host {
                class: &class.name,
                module: class.module,
                defined: &class.defined,
            };
            for declared in &class.tail {
                declared.declare(&mut facts, file, &host, known, framework);
            }
        }
        for element in emit {
            relation(&mut facts, element);
        }
        // The class side and the callbacks are **inherited**, so
        // they are written once per base class rather than once per model — [`class_side`] has
        // the argument, and `bases` is what a caller that knows the superclass chain hands over.
        // `abstract_classes` is no longer consulted here and the field is kept for the relation
        // half's own caller: a base is normally the abstract class, and that is now the point.
        for base in bases {
            callbacks(&mut facts, base);
            class_side(&mut facts, base);
        }
        facts
    }

    /// Every class in this file that says it is abstract — see [`declares_abstract`].
    pub fn abstract_classes(&self) -> impl Iterator<Item = &str> {
        self.abstract_classes.iter().map(String::as_str)
    }

    /// Whether this file has anything for the second phase to do.
    ///
    /// Asked so that the union of every fact in the project — which is one merge per settle — is
    /// built only for a workspace that writes a `delegate` or an `alias_attribute`. A project
    /// with neither pays nothing at all for the phase.
    #[must_use]
    pub fn derives(&self) -> bool {
        self.classes
            .iter()
            .any(|class| !class.delegates.is_empty() || class.tail.iter().any(Tail::derived))
    }

    /// What a `delegate` and an `alias_attribute` declare, given everything
    /// already said.
    ///
    /// Separate from [`Model::signatures`] because it is a second *phase* and not a second
    /// reader: the names were read in the same walk of the same parse, and what could not be
    /// known then is what `Story#user` returns — a fact some other file's generator writes in
    /// this same run. `project` is that, merged; see
    /// [`Delegate::declare`](super::delegates::Delegate::declare) for what the two hops ask, and
    /// [`Tail::declare_derived`](super::tail::Tail::declare_derived) for the one hop an alias
    /// needs.
    #[must_use]
    pub fn derived(&self, file: &str, project: &Facts) -> Facts {
        let mut facts = Facts::default();
        for class in &self.classes {
            // A concern's rule, unchanged: an instance member of one hangs on the module and
            // reaches every includer through the `include` the user already wrote. A `delegate`
            // is an instance member like any other, and unlike a `scope` it names one type
            // whoever includes it — `delegate :name, to: :user` is `User#name` in every one.
            let owner = if class.module {
                Owner::Module(class.name.clone())
            } else {
                Owner::Instance(class.name.clone())
            };
            for delegate in &class.delegates {
                delegate.declare(&mut facts, file, &owner, project);
            }
            for declared in &class.tail {
                declared.declare_derived(&mut facts, file, &owner, project, &class.defined);
            }
        }
        facts
    }
}

/// What a model file's macros need to know about every file but this one.
///
/// One parameter rather than six, and [`types::Sources`](crate::analysis::types::Sources) is the
/// precedent: six things that always travel together are one thing with six fields, and keeping
/// them apart is how a signature reaches eight arguments and stops being readable — which is
/// exactly what the includer fan-out does to it.
///
/// Every field is the *whole project's* rather than this file's, and each for the same reason:
/// the file that writes a fact is rarely the file that holds the evidence for it.
///
/// - `known` is every class the application itself defines; a macro naming anything else
///   declares nothing, which is the clause that keeps `belongs_to :parent_comment` from
///   inventing a `ParentComment` and the reason `class_name:` is a correctness requirement
///   rather than a refinement — 32 of lobsters' 76 singular associations carry one.
/// - `framework` is every class a gem defines, which the long tail asks about and no
///   association does.
/// - `relations` is every element class that has a generated relation class *somewhere*, and
///   `emit` is the subset this file is the one to write. They are two sets because a relation
///   class belongs to an element type and not to a file: `has_many :comments` in four models is
///   one `Comment::Relation`, and the caller picks which of the four writes it.
/// - `abstract_classes`: the file that writes `ApplicationRecord::Relation` is
///   rarely `application_record.rb`.
/// - `includers` is the concern fan-out's, and it is the whole project's for a stronger version of the same
///   reason: the classes a concern's `scope` lands on are named in files this one has never
///   heard of, and a concern that is included nowhere declares nothing.
pub struct Elsewhere<'a> {
    pub known: &'a BTreeSet<String>,
    pub framework: &'a BTreeSet<String>,
    /// Every class this project treats as an ActiveRecord model.
    ///
    /// The host test, and it is the **union** `relations` is filtered out of rather than
    /// a fourth notion of what a model is: a class inherits `ActiveRecord::Base` through
    /// `Context::models`' walk, or some macro in the project made it a collection element. The
    /// two filters `relations` adds are about *emitting a relation class* — a name the
    /// application has already used, a name rubydex invented — and neither is a reason to stop
    /// believing the class is a model, so they are applied there and not here.
    pub models: &'a BTreeSet<String>,
    pub relations: &'a BTreeSet<String>,
    pub emit: &'a BTreeSet<String>,
    /// The base classes whose class side and callbacks this document writes.
    ///
    /// A *base* and not a model: `Story.where` is inherited, so one copy on `ApplicationRecord`
    /// answers for every model under it. [`class_side`] has the argument and
    /// `Analysis::model_declarations` decides which document writes which base.
    pub bases: &'a BTreeSet<String>,
    pub includers: &'a BTreeMap<String, BTreeSet<String>>,
    /// Every `(class, member)` an earlier generator in this pass already declared.
    ///
    /// It holds the schemas' columns because that is what runs before this: an
    /// untyped `attribute :note` on a class whose table has a `note` column must declare
    /// nothing, because `attributes.rb` says a call with no cast type keeps "the previously
    /// defined type", which *is* the column — so the two would otherwise be one silent overload
    /// set and the type would be whichever `Types::harvest` read last.
    pub columns: &'a BTreeSet<(String, String)>,
}

/// The empty project, for a test that cares about one field and has to state six.
///
/// `..Elsewhere::nothing()` is what lets a test say only what it is about — which is also what
/// the seven positional arguments it replaced could not do, since every one of them had to be
/// written whether or not the case under test had anything to put in it.
#[cfg(test)]
impl<'a> Elsewhere<'a> {
    pub(super) fn nothing() -> Self {
        use std::sync::LazyLock;

        static NOTHING: LazyLock<BTreeSet<String>> = LazyLock::new(BTreeSet::new);
        static NOBODY: LazyLock<BTreeMap<String, BTreeSet<String>>> = LazyLock::new(BTreeMap::new);
        static NO_COLUMNS: LazyLock<BTreeSet<(String, String)>> = LazyLock::new(BTreeSet::new);

        Self {
            known: &NOTHING,
            framework: &NOTHING,
            models: &NOTHING,
            relations: &NOTHING,
            emit: &NOTHING,
            bases: &NOTHING,
            includers: &NOBODY,
            columns: &NO_COLUMNS,
        }
    }
}

impl Association {
    /// The class this macro names, or `None` where the application defines none of them.
    ///
    /// Rails. order, and the order is the whole of it: where a bare name and a nested name both
    /// exist, Rails takes the nested one. Taking the bare one is **wrong** at real sites — most
    /// often a `db/migrate` throwaway model shadowing the application's own, but also names like
    /// `ActiveStorage::Attachment` and `Blazer::Audit`, which are neither migrations nor harmless.
    ///
    /// A name that is not a constant at all stays declined however it is nested, which is the
    /// clause solidus needs: its admin controllers write `belongs_to "spree/order"` ten times
    /// from `Spree::Admin::ResourceController`, and `Spree/order` is nonsense with every prefix
    /// this list can put in front of it.
    fn resolved<'a>(&'a self, known: &BTreeSet<String>) -> Option<&'a str> {
        self.candidates
            .iter()
            .find(|candidate| known.contains(*candidate))
            .map(String::as_str)
    }

    /// Say this macro's member, or decline and say nothing.
    fn declare(
        &self,
        facts: &mut Facts,
        file: &str,
        class: &ModelClass,
        elsewhere: &Elsewhere<'_>,
    ) {
        let Elsewhere {
            known,
            models,
            relations,
            includers,
            ..
        } = *elsewhere;
        // The one gate in this file that asks about the class the macro is written **on** rather
        // than about the class it names.
        //
        // Both serializer gems spell `has_many`, `has_one` and `belongs_to`, store what they are
        // given and define **no method**. Unlike `attribute` there is no *shape* to gate on:
        // `has_many :statuses` is byte-identical in a model and in a serializer. So it has to be
        // the host.
        //
        // **An admit list and not a decline list**, which costs the same and covers a gem without
        // naming it — a controller defining its own class-side `belongs_to` for nested-resource
        // routing declines by the same sentence, where a blocklist would have to be told about
        // each such gem one at a time. **A `module` passes unconditionally**: a concern inherits
        // nothing, and most association calls written outside a model are written in one.
        //
        // Nothing real is lost because `models` is the *union*. A model whose base class lives in
        // a gem's `lib/` is out of reach of `Context::models`' walk, and is admitted anyway as a
        // collection element of some model that names it.
        //
        // It is provably a no-op for [`Kind::Scope`]: `relations` is a subset of `models`, and a
        // `scope` on a class already declines below unless `relations` holds its name.
        if !class.module && !models.contains(&class.name) {
            return;
        }
        let Some(target) = self.resolved(known) else {
            return;
        };
        // `has_many :voters, through: :votes` reads a second association, and an intermediate
        // that is not declared on this class is a macro whose meaning is somewhere this reader
        // cannot see. Declining is the same answer a missing class gets.
        if let Some(through) = &self.through
            && !class
                .associations
                .iter()
                .any(|other| other.name == *through)
        {
            return;
        }
        // A `scope` in a concern is the one macro a module body reads and refuses to write
        // down, and this is where it is written. Rails `class_eval`s `included do` on the
        // *including* class, so `scope :expired` in `Expireable` is `Poll.expired` **and**
        // `Invite.expired` — six different relation types for one line, none of them the
        // module's. Declaring it on the module's own singleton would answer
        // `Expireable.expired`, which raises, and would still leave `Poll.expired` unanswered.
        // So the declaration goes on each includer instead, once per pair, and the span still
        // points at the one `scope` line in the concern.
        //
        // **What bounds it is `relations`**, and it needs no gate of its own: every model already
        // owns a relation class, so a `scope` fanned onto one has a type to return, and a class
        // that is neither a model nor a collection element — a PORO that includes a concern —
        // declines here exactly as a `has_many` naming it would. Inventing a `Relation` for a
        // class with no table is the one way this could answer worse rather than not at all.
        if class.module && self.kind == Kind::Scope {
            for includer in includers.get(&class.name).into_iter().flatten() {
                if !relations.contains(includer) {
                    continue;
                }
                facts.declare(Declared {
                    owner: Owner::Singleton(includer.clone()),
                    name: self.name.clone(),
                    returns: relation_of(includer),
                    parameters: "(*untyped)".to_owned(),
                    because: format!(
                        "From `{file}`, `{} :{}` in `{}`, which `{includer}` includes.",
                        self.spelled, self.name, class.name
                    ),
                    at: Some((self.at, self.name_at)),
                    from: Source::Association,
                    overloads: Vec::new(),
                });
            }
            return;
        }
        let returns = match self.kind {
            Kind::One if self.optional => format!("{target}?"),
            Kind::One => target.to_owned(),
            Kind::Maybe => format!("{target}?"),
            Kind::Many | Kind::Scope => {
                if !relations.contains(target) {
                    return;
                }
                relation_of(target)
            }
        };
        // A scope is a class method, and rubydex files `def self.` on the singleton exactly as
        // it files `Story.recent` there — so this is the same fact written on both sides. An
        // instance member of a concern hangs on the module, which is the whole mechanism:
        // rubydex indexes an RBS `include` exactly as it indexes a Ruby one, so the member
        // reaches every includer through the `include` the user already wrote.
        let (owner, parameters) = match self.kind {
            Kind::Scope => (Owner::Singleton(class.name.clone()), "(*untyped)"),
            _ if class.module => (Owner::Module(class.name.clone()), "()"),
            _ => (Owner::Instance(class.name.clone()), "()"),
        };
        facts.declare(Declared {
            owner: owner.clone(),
            name: self.name.clone(),
            returns,
            parameters: parameters.to_owned(),
            because: format!(
                "From `{file}`, `{} :{}`{}.",
                self.spelled,
                self.name,
                if self.kind == Kind::Scope {
                    String::new()
                } else {
                    format!(", which is a `{target}`")
                }
            ),
            at: Some((self.at, self.name_at)),
            from: Source::Association,
            overloads: Vec::new(),
        });
        // Everything else the one macro line installs, and the list is Rails' own rather than
        // this reader's: `associations/builder/` — `Association::define_readers`/`define_writers`
        // for the pair every macro writes, `SingularAssociation::define_accessors` for the five a
        // `belongs_to` or a `has_one` adds, `CollectionAssociation`'s pair for `_ids`, and
        // `BelongsTo::define_change_tracking_methods`.
        //
        // Two things the builders say that a reading of the macro names would not. The
        // constructors are written `unless reflection.polymorphic?`, which costs this reader
        // nothing either way because a polymorphic `belongs_to` names no class and `resolved`
        // declined it above. And `_changed?` is `belongs_to`'s alone, `BelongsTo` being the only
        // builder that overrides `define_change_tracking_methods`.
        //
        // **Four are declined on a measurement**: `reload_<name>`, `reset_<name>`,
        // `<name>_changed?` and `<name>_previously_changed?` are each written a handful of times
        // across six applications, against two declarations on every singular association and two
        // more on every `belongs_to` — thousands. The four that ship price the other way round.
        let mut installs = |name: String, parameters: String, returns: String| {
            facts.declare(Declared {
                owner: owner.clone(),
                name: name.clone(),
                returns,
                parameters,
                because: format!(
                    "From `{file}`, `{} :{}`, which also installs `{name}`.",
                    self.spelled, self.name
                ),
                at: Some((self.at, self.name_at)),
                from: Source::Association,
                overloads: Vec::new(),
            });
        };
        match self.kind {
            Kind::One | Kind::Maybe => {
                // **The writer is nilable whatever the reader is**, and the two are not a copy
                // of each other: `belongs_to :user` reads a `User` because Rails 5 made the
                // association required, and `story.user = nil` is still ordinary Ruby that
                // raises nothing — the validation is what fails, at save. Assigning a subclass
                // hands the subclass back, so the declared type is a supertype of every value
                // the call can return rather than an approximation of one.
                installs(
                    format!("{}=", self.name),
                    format!("({target}?)"),
                    format!("{target}?"),
                );
                // **These are not nilable and the reader may be**, which is the row here that
                // is not a copy of the reader's either, in the other direction:
                // `belongs_to :user, optional: true` reads a `User?` because the row may not be
                // there, and `create_user` *makes* one.
                for name in [
                    format!("build_{}", self.name),
                    format!("create_{}", self.name),
                    format!("create_{}!", self.name),
                ] {
                    installs(
                        name,
                        format!("(*untyped) ?{{ ({target}) -> untyped }}"),
                        target.to_owned(),
                    );
                }
            }
            Kind::Many => {
                // A collection writer takes an array *or* a relation and hands back whatever it
                // was given, so unlike the singular one there is no type to state: assigning
                // `[a, b]` returns an `Array` and assigning a relation returns the relation.
                installs(
                    format!("{}=", self.name),
                    "(untyped)".to_owned(),
                    "untyped".to_owned(),
                );
                // `ids_reader` is `pluck(primary_key)`, and what a primary key holds is the
                // schema's to say — in a *different* generated document, which this reader
                // cannot ask. `Array[untyped]` is what is known: an `Integer` would be right
                // for a `bigint` and wrong for every `id: :uuid` table, and the element type is
                // not what these 1,191 call sites want. The array is, and it answers `each`,
                // `map`, `size` and `include?` exactly.
                //
                // Rails singularizes the **association's own name** and not the class it
                // resolves to — `has_many :authors, class_name: "User"` is `author_ids` — so
                // this reads `self.name` and never `target`.
                let ids = format!("{}_ids", singularize(&self.name));
                installs(ids.clone(), "()".to_owned(), "Array[untyped]".to_owned());
                installs(
                    format!("{ids}="),
                    "(untyped)".to_owned(),
                    "untyped".to_owned(),
                );
            }
            // A `scope` is not an association: `define_readers` never runs for one, and the
            // class method declared above is the whole of what the macro installs.
            Kind::Scope => {}
        }
    }
}

/// The class every relation ya-lsp writes inherits from, and where the query interface lives.
///
/// One copy for the project, and the whole of it. Spelled once per
/// element type — 46 names on `Story::Relation`, 46 more on `Comment::Relation` — because four
/// dozen of its signatures name what the collection holds. Two receiver-relative return types
/// take that dependency out of the text: [`Return::Element`](crate::analysis::types::Return::Element)
/// means "the model this receiver is about" and
/// [`Return::Collection`](crate::analysis::types::Return::Collection) means "that model's
/// relation", so `def first: () -> ActiveRecordElement?` written **once** answers `Story` on
/// `Story::Relation` and `Comment` on `Comment::Relation`. What each model's document then
/// holds is one line — `class Story::Relation < ActiveRecordRelation end`.
///
/// **A superclass and not an `include`**, which is a correction to the obvious shape rather
/// than a preference: the shared half was a module because a module is what an `include` can
/// name, and the class side could not use it at all. A superclass carries both sides — a class
/// object's singleton chain follows the class chain — and it is the only spelling that does.
///
/// **It works because nothing else declares `Story::Relation`.** A generated superclass on a
/// class the user's own file already gives one is **silently ignored**: measured, a `class
/// Widget < SpikeBase` in RBS beside a `class Widget < ApplicationRecord` in Ruby leaves
/// `SpikeBase`'s members unreachable, with no error and no diagnostic. That is why the class
/// side inherits through the model's **own** base class — see [`class_side`] — instead of
/// through one this crate invents.
///
/// Top-level for [`ROUTE_HELPERS`](crate::workspace::rails::ROUTE_HELPERS)' reason, and the
/// same collision rule: a project that already declares this name means something by it, and
/// the pass writes nothing rather than shadowing it.
///
/// **What it costs is the hover card.** `story.comments.where(...)` prints
/// `ActiveRecordRelation#where` rather than `Comment::Relation#where`, for every name in the
/// interface. The element is gone from the card and still in the *answer*, which is the half a
/// reader chains off.
pub const RELATION_BASE: &str = "ActiveRecordRelation";

/// One name of ActiveRecord's query interface, and the evidence for each side it goes on.
struct Query {
    name: &'static str,
    /// Everything before the `->`: the positional parameters, and the block where the method
    /// hands one the element.
    parameters: String,
    returns: String,
    /// Every arm after the first, for the four names one signature cannot state.
    overloads: Vec<(String, String)>,
    /// Which of the two types the name is really on.
    side: Side,
}

/// Where ActiveRecord puts a name, which is the whole of what [`Query`] carries a side for.
///
/// `ActiveRecord::Querying::QUERYING_METHODS` is the class side **by construction** —
/// `delegate(*QUERYING_METHODS, to: :all)` is the single line that makes `Story.where` mean
/// `Story.all.where` — so which names it holds is read out of Rails rather than assumed, for
/// the `enum`'s reason. A name it does not hold exists on the relation and **raises on the
/// model**, measured at 64 lobsters positions; and the traffic runs the other way too:
/// `create!` is a class method every application
/// writes and no relation answers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    /// In `QUERYING_METHODS`: the relation defines it and the model delegates to `all`.
    Both,
    /// `Relation`'s own, and either a `NoMethodError` on the model or somebody else's method
    /// there. `size`, `length`, `empty?`, `to_a` and `each` are relation-only, which takes the
    /// last two off the singleton; `new` is the sixth and is here for the other reason — a model
    /// really does answer it, from `Class`, and a declaration on the class side would shadow it.
    Relation,
    /// `Persistence::ClassMethods`', and nothing on a relation answers it.
    ///
    /// **One name**, and a list of six is easy to arrive at: a completion sweep catches the
    /// other five: `activerecord/lib/active_record/relation.rb` defines `new` at 126, `build`
    /// as an alias of it at 134, `create` at 155, `create!` at 170, `update` at 640 and
    /// `update!` at 664 — so `story.comments.create!` really is a call, and declaring those
    /// class-only took the name-matched offer away from it and gave nothing back.
    /// `instantiate` is the one `Relation` genuinely does not define.
    Class,
}

/// Every name in `ActiveRecord::Querying::QUERYING_METHODS` that hands back a relation.
///
/// One list rather than one entry each, because the whole content of the row is the name: they
/// take anything and they return the relation, which is what makes a query chainable at all.
/// `with` is the odd one and is here for the same reason it is anywhere — `QueryMethods#with`
/// is on the relation and `Querying#with` is a `def` beside the list rather than a member of it,
/// so it is on both sides exactly as the delegated names are.
const RELATIONAL: [&str; 40] = [
    "reselect",
    "order",
    "regroup",
    "in_order_of",
    "reorder",
    "default_order",
    "group",
    "limit",
    "offset",
    "joins",
    "left_joins",
    "left_outer_joins",
    "where",
    "rewhere",
    "invert_where",
    "preload",
    "eager_load",
    "includes",
    "from",
    "lock",
    "readonly",
    "and",
    "or",
    "annotate",
    "optimizer_hints",
    "extending",
    "having",
    "create_with",
    "distinct",
    "references",
    "none",
    "unscope",
    "merge",
    "except",
    "only",
    "strict_loading",
    "excluding",
    "without",
    "with_recursive",
    "with",
];

/// The ordinal finders that answer a record or nothing, and their bang twins that raise.
///
/// Rails writes them out one by one in `FinderMethods` and so does this: `second` through
/// `fifth`, `forty_two` — which is a joke that has been in ActiveRecord since 2013 and is a real
/// method — and the two counted from the other end. `first`, `last` and `take` are **not** here,
/// because each of them takes an optional count that changes what it hands back, which is
/// [`Query::overloads`]'.
const ORDINALS: [&str; 7] = [
    "second",
    "third",
    "fourth",
    "fifth",
    "forty_two",
    "third_to_last",
    "second_to_last",
];

/// The eight names that find a record or make one, and hand back the record either way.
const CREATORS: [&str; 8] = [
    "first_or_create",
    "first_or_create!",
    "first_or_initialize",
    "find_or_create_by",
    "find_or_create_by!",
    "find_or_initialize_by",
    "create_or_find_by",
    "create_or_find_by!",
];

/// The names that hand back a `Promise` and nothing else — `ActiveRecord::Promise`, which is
/// resolved by `#value` and is a class no generator here writes.
///
/// Declared anyway, `untyped`, which is `delegate`'s inversion applied to a *name*: eight of them
/// are in `QUERYING_METHODS` and this table's bound is that list rather than a judgement about
/// which of its names deserve to be in it. The type declines and
/// [`Types::harvest`](crate::analysis::types::Types::harvest) drops an `untyped`, so what they
/// cost is a completion entry each and what they buy is that `Story.async_count` resolves to
/// something instead of to the name rung.
const ASYNC: [&str; 8] = [
    "async_ids",
    "async_count",
    "async_average",
    "async_minimum",
    "async_maximum",
    "async_sum",
    "async_pluck",
    "async_pick",
];

/// The bulk writers, which all hand back an `ActiveRecord::Result` or the ids it carries.
const WRITES: [&str; 6] = [
    "insert",
    "insert_all",
    "insert!",
    "insert_all!",
    "upsert",
    "upsert_all",
];

/// The query interface ActiveRecord installs, written once and declared on the sides it is on.
///
/// Each entry is a signature *without* its `def`, because most of these facts are true twice: on
/// the relation class as instance methods, and on the model's own singleton as class methods.
/// Writing the list once is what makes "the singleton and the relation cannot disagree about what
/// `where` returns" a property of this file rather than of somebody remembering.
///
/// # The bound is Rails' own list
///
/// A table chosen for being *typeable* is a bound nobody can check. **The list is
/// `QUERYING_METHODS`**, plus the two places Rails puts a class method that is not in it:
/// `Persistence::ClassMethods`, where `create!` lives, and `Querying#with`, a `def` beside the
/// constant rather than an entry in it. A name is in this table because Rails put it on a model,
/// not because ya-lsp could think of a type for it — which is why the async family and the bulk
/// writers are here at all.
///
/// **What can still be refused is the type**, which is `delegate`'s inversion and why the width is
/// safe. `pick`, `calculate`, `minimum`, `maximum`, every `async_*` and every bulk writer return
/// `untyped`: the name resolves, the chain stops, and
/// [`Types::harvest`](crate::analysis::types::Types::harvest) drops the claim rather than carrying
/// a wrong one.
///
/// # The approximations, stated rather than hidden
///
/// - **A scalar or an array in one argument.** `find`, `create`, `create!`, `build`, `instantiate`
///   and `destroy` hand back a record for a scalar and an `Array` for an array, and both calls
///   write **one** positional argument, so [`Arity`](crate::analysis::cursor::Arity) cannot tell
///   them apart. Each types the singular. `update` and `update!` are the exception and return
///   `untyped`: their first parameter *defaults to `:all`*, so the array is not even the unlikely
///   answer.
/// - **`count` after a `group` is a `Hash`** and this says `Integer` unconditionally — the same
///   order of inexactness as `where` always returning a relation.
/// - **`pluck` and `ids` are `Array[untyped]`**, not `Array[Element]`: `Story.pluck(:title)` is an
///   array of *columns*, and all this module knows is that it is an `Array`.
///
/// # What `where` cannot say
///
/// `where` with **no argument** returns a `QueryMethods::WhereChain`, which is where `not`,
/// `missing` and `associated` live. An arity split beside `first`'s is **not expressible** for it:
/// `arity_of` deliberately does not count a keyword hash as a positional argument, so that
/// `3.7.round(half: :up)` reaches the zero-argument arm — which makes `where()` and
/// `where(title: "x")` the same call to this machinery. An arm answering `WhereChain` at arity 0
/// would answer it for the commonest call in Rails. So `where` returns a relation on every arm,
/// `WhereChain` is not generated, and `where.not` stays on the name rung: a bound on the arity
/// partition rather than a missing table.
fn query_interface() -> Vec<Query> {
    let element = ELEMENT;
    let relation = COLLECTION.to_owned();
    let nilable = format!("{element}?");
    let records = format!("Array[{element}]");
    let taking_element = || format!("(*untyped) ?{{ ({element}) -> untyped }}");
    let plain = |name, parameters: String, returns: String, side| Query {
        name,
        parameters,
        returns,
        overloads: Vec::new(),
        side,
    };
    let both =
        |name, parameters: String, returns: String| plain(name, parameters, returns, Side::Both);
    let mut queries = Vec::new();

    // The three whose optional count changes the answer, and the one whose block does. Every
    // other name in this function states one arm.
    for name in ["first", "last", "take"] {
        queries.push(Query {
            name,
            parameters: "()".to_owned(),
            returns: nilable.clone(),
            overloads: vec![("(Integer)".to_owned(), records.clone())],
            side: Side::Both,
        });
    }
    queries.push(Query {
        name: "select",
        parameters: "(*untyped)".to_owned(),
        returns: relation.clone(),
        // `Enumerable#select` reached through `super`, so the block is handed an element and the
        // result is an `Array` of them rather than a relation. A *required* block, which is what
        // puts this arm on the other side of the partition from the one above it.
        overloads: vec![(format!("() {{ ({element}) -> untyped }}"), records.clone())],
        side: Side::Both,
    });

    for name in RELATIONAL {
        queries.push(both(name, "(*untyped)".to_owned(), relation.clone()));
    }
    for name in ORDINALS {
        queries.push(both(name, "()".to_owned(), nilable.clone()));
    }
    // Every ordinal has a bang twin that raises instead of answering `nil`, and so do the three
    // above; `sole` is `FinderMethods`' own and behaves the same way.
    for name in [
        "first!",
        "last!",
        "take!",
        "second!",
        "third!",
        "fourth!",
        "fifth!",
        "forty_two!",
        "third_to_last!",
        "second_to_last!",
        "sole",
    ] {
        queries.push(both(name, "()".to_owned(), element.to_owned()));
    }
    queries.push(both("find", "(untyped)".to_owned(), element.to_owned()));
    queries.push(both("find_by", "(*untyped)".to_owned(), nilable.clone()));
    queries.push(both(
        "find_by!",
        "(*untyped)".to_owned(),
        element.to_owned(),
    ));
    queries.push(both(
        "find_sole_by",
        "(*untyped)".to_owned(),
        element.to_owned(),
    ));
    for name in CREATORS {
        queries.push(both(name, taking_element(), element.to_owned()));
    }

    // The predicates. `exists?` takes conditions and no block; the other four are `Enumerable`'s
    // reached through `super`, so the block is optional and is handed an element.
    queries.push(both("exists?", "(*untyped)".to_owned(), "bool".to_owned()));
    for name in ["any?", "none?", "one?"] {
        queries.push(both(name, taking_element(), "bool".to_owned()));
    }
    queries.push(both(
        "many?",
        format!("() ?{{ ({element}) -> untyped }}"),
        "bool".to_owned(),
    ));

    // How many rows a write touched.
    for (name, parameters) in [
        ("delete", "(untyped)"),
        ("delete_all", "()"),
        ("delete_by", "(*untyped)"),
        ("update_all", "(untyped)"),
        ("touch_all", "(*untyped)"),
    ] {
        queries.push(both(name, parameters.to_owned(), "Integer".to_owned()));
    }
    // The two that instantiate what they remove and hand the records back.
    queries.push(both("destroy_all", "()".to_owned(), records.clone()));
    queries.push(both("destroy_by", "(*untyped)".to_owned(), records.clone()));
    queries.push(both("destroy", "(untyped)".to_owned(), element.to_owned()));

    // `Batches`. The block is **required** on all three: without one each returns an enumerator,
    // and an optional-block arm would claim `void` for a call that chains off it.
    queries.push(both(
        "find_each",
        format!("(*untyped) {{ ({element}) -> void }}"),
        "void".to_owned(),
    ));
    queries.push(both(
        "find_in_batches",
        format!("(*untyped) {{ (Array[{element}]) -> void }}"),
        "void".to_owned(),
    ));
    queries.push(both(
        "in_batches",
        format!("(*untyped) {{ ({relation}) -> void }}"),
        "void".to_owned(),
    ));

    // `Calculations`.
    queries.push(both("count", taking_element(), "Integer".to_owned()));
    queries.push(both(
        "average",
        "(untyped)".to_owned(),
        "Numeric?".to_owned(),
    ));
    // No block arm: `Enumerable#sum` with one hands back whatever the block summed, and a
    // relation's own `sum` is a number. Stating only the blockless arm is what makes
    // `Story.sum { ... }` answer nothing rather than answer wrongly.
    queries.push(both("sum", "(*untyped)".to_owned(), "Numeric".to_owned()));
    for (name, parameters) in [
        ("minimum", "(untyped)"),
        ("maximum", "(untyped)"),
        ("calculate", "(untyped, untyped)"),
        ("pick", "(*untyped)"),
    ] {
        queries.push(both(name, parameters.to_owned(), "untyped".to_owned()));
    }
    queries.push(both(
        "pluck",
        "(*untyped)".to_owned(),
        "Array[untyped]".to_owned(),
    ));
    queries.push(both("ids", "()".to_owned(), "Array[untyped]".to_owned()));
    // `preload(association).collect(&association)`, so an array of whatever the association is.
    queries.push(both(
        "extract_associated",
        "(untyped)".to_owned(),
        "Array[untyped]".to_owned(),
    ));

    for name in ASYNC.into_iter().chain(WRITES) {
        queries.push(both(name, "(*untyped)".to_owned(), "untyped".to_owned()));
    }

    // `Relation`'s own, which raise on the model — the reason [`Side`]
    // exists rather than a `delegated` flag that only ever subtracted.
    queries.push(plain(
        "to_a",
        "()".to_owned(),
        records.clone(),
        Side::Relation,
    ));
    queries.push(plain(
        "each",
        format!("() {{ ({element}) -> void }}"),
        relation.clone(),
        Side::Relation,
    ));
    for name in ["size", "length"] {
        queries.push(plain(
            name,
            "()".to_owned(),
            "Integer".to_owned(),
            Side::Relation,
        ));
    }
    queries.push(plain(
        "empty?",
        "()".to_owned(),
        "bool".to_owned(),
        Side::Relation,
    ));

    // `relation.rb:1209` is `def reload; reset; load; end` and `load` hands back `self`, so a
    // reloaded relation is the relation. It is [`Side::Relation`] like the four above it —
    // `ActiveRecord::Base#reload` is an *instance* method, so `Story.reload` reaches `Class`,
    // finds nothing and raises. `CollectionProxy` overrides it and also returns the receiver,
    // which is what makes one row right for both. Measured over six corpora on a receiver
    // naming a declared `has_many`: **66** call sites.
    queries.push(plain(
        "reload",
        "()".to_owned(),
        relation.clone(),
        Side::Relation,
    ));

    // `Persistence::ClassMethods`: the names that are not in `QUERYING_METHODS` at all, which
    // is why `create!` was missing from a table built out of that constant alone. **Five of
    // these six are on the relation too** and `relation.rb` is what says so — see [`Side::Class`].
    for name in ["create", "create!", "build"] {
        queries.push(both(name, taking_element(), element.to_owned()));
    }
    // `relation.rb:134` is `alias build new` — the *same method* — so declaring one and not the
    // other was an incoherence rather than a bound. It is the relation's alone because a model
    // gets `new` from `Class`, which is where the class side would have shadowed something real.
    // Measured over six corpora on a receiver naming a declared `has_many`: `.new` 174 sites
    // against `.build`'s 178.
    queries.push(plain(
        "new",
        taking_element(),
        element.to_owned(),
        Side::Relation,
    ));
    for name in ["update", "update!"] {
        queries.push(both(name, "(*untyped)".to_owned(), "untyped".to_owned()));
    }
    queries.push(plain(
        "instantiate",
        "(*untyped)".to_owned(),
        element.to_owned(),
        Side::Class,
    ));

    queries
}

/// Where each of [`callback_names`]' four groups comes from, for the provenance line.
///
/// Named rather than described, because "which callbacks exist" is a question with a file that
/// answers it and the card should say which file.
const ONLY_AFTER: &str = "`define_model_callbacks :initialize, :find, :touch, only: :after`";
const EVERY_PREFIX: &str = "`define_model_callbacks :save, :create, :update, :destroy`";
const VALIDATION: &str = "`ActiveModel::Validations::Callbacks`";
const TRANSACTION: &str = "`ActiveRecord::Transactions`";

/// Every class-side callback registrar ActiveRecord installs on a model, and what installed it.
///
/// A convention with **no macro
/// behind it at all**. `before_create` is a `def` in activesupport that
/// `define_model_callbacks` wrote at boot, so no file in the workspace declares it and the graph
/// correctly found nothing — which left the name rung answering, and the name rung found
/// `Fabrication::Schematic::Evaluator#before_create` in a gem.
///
/// **The four groups are Rails' own four call sites**, walked rather than remembered, for item
/// 19's reason: a table this crate writes from a reading of the docs is a table that goes stale
/// silently. That is also what makes this **twenty-three** names rather than the thirty
/// a `before`/`around`/`after` × ten events product rule would give. Seven of those thirty do
/// not exist and Ruby raises on each: `initialize`, `find` and `touch` are declared `only:
/// :after`; `ActiveModel::Validations::Callbacks` writes `before_validation` and
/// `after_validation` by hand and there is no `around_validation`; and `commit` and `rollback`
/// are not `define_model_callbacks` calls at all — `ActiveRecord::Transactions` writes six
/// `def`s, of which four are the `after_*_commit` shortcuts.
fn callback_names() -> Vec<(String, &'static str)> {
    let mut names = Vec::new();
    for event in ["initialize", "find", "touch"] {
        names.push((format!("after_{event}"), ONLY_AFTER));
    }
    for event in ["save", "create", "update", "destroy"] {
        for prefix in ["before", "around", "after"] {
            names.push((format!("{prefix}_{event}"), EVERY_PREFIX));
        }
    }
    for name in ["before_validation", "after_validation"] {
        names.push((name.to_owned(), VALIDATION));
    }
    for name in [
        "after_commit",
        "after_rollback",
        "after_save_commit",
        "after_create_commit",
        "after_update_commit",
        "after_destroy_commit",
    ] {
        names.push((name.to_owned(), TRANSACTION));
    }
    names
}

/// The callbacks, on the singleton of one model.
///
/// **Nothing here is mapped**, which is the no-place rule and the reason this cannot make a jump
/// worse: `define_model_callbacks` is a `def` in activesupport that no line of the user's code
/// wrote, so there is no span to record and the honest answer to "where was this declared" is
/// nowhere. What it takes away is a *jump into a gem that has nothing to do with the file*.
///
/// `(*untyped)` because a callback takes symbols, a hash of conditions, or neither; `-> void`
/// because nobody chains off one. The block is optional and is handed the **record**, which is
/// `ActiveSupport::Callbacks`' own behaviour for a proc that takes an argument — so
/// `before_save { |story| ... }` types `story`, and a call that writes no block reaches the same
/// arm, because an optional block applies both ways.
///
/// **Declared on the base and inherited**, exactly as [`class_side`] is and for the same
/// reason: a callback registrar is a class method, and one copy on `ApplicationRecord` answers
/// for every model under it. The block parameter is
/// [`Return::Element`](crate::analysis::types::Return::Element), so `before_save { |story| ... }`
/// still types `story` as the model the call was written on rather than as the base.
///
/// An abstract class keeping these is no longer a place this parts company with [`class_side`],
/// because that one is on the abstract class too now — `ApplicationRecord.before_save` is real
/// Ruby, which is what this clause was always about.
fn callbacks(facts: &mut Facts, base: &str) {
    for (name, installed_by) in callback_names() {
        facts.declare(Declared {
            owner: Owner::Singleton(base.to_owned()),
            name,
            returns: "void".to_owned(),
            parameters: format!("(*untyped) ?{{ ({ELEMENT}) -> void }}"),
            because: format!(
                "ActiveRecord's callback, installed on every model by {installed_by}. \
                 ya-lsp writes this; no file declares it."
            ),
            at: None,
            from: Source::Interface,
            overloads: Vec::new(),
        });
    }
}

/// The relation class for a collection of `element`, monomorphised.
///
/// No type parameter anywhere, which is the decision every collection rests on: ya-lsp
/// writes this text, so it never has to write an `ActiveRecord::Relation[Comment]` that finding
/// 33 measured it cannot then instantiate. The price is one class per element type and a name
/// that must not collide, and both are the caller's to hold.
///
/// **Nothing here is mapped.** No line of anybody's code declares `Comment::Relation#first`, so
/// there is no span to record and [`Origin::Unknown`](crate::analysis::synthesized::Origin) is
/// the honest answer: it types a chain and is never a jump target. A relation shared by four
/// `has_many :comments` could be pointed at one of them, and pointing at an arbitrary one of
/// four is the confidently-wrong answer this half of the release exists to avoid.
///
/// One comment for the whole class, because the *class* is what ya-lsp invented: a reader who
/// reaches any member of it has already been told, by its name, that nobody wrote it.
/// [`class_side`] cannot say it that way and does not try.
fn relation(facts: &mut Facts, element: &str) {
    let owner = Owner::Instance(relation_of(element));
    facts.note(
        owner.clone(),
        format!("A collection of `{element}`. ya-lsp writes this class; no file declares it."),
    );
    // And that is the whole of it. Every member is on [`RELATION_BASE`], written once for the
    // project, because the two receiver-relative return types keep the element out of the
    // signatures.
    //
    // A project that declares [`RELATION_BASE`] itself is why this is never reached with a
    // superclass it did not write: `Analysis::model_declarations` withdraws every relation
    // rather than letting one inherit whatever the user meant by the name.
    facts.inherits(owner, RELATION_BASE.to_owned());
}

/// The query interface, written once for the whole project.
///
/// [`RELATION_BASE`] has the argument. Every relation class in the workspace inherits from this
/// one and declares nothing of its own, so the interface costs a project *one* copy however
/// many models it has — which is the difference between 77,128 generated members on discourse
/// and a number that does not grow with the model count.
///
/// **`include Enumerable`**, which is Rails' own line — `ActiveRecord::Relation` includes it —
/// and leaving it out costs measurable down-moves: `User.where(...).select(:id)`
/// answers a relation, and a relation with no `Enumerable` in it is a **dead end** for the
/// `.index_by` or `.map` that habitually follows: the first hop made right and the second made
/// impossible. It goes on the class every relation inherits, for the same reason everything
/// else here does — one `include` for the project.
///
/// The names go on in [`query_interface`]'s order and the ones that are `Persistence`'s alone
/// are skipped — [`Side`] is what says which, and `instantiate` is the only one this body does
/// not get.
pub fn relation_base(facts: &mut Facts) {
    let owner = Owner::Instance(RELATION_BASE.to_owned());
    facts.note(
        owner.clone(),
        "ActiveRecord's query interface, for every relation in the project. ya-lsp writes this \
         class; no file declares it."
            .to_owned(),
    );
    facts.mixin(owner.clone(), "Enumerable".to_owned());
    for query in query_interface() {
        if query.side == Side::Class {
            continue;
        }
        facts.declare(Declared {
            owner: owner.clone(),
            name: query.name.to_owned(),
            returns: query.returns,
            parameters: query.parameters,
            because: String::new(),
            at: None,
            from: Source::Interface,
            overloads: query.overloads,
        });
    }
}

/// The delegated half of the same list on a class object, so a chain can *start*.
///
/// `Story.recent.first.title` works off a macro alone and `Story.first.title` does not, because
/// nothing declares `first`, `where` or `find` on a model itself — the half of the query interface
/// that has no macro to be read from. There is no new convention here and no new reading of
/// anybody's file: [`query_interface`] already knows what each of these returns.
///
/// # It goes on the **base** class, and that is what keeps the declaration count down
///
/// `base` is the topmost class of a model's own superclass chain that the application declares —
/// and then, where the bundle is indexed, `ActiveRecord::Base` itself, which is where Rails really
/// installs the interface. `synthesize::base_of` is the walk. Ruby follows a class object's
/// singleton chain up the class chain, so one copy on the base answers for every model under it,
/// and a project pays the interface and the callbacks once per *base* rather than once per model —
/// an order of magnitude fewer generated members.
///
/// **A base this crate invented would have been simpler and does not work.** A generated
/// `class Story < ActiveRecordModel` is silently ignored wherever the user's own file already
/// writes a superclass, which is every Rails model there is — see [`RELATION_BASE`], where the
/// same mechanism is safe because nothing but this pass declares a relation class. So the
/// inheritance has to be the one the application already wrote.
///
/// # Why inheriting the class side is safe here
///
/// It is inherited, so an interface naming a *concrete* class would answer `Category.order(...)`
/// with a relation of a class no row is an instance of. The two receiver-relative returns are what
/// fix that rather than avoid it: `Category.order` reads
/// [`Return::Collection`](crate::analysis::types::Return::Collection) and answers
/// `Category::Relation`, and `Captain::Assistant.find` answers a `Captain::Assistant`.
///
/// What is left of the trade is stated rather than hidden: **`ApplicationRecord.where` resolves
/// and raises in Ruby**. That is the same kind of wrongness as the callbacks', which are declared
/// on an abstract class deliberately, and it costs a name on a receiver nobody writes — where the
/// alternative is a wrong *type* on receivers everybody writes.
///
/// **Nothing here is mapped**, by the no-place rule: no line of anybody's code declares
/// `Story.where`.
fn class_side(facts: &mut Facts, base: &str) {
    for query in query_interface() {
        let because = match query.side {
            Side::Relation => continue,
            // Deliberately short, and the length is the reason. This sentence is written above
            // **every** class-side declaration, which measures the provenance at 57% of
            // all the generated RBS in the workspace before it was cut. What it may not do is
            // disappear: `class ApplicationRecord` is the user's own class, and a generated
            // `def self.pluck` with nothing above it reads as something their file declared.
            // A relation class carries none because the *class* is what ya-lsp invented, which
            // is an argument this side cannot make.
            Side::Both => {
                "ActiveRecord's query interface, on every model that inherits this.".to_owned()
            }
            Side::Class => "ActiveRecord's `Persistence::ClassMethods`.".to_owned(),
        };
        facts.declare(Declared {
            owner: Owner::Singleton(base.to_owned()),
            name: query.name.to_owned(),
            returns: query.returns,
            parameters: query.parameters,
            because,
            at: None,
            from: Source::Interface,
            overloads: query.overloads,
        });
    }
}

/// The blocks whose statements are still statements of the body around them.
///
/// Two, and each is only a host where it is the Ruby it claims to be. `included do` is
/// `ActiveSupport::Concern`'s and exists only on a **module**: all 147 macro-bearing ones in
/// the six corpora are in a module body, and `included do` written in a `class` is a
/// `NoMethodError` rather than a declaration this reader was missing. `with_options` is
/// ActiveSupport's and is a plain method on `Object`, so it hosts anywhere — 31 of the corpus'
/// 31 are in a class, and the ones inside `included do` are reached through it.
///
/// A third possible host is deliberately absent. `class_methods do`
/// holds **no macro at all** in any of the six corpora — its calls are `private` (19),
/// `delegate` (3) and `attr_reader` (2) — and one written there would be broken Ruby too:
/// `ActiveSupport::Concern` builds a nested `ClassMethods` module and `module_eval`s the block
/// on it, and a plain `Module` has no `has_many`. Nor would it declare on the module's own
/// singleton, which is wrong in
/// the other direction, which `synthesized.md` argues: those methods reach the includer by
/// `extend`, so the module's own singleton is exactly the place they are *not*.
const HOSTS: [(&str, bool); 2] = [("included", true), ("with_options", false)];

/// What one body declared, before it is known whether the body declared anything.
#[derive(Debug, Default)]
struct Macros {
    associations: Vec<Association>,
    enums: Vec<Enum>,
    attributes: Vec<Attribute>,
    delegates: Vec<Delegate>,
    tail: Vec<Tail>,
    exports: Vec<String>,
    helpers: Vec<String>,
    /// Not a macro, and deliberately not part of [`Macros::is_empty`]: a body full of `def`s and
    /// no macro at all is not a model class, and recording one would put every file in the
    /// application on the generated list.
    defined: BTreeSet<(bool, String)>,
}

impl Macros {
    fn is_empty(&self) -> bool {
        self.associations.is_empty()
            && self.enums.is_empty()
            && self.attributes.is_empty()
            && self.delegates.is_empty()
            && self.tail.is_empty()
            && self.exports.is_empty()
            && self.helpers.is_empty()
    }
}

/// The modules one `helper` call names.
///
/// Two spellings and they are `delegates::classify`'s two, asked here about a module rather than
/// about a `to:`: a constant path is the module itself, and a symbol or string is a name Rails
/// camelizes and appends `Helper` to. A list is one call — `helper :application, :email` is
/// discourse's only one — and an argument that is neither is skipped rather than declining the
/// call, because the two in the corpus that are neither sit *beside* names this can read.
fn helper_modules(source: &str, node: &CallNode<'_>) -> Vec<String> {
    let Some(arguments) = node.arguments() else {
        return Vec::new();
    };
    arguments
        .arguments()
        .iter()
        .filter_map(|argument| {
            if argument.as_constant_read_node().is_some()
                || argument.as_constant_path_node().is_some()
            {
                return Some(constant_spelling(source, &argument));
            }
            let (name, _) = symbol_or_string(source, &argument)?;
            super::inflect::helper_module(&name)
        })
        .collect()
}

/// Whether this call is one the body itself is making.
///
/// Receiverless is the ordinary case. The other one is the `t` of `with_options … do |t| … end`:
/// a block that takes the option merger calls the macros on it, and eleven of the corpus'
/// twelve `with_options` blocks take nothing instead, so the receiver that counts is whatever
/// the enclosing block was handed and nothing else.
fn bare(node: &CallNode<'_>, receiver: Option<&str>) -> bool {
    match node.receiver() {
        None => true,
        Some(inner) => receiver.is_some_and(|name| reads_local(&inner, name)),
    }
}

/// The statements of a host block, when this call opens one here.
fn host_body<'pr>(node: &CallNode<'pr>, called: &str, module: bool) -> Option<StatementsNode<'pr>> {
    HOSTS
        .iter()
        .find(|(name, _)| *name == called)
        .filter(|(_, concern)| module || !concern)?;
    node.block()?.as_block_node()?.body()?.as_statements_node()
}

struct Models<'src> {
    source: &'src str,
    nesting: Vec<String>,
    classes: Vec<ModelClass>,
    abstract_classes: BTreeSet<String>,
}

/// Whether this body says its class is **abstract**, in either of the two spellings Rails has.
///
/// `self.abstract_class = true` is the old one and `primary_abstract_class` — a plain
/// receiverless call — is what Rails 7 generates. Both are read because supporting one is a coin
/// flip per repository: of the six corpora, chatwoot, forem and solidus write the first,
/// mastodon writes the second, and lobsters writes the first in the one class the whole of item
/// 37's defect is about.
///
/// The literal is checked rather than assumed. `self.abstract_class = false` is legal Ruby and
/// means the opposite, and a reader that matched on the name alone would take a concrete model's
/// class side away from it.
fn declares_abstract(statements: &StatementsNode<'_>) -> bool {
    statements.body().iter().any(|statement| {
        let Some(call) = statement.as_call_node() else {
            return false;
        };
        match call.receiver() {
            None => call.name().as_slice() == b"primary_abstract_class",
            Some(receiver) => {
                receiver.as_self_node().is_some()
                    && call.name().as_slice() == b"abstract_class="
                    && call.arguments().is_some_and(|arguments| {
                        arguments
                            .arguments()
                            .iter()
                            .next()
                            .is_some_and(|node| node.as_true_node().is_some())
                    })
            }
        }
    })
}

impl Models<'_> {
    /// One body, and then the class and module bodies written as statements of it.
    ///
    /// **Statements, not a walk of the whole tree.** A generic `Visit` descends into every
    /// method body in the file, which on a large one is thousands of frames on a thread whose
    /// stack is Rust's 2 MiB default — measured, as a crash. Recursing on class nesting makes
    /// the depth the depth of `module A; module B; class C`, and it says exactly what the
    /// bounding rule below says: a `class` inside an `if` is not a statement of the body, and
    /// neither is a macro inside a `def`.
    fn walk<'pr>(&mut self, body: Option<Node<'pr>>, module: bool) {
        let Some(statements) = body.and_then(|body| body.as_statements_node()) else {
            return;
        };
        if !self.nesting.is_empty() {
            if !module && declares_abstract(&statements) {
                self.abstract_classes.insert(self.nesting.join("::"));
            }
            let mut found = Macros::default();
            self.collect(&statements, module, &mut Vec::new(), None, &mut found);
            if !found.is_empty() {
                self.classes.push(ModelClass {
                    name: self.nesting.join("::"),
                    module,
                    associations: found.associations,
                    enums: found.enums,
                    attributes: found.attributes,
                    delegates: found.delegates,
                    tail: found.tail,
                    exports: found.exports,
                    helpers: found.helpers,
                    defined: found.defined,
                });
            }
        }
        for statement in statements.body().iter() {
            let (path, inner, nested) = if let Some(class) = statement.as_class_node() {
                (class.constant_path(), class.body(), false)
            } else if let Some(module) = statement.as_module_node() {
                (module.constant_path(), module.body(), true)
            } else {
                continue;
            };
            self.nesting.push(constant_spelling(self.source, &path));
            self.walk(inner, nested);
            self.nesting.pop();
        }
    }

    /// The macros of one body, and of every [`HOSTS`] block written as a statement of it.
    ///
    /// `hosts` is the `with_options` calls this body is inside, innermost last, and `receiver`
    /// is the name a macro may be called *on* and still count. Both are properties of where the
    /// recursion is rather than of the file, which is why they are arguments and not fields.
    ///
    /// The depth is the nesting of these two calls — three at the deepest in the corpus,
    /// `included do > with_options > with_options` — which is the same bound
    /// [`Self::walk`] already accepts for `module A; module B; class C`.
    ///
    /// **An `enum` in a module body is declined, and the corpus' one occurrence is why.**
    /// mastodon's `Status::Visibility` declares `enum :visibility` inside `included do`, and
    /// `statuses.visibility` is an `integer` column. The label is a `String` and the column is
    /// an `Integer`; that pair is resolved by having the schema decline the
    /// column, which it can do because it is told which `(class, attribute)` pairs an `enum`
    /// re-types — and a concern names a module, which claims no table. Declaring it would put
    /// two types for one member in two generated documents with nothing able to see the pair,
    /// which is the defect the withdrawal exists to prevent.
    fn collect<'pr>(
        &self,
        statements: &StatementsNode<'pr>,
        module: bool,
        hosts: &mut Vec<CallNode<'pr>>,
        receiver: Option<&str>,
        into: &mut Macros,
    ) {
        for statement in statements.body().iter() {
            if let Some(written) = statement.as_def_node() {
                into.defined.insert((
                    written.receiver().is_some(),
                    String::from_utf8_lossy(written.name().as_slice()).into_owned(),
                ));
                continue;
            }
            let Some(call) = statement.as_call_node() else {
                continue;
            };
            if !bare(&call, receiver) {
                continue;
            }
            let called = String::from_utf8_lossy(call.name().as_slice()).into_owned();
            if let Some(inner) = host_body(&call, &called, module) {
                let options = called == "with_options";
                let yielded = block_parameter(&call);
                if options {
                    hosts.push(call);
                }
                self.collect(&inner, module, hosts, yielded.as_deref(), into);
                if options {
                    hosts.pop();
                }
            } else if called == "enum" {
                if !module {
                    into.enums.extend(enums::read(self.source, &call));
                }
            } else if called == "attribute" {
                into.attributes.extend(attributes::read(self.source, &call));
            } else if called == "delegate" {
                into.delegates
                    .extend(delegates::read(self.source, &call, hosts));
            } else if called == "helper" {
                // `modules_for_helpers` takes a Module, or a name it camelizes and appends
                // `Helper` to. Anything else — `helper Rails.application.routes.url_helpers`,
                // which forem and solidus both write, and a `helper do … end` block — is a
                // module this reader cannot name, and naming none is the answer.
                into.helpers.extend(helper_modules(self.source, &call));
            } else if called == "helper_method" {
                // Read here rather than in [`tail`] because what it produces is not a member:
                // `helper_method :current_user` declares nothing at all, it says that the
                // `def current_user` this class already writes may be called from a template.
                // A **module** may say it too — 8 of the six corpora's 60 exported names are
                // written in a concern, and `included do` is a statement of this body by item
                // 20's rule, so both spellings arrive here without a case for either.
                into.exports
                    .extend(syntax::positional_names(self.source, &call));
            } else if let Some(read) = tail::read(self.source, &call, &called) {
                into.tail.push(read);
            } else if let Some(association) = self.association(&call, hosts) {
                into.associations.push(association);
            }
        }
    }

    fn association<'pr>(
        &self,
        node: &CallNode<'pr>,
        hosts: &[CallNode<'pr>],
    ) -> Option<Association> {
        let called = String::from_utf8_lossy(node.name().as_slice()).into_owned();
        let (spelled, kind) = ASSOCIATIONS.iter().find(|(name, _)| *name == called)?;
        let (name, name_at) = first_symbol_or_string(self.source, node)?;
        // `polymorphic: true` says the class is decided at run time by a column, so there is no
        // class to name. Emitting nothing is the answer; emitting the association's own
        // camelized name would be a class that does not exist at best and the wrong one at
        // worst.
        if inherited(node, hosts, "polymorphic").is_some_and(|value| value.as_true_node().is_some())
        {
            return None;
        }
        let candidates = match kind {
            // A scope returns a relation of the class it is written in, and its own name says
            // nothing about a type. The lambda's body is never read: that is the declarative
            // rule at its hardest case, and `-> { where(user: Current.user) }` is exactly the
            // Ruby this crate refuses to run.
            Kind::Scope => vec![self.nesting.join("::")],
            _ => self.target(node, hosts, &name, *kind)?,
        };
        Some(Association {
            spelled,
            name,
            candidates,
            kind: *kind,
            optional: inherited(node, hosts, "optional")
                .is_some_and(|value| value.as_true_node().is_some()),
            through: inherited(node, hosts, "through")
                .and_then(|value| Some(symbol_or_string(self.source, &value)?.0)),
            at: header(node)?,
            name_at,
        })
    }

    /// The classes an association could name, innermost first.
    ///
    /// `class_name: "Comment"` wins over everything, which is not a refinement: 32 of the 76
    /// singular associations in the corpus carry one, and `belongs_to :parent_comment` without
    /// it camelizes to a `ParentComment` that no application has ever defined. It does **not**
    /// win over the nesting, and that is Rails rather than a choice here: `compute_type` is
    /// handed the written name and walks it exactly as it walks a derived one, so
    /// `class_name: "Order"` inside `module Spree` is `Spree::Order`. The one spelling that
    /// skips the walk is Rails' own first branch — a leading `::` is an absolute reference.
    fn target<'pr>(
        &self,
        node: &CallNode<'pr>,
        hosts: &[CallNode<'pr>],
        name: &str,
        kind: Kind,
    ) -> Option<Vec<String>> {
        if let Some(written) = inherited(node, hosts, "class_name")
            .and_then(|value| Some(string_literal(self.source, &value)?.0))
        {
            return Some(match written.strip_prefix("::") {
                Some(absolute) => vec![absolute.to_owned()],
                None => self.candidates(&written),
            });
        }
        // `has_many :voters, through: :votes, source: :user` is a collection of `User`, and
        // `source:` is the only thing in the call that says so.
        let source = inherited(node, hosts, "source")
            .and_then(|value| Some(symbol_or_string(self.source, &value)?.0));
        let spelled = source.as_deref().unwrap_or(name);
        let bare = match kind {
            Kind::Many => camelize(&singularize(spelled)),
            _ => camelize(spelled),
        }?;
        Some(self.candidates(&bare))
    }

    /// The same list [`syntax::candidates`] builds, for the body this reader is inside.
    ///
    /// [`syntax::candidates`]: super::syntax::candidates
    fn candidates(&self, name: &str) -> Vec<String> {
        candidates(&self.nesting.join("::"), name)
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    /// What the two view-context macros take out of a body, and what they refuse.
    ///
    /// The only pair of macros here that declare nothing at all: a `helper_method` names a `def`
    /// the class already writes and a `helper` names a module somebody else wrote, so what is
    /// recorded is a permission and a mixin rather than a member. Both spellings of the second
    /// are read because both are in the corpus — 22 of the 26 calls are a symbol and 4 are a
    /// constant, of which forem's two are bare and solidus' two are paths — and the row that
    /// matters is the fifth: `helper` on an *expression* is a
    /// module this reader cannot name, and forem and solidus both write one, so it is skipped
    /// beside the names on the same line rather than declining the call.
    #[test]
    fn what_a_body_hands_to_the_view_context() {
        let source = "\
class ApplicationMailer < ActionMailer::Base
  helper :application
  helper :frontend_urls, AuthenticationHelper, Spree::Admin::OrdersHelper
  helper Rails.application.routes.url_helpers
  helper
  helper_method :current_user, \"logged_in?\"
  helper_method(*EXPORTS)
end

module Authentication
  included do
    helper_method :current_account
  end
end

class Story < ApplicationRecord
  has_many :comments
end
";
        let model = super::read_model(source);
        assert_eq!(
            model.helper_modules().collect::<Vec<_>>(),
            [(
                "ApplicationMailer",
                [
                    "ApplicationHelper".to_owned(),
                    "FrontendUrlsHelper".to_owned(),
                    "AuthenticationHelper".to_owned(),
                    "Spree::Admin::OrdersHelper".to_owned(),
                ]
                .as_slice()
            )]
        );
        assert_eq!(
            model.exports().collect::<Vec<_>>(),
            [
                (
                    "ApplicationMailer",
                    ["current_user".to_owned(), "logged_in?".to_owned()].as_slice()
                ),
                ("Authentication", ["current_account".to_owned()].as_slice()),
            ]
        );
    }

    /// The end of the chain the model gate climbs, and the two spellings an application writes.
    #[test]
    fn what_a_class_says_when_it_says_it_is_abstract() {
        // Both spellings ship for the `enum`'s reason: supporting one is a coin
        // flip per repository. `= false` is read as the `false` it is, because reading it as
        // the name alone takes a concrete model's class side away from it — the wrong-answer
        // direction. A **module** cannot be an ActiveRecord class at all.
        let source = "\
class ApplicationRecord < ActiveRecord::Base
  self.abstract_class = true
end

class Modern < ActiveRecord::Base
  primary_abstract_class
end

class Concrete < ApplicationRecord
  self.abstract_class = false
  self.table_name = \"legacy\"
  has_many :comments
  def spin
  end
end

class Elsewhere < ApplicationRecord
  Other.abstract_class = true
end

module Storyish
  self.abstract_class = true
end
";
        assert_eq!(
            read_model(source).abstract_classes().collect::<Vec<_>>(),
            ["ApplicationRecord", "Modern"]
        );
    }

    #[test]
    fn what_a_model_says_it_inherits() {
        assert!(is_record_base("ApplicationRecord"));
        assert!(is_record_base("ActiveRecord::Base"));
        // A suffix for the first, because `Spree::ApplicationRecord` is one — the same
        // rule for a mailer, asked here about a model.
        assert!(is_record_base("Spree::ApplicationRecord"));
        // And never for the second, because `Base` on its own is somebody else's class: Sinatra
        // and Sidekiq both ship one.
        assert!(!is_record_base("Base"));
        assert!(!is_record_base("Sinatra::Base"));
        assert!(!is_record_base(""));
    }

    use super::*;
    use crate::generated::declaring;

    const MODEL: &str = "\
class Story < ApplicationRecord
  belongs_to :user
  belongs_to :parent_story, class_name: \"Story\", optional: true
  belongs_to :owner, polymorphic: true
  has_one :draft, class_name: \"Comment\"
  has_many :comments
  has_many :taggings
  has_many :tags, through: :taggings
  has_many :voters, through: :votes, source: :user
  scope :recent, -> { order(created_at: :desc) }
end
";

    fn known() -> BTreeSet<String> {
        ["Story", "User", "Comment", "Tag", "Tagging"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn relations() -> BTreeSet<String> {
        ["Comment", "Tag", "Tagging", "Story"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// The whole of what the host test changes: the host, asked one question earlier.
    ///
    /// One source, three bodies, one macro apiece and the same macro. `Story` inherits
    /// `ApplicationRecord` and declares; `StorySerializer` inherits `ActiveModel::Serializer`,
    /// where `has_many` stores an `Attribute` and defines **no method**, and declares nothing;
    /// `Storyish` is a `module`, which passes whatever it inherits because a concern inherits
    /// nothing at all. The three are one fixture rather than three because what is being
    /// asserted is that one rule separates them.
    #[test]
    fn only_a_model_or_a_module_hosts_an_association_macro() {
        let model = read_model(
            "class Story < ApplicationRecord\n  has_many :comments\nend\n\
             class StorySerializer < ActiveModel::Serializer\n  has_many :comments\nend\n\
             module Storyish\n  has_many :comments\nend\n",
        );
        let known: BTreeSet<String> = ["Story", "StorySerializer", "Storyish", "Comment"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let relations: BTreeSet<String> = ["Comment"].into_iter().map(str::to_owned).collect();
        let rbs = model
            .signatures(
                "app/models/story.rb",
                &Elsewhere {
                    known: &known,
                    // The serializer is deliberately absent and `Storyish` deliberately too:
                    // a module must not need to be here.
                    models: &["Story"].into_iter().map(str::to_owned).collect(),
                    relations: &relations,
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]))
            .rbs;
        assert!(rbs.contains("class Story\n"), "the model declares: {rbs}");
        assert!(
            rbs.contains("module Storyish\n"),
            "and so does the concern: {rbs}"
        );
        assert!(
            !rbs.contains("StorySerializer"),
            "and the serializer says nothing at all: {rbs}"
        );
    }

    /// The half that takes the host gate's cost from seven declarations to nothing.
    ///
    /// `Tag`'s base class is in a gem's `lib/`, so the walk `Context::models` does cannot reach
    /// it and it is not a model by inheritance. It is one anyway, because some model in the
    /// application says `has_many :tags` — which is what puts it in the union this asks. Both
    /// of forem's two gem-rooted models are exactly this shape.
    #[test]
    fn a_model_whose_base_class_is_in_a_gem_is_still_a_host() {
        let model = read_model("class Tag < ActsAsTaggableOn::Tag\n  has_many :taggings\nend\n");
        let known: BTreeSet<String> = ["Tag", "Tagging"].into_iter().map(str::to_owned).collect();
        let relations: BTreeSet<String> = ["Tagging"].into_iter().map(str::to_owned).collect();
        let rbs = model
            .signatures(
                "app/models/tag.rb",
                &Elsewhere {
                    known: &known,
                    // Not `ActsAsTaggableOn::Tag`-rooted and so not from the chain — from the
                    // collection half, which is the only reason `Tag` is here.
                    models: &["Tag"].into_iter().map(str::to_owned).collect(),
                    relations: &relations,
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]))
            .rbs;
        assert!(
            rbs.contains("def taggings: () -> Tagging::Relation"),
            "the gem-rooted model still declares: {rbs}"
        );
    }

    /// Why this is an admit list rather than a list of serializer names.
    ///
    /// Solidus' `Spree::Admin::ResourceController` defines its own class-side `belongs_to` for
    /// nested-resource routing and defines no method. The symbol spelling is deliberate: the
    /// thirteen real calls are written `belongs_to "spree/product"`, which are
    /// declined today by an unrelated accident of [`camelize`], so a fixture written that way
    /// would pass with the gate removed.
    #[test]
    fn a_controller_that_spells_an_association_macro_declares_nothing() {
        let model = read_model(
            "class Spree::Admin::ProductsController < Spree::Admin::ResourceController\n  \
             belongs_to :product\nend\n",
        );
        let known: BTreeSet<String> = [
            "Spree::Admin::ProductsController",
            "Spree::Product",
            "Product",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let rbs = model
            .signatures(
                "app/controllers/spree/admin/products_controller.rb",
                &Elsewhere {
                    known: &known,
                    models: &["Spree::Product"].into_iter().map(str::to_owned).collect(),
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]))
            .rbs;
        assert_eq!(rbs, "", "a controller is not a macro host: {rbs}");
    }

    #[test]
    fn the_rbs_a_model_declares() {
        // Pinned whole, for the reason the schema's is: every rule in this half shows up in the
        // text, and asserting them one predicate at a time is how a change to the shape passes
        // ten green tests.
        let model = read_model(MODEL);
        let declarations = model
            .signatures(
                "app/models/story.rb",
                &Elsewhere {
                    known: &known(),
                    models: &known(),
                    relations: &relations(),
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]));
        assert_eq!(
            declarations.rbs,
            "\
class Story
  # From `app/models/story.rb`, `belongs_to :user`, which is a `User`.
  def user: () -> User
  # From `app/models/story.rb`, `belongs_to :user`, which also installs `user=`.
  def user=: (User?) -> User?
  # From `app/models/story.rb`, `belongs_to :user`, which also installs `build_user`.
  def build_user: (*untyped) ?{ (User) -> untyped } -> User
  # From `app/models/story.rb`, `belongs_to :user`, which also installs `create_user`.
  def create_user: (*untyped) ?{ (User) -> untyped } -> User
  # From `app/models/story.rb`, `belongs_to :user`, which also installs `create_user!`.
  def create_user!: (*untyped) ?{ (User) -> untyped } -> User
  # From `app/models/story.rb`, `belongs_to :parent_story`, which is a `Story`.
  def parent_story: () -> Story?
  # From `app/models/story.rb`, `belongs_to :parent_story`, which also installs `parent_story=`.
  def parent_story=: (Story?) -> Story?
  # From `app/models/story.rb`, `belongs_to :parent_story`, which also installs `build_parent_story`.
  def build_parent_story: (*untyped) ?{ (Story) -> untyped } -> Story
  # From `app/models/story.rb`, `belongs_to :parent_story`, which also installs `create_parent_story`.
  def create_parent_story: (*untyped) ?{ (Story) -> untyped } -> Story
  # From `app/models/story.rb`, `belongs_to :parent_story`, which also installs `create_parent_story!`.
  def create_parent_story!: (*untyped) ?{ (Story) -> untyped } -> Story
  # From `app/models/story.rb`, `has_one :draft`, which is a `Comment`.
  def draft: () -> Comment?
  # From `app/models/story.rb`, `has_one :draft`, which also installs `draft=`.
  def draft=: (Comment?) -> Comment?
  # From `app/models/story.rb`, `has_one :draft`, which also installs `build_draft`.
  def build_draft: (*untyped) ?{ (Comment) -> untyped } -> Comment
  # From `app/models/story.rb`, `has_one :draft`, which also installs `create_draft`.
  def create_draft: (*untyped) ?{ (Comment) -> untyped } -> Comment
  # From `app/models/story.rb`, `has_one :draft`, which also installs `create_draft!`.
  def create_draft!: (*untyped) ?{ (Comment) -> untyped } -> Comment
  # From `app/models/story.rb`, `has_many :comments`, which is a `Comment`.
  def comments: () -> Comment::Relation
  # From `app/models/story.rb`, `has_many :comments`, which also installs `comments=`.
  def comments=: (untyped) -> untyped
  # From `app/models/story.rb`, `has_many :comments`, which also installs `comment_ids`.
  def comment_ids: () -> Array[untyped]
  # From `app/models/story.rb`, `has_many :comments`, which also installs `comment_ids=`.
  def comment_ids=: (untyped) -> untyped
  # From `app/models/story.rb`, `has_many :taggings`, which is a `Tagging`.
  def taggings: () -> Tagging::Relation
  # From `app/models/story.rb`, `has_many :taggings`, which also installs `taggings=`.
  def taggings=: (untyped) -> untyped
  # From `app/models/story.rb`, `has_many :taggings`, which also installs `tagging_ids`.
  def tagging_ids: () -> Array[untyped]
  # From `app/models/story.rb`, `has_many :taggings`, which also installs `tagging_ids=`.
  def tagging_ids=: (untyped) -> untyped
  # From `app/models/story.rb`, `has_many :tags`, which is a `Tag`.
  def tags: () -> Tag::Relation
  # From `app/models/story.rb`, `has_many :tags`, which also installs `tags=`.
  def tags=: (untyped) -> untyped
  # From `app/models/story.rb`, `has_many :tags`, which also installs `tag_ids`.
  def tag_ids: () -> Array[untyped]
  # From `app/models/story.rb`, `has_many :tags`, which also installs `tag_ids=`.
  def tag_ids=: (untyped) -> untyped
  # From `app/models/story.rb`, `scope :recent`.
  def self.recent: (*untyped) -> Story::Relation
end
"
        );
        assert_eq!(declarations.classes, 1);
        // Seven readers; a writer and three constructors for each of the three **singular**
        // associations whose class this project defines — `owner` is polymorphic and `ghost`
        // names nothing, so neither declares anything at all — and a writer and the `_ids` pair
        // for each of the three collections, whose constructors are the relation's.
        assert_eq!(declarations.spans.len(), 7 + 3 * 4 + 3 * 3);
    }

    #[test]
    fn every_macro_points_at_the_line_that_declared_it() {
        let model = read_model(MODEL);
        let declarations = model
            .signatures(
                "app/models/story.rb",
                &Elsewhere {
                    known: &known(),
                    models: &known(),
                    relations: &relations(),
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]));
        let at = |span: &crate::generated::Span| {
            (
                &MODEL[span.declared.0 as usize..span.declared.1 as usize],
                &MODEL[span.selection.0 as usize..span.selection.1 as usize],
            )
        };
        assert_eq!(at(&declarations.spans[0]), ("belongs_to :user", "user"));
        // The writer and the three constructors that macro also installs point at the same
        // line, because it is the line Rails writes them from — see `Association::declare`.
        for span in &declarations.spans[1..5] {
            assert_eq!(at(span), ("belongs_to :user", "user"));
        }
        assert_eq!(
            at(&declarations.spans[5]),
            (
                "belongs_to :parent_story, class_name: \"Story\", optional: true",
                "parent_story"
            )
        );
        // And a collection's three do the same: `has_many :comments` is one line and
        // `comments`, `comments=`, `comment_ids` and `comment_ids=` are four members of it.
        for span in &declarations.spans[15..19] {
            assert_eq!(at(span), ("has_many :comments", "comments"));
        }
        assert_eq!(
            at(declarations.spans.last().expect("a span")),
            ("scope :recent, -> { order(created_at: :desc) }", "recent")
        );
    }

    /// The macro every corpus lints away, and Rails' own last
    /// line of it — `has_many name, scope, **hm_options, &extension`.
    ///
    /// So it is one row in `ASSOCIATIONS` and *no* new code path: the element type is
    /// singularized by the same function, `class_name:` is read by the same one, and the
    /// relation class is the ordinary one. What it does need is the macro's own spelling, because a
    /// provenance line that said `has_many :tags` would be naming a macro the file does not
    /// contain.
    #[test]
    fn has_and_belongs_to_many_is_a_collection_and_says_which_macro_said_so() {
        let source = "\
class Story < ApplicationRecord
  has_and_belongs_to_many :tags
  has_and_belongs_to_many :people
  has_and_belongs_to_many :editors, class_name: \"User\"
end
";
        let declarations = read_model(source)
            .signatures(
                "app/models/story.rb",
                &Elsewhere {
                    known: &["Tag", "Person", "User"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    // `Story` is the host and not a target, so it is here and not in `known`:
                    // The host test asks whether the class the macro is written on is a model.
                    models: &["Story", "Tag", "Person", "User"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    relations: &["Tag", "Person", "User"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]));
        assert_eq!(
            declarations.rbs,
            "\
class Story
  # From `app/models/story.rb`, `has_and_belongs_to_many :tags`, which is a `Tag`.
  def tags: () -> Tag::Relation
  # From `app/models/story.rb`, `has_and_belongs_to_many :tags`, which also installs `tags=`.
  def tags=: (untyped) -> untyped
  # From `app/models/story.rb`, `has_and_belongs_to_many :tags`, which also installs `tag_ids`.
  def tag_ids: () -> Array[untyped]
  # From `app/models/story.rb`, `has_and_belongs_to_many :tags`, which also installs `tag_ids=`.
  def tag_ids=: (untyped) -> untyped
  # From `app/models/story.rb`, `has_and_belongs_to_many :people`, which is a `Person`.
  def people: () -> Person::Relation
  # From `app/models/story.rb`, `has_and_belongs_to_many :people`, which also installs `people=`.
  def people=: (untyped) -> untyped
  # From `app/models/story.rb`, `has_and_belongs_to_many :people`, which also installs `person_ids`.
  def person_ids: () -> Array[untyped]
  # From `app/models/story.rb`, `has_and_belongs_to_many :people`, which also installs `person_ids=`.
  def person_ids=: (untyped) -> untyped
  # From `app/models/story.rb`, `has_and_belongs_to_many :editors`, which is a `User`.
  def editors: () -> User::Relation
  # From `app/models/story.rb`, `has_and_belongs_to_many :editors`, which also installs `editors=`.
  def editors=: (untyped) -> untyped
  # From `app/models/story.rb`, `has_and_belongs_to_many :editors`, which also installs `editor_ids`.
  def editor_ids: () -> Array[untyped]
  # From `app/models/story.rb`, `has_and_belongs_to_many :editors`, which also installs `editor_ids=`.
  def editor_ids=: (untyped) -> untyped
end
"
        );
        // And the jump lands on the macro, exactly as a `has_many`'s does.
        let at = |span: (u32, u32)| &source[span.0 as usize..span.1 as usize];
        assert_eq!(
            (
                at(declarations.spans[0].declared),
                at(declarations.spans[0].selection)
            ),
            ("has_and_belongs_to_many :tags", "tags")
        );
    }

    #[test]
    fn what_a_macro_declares_nothing_about() {
        let model = read_model(MODEL);
        let rbs = model
            .signatures(
                "app/models/story.rb",
                &Elsewhere {
                    known: &known(),
                    models: &known(),
                    relations: &relations(),
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]))
            .rbs;
        // `polymorphic:` names a class only a column knows at run time.
        assert!(!rbs.contains("def owner"), "{rbs}");
        // `through: :votes` names an association this class does not declare.
        assert!(!rbs.contains("def voters"), "{rbs}");

        // A collection whose element type has no relation class keeps its `belongs_to`s and
        // loses every collection, which is the decline being local to the macro that needed it.
        let rbs = model
            .signatures(
                "app/models/story.rb",
                &Elsewhere {
                    known: &known(),
                    models: &known(),
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]))
            .rbs;
        assert!(rbs.contains("def user"), "{rbs}");
        assert!(!rbs.contains("def comments"), "{rbs}");
        assert!(!rbs.contains("def self.recent"), "{rbs}");

        // And a file naming nothing the application defines opens no class at all.
        let rbs = model
            .signatures(
                "app/models/story.rb",
                &Elsewhere {
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]))
            .rbs;
        assert!(
            rbs.is_empty(),
            "a class with nothing to say opens nothing: {rbs}"
        );
    }

    #[test]
    fn what_a_model_says_it_needs_before_anything_is_written() {
        // The two questions the caller asks every file before letting any of them write: which
        // classes are named, and which of them need a relation. `Story` is in the second list
        // because of its `scope`, which is the one that is easy to miss.
        //
        // The first is a list per macro: every association here asks for
        // `Story::Thing` before the bare `Thing`, because Rails resolves an association's class
        // against the nesting of the class the macro is written on and a top-level class is a
        // nesting of one. The `scope` is the exception with no walk at all — a relation of the
        // class it is written on is a name and not a guess.
        let model = read_model(MODEL);
        assert_eq!(
            model.targets().collect::<Vec<_>>(),
            [
                "Story::User",
                "User",
                "Story::Story",
                "Story",
                "Story::Comment",
                "Comment",
                "Story::Comment",
                "Comment",
                "Story::Tagging",
                "Tagging",
                "Story::Tag",
                "Tag",
                "Story::User",
                "User",
                "Story",
            ]
        );
        assert_eq!(
            model.collections(&known()).collect::<Vec<_>>(),
            ["Comment", "Tagging", "Tag", "User", "Story"]
        );
    }

    /// The fixture is the point: **both** spellings exist, so a reader that took
    /// the bare name would pass a test where only one did.
    ///
    /// `Spree::LineItem` naming `Adjustment` is `Spree::Adjustment` and never the top-level
    /// `Adjustment`, which is `ActiveRecord::Inheritance#compute_type`'s order rather than a
    /// preference. The `has_many` says the same thing about the relation: `collections` and
    /// `signatures` have to agree about which class was named, or the member is typed as a
    /// relation of a class it does not hold.
    #[test]
    fn an_association_resolves_against_the_nesting_of_the_class_it_is_written_on() {
        let model = read_model(
            "\
module Spree
  class LineItem < ApplicationRecord
    belongs_to :adjustment
    has_many :orders
  end
end
",
        );
        let known: BTreeSet<String> = [
            "Spree",
            "Spree::LineItem",
            "Spree::Adjustment",
            "Adjustment",
            "Spree::Order",
            "Order",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert_eq!(
            model.collections(&known).collect::<Vec<_>>(),
            ["Spree::Order"],
            "the relation asked for is one of the class that will be declared"
        );
        let relations: BTreeSet<String> = ["Spree::Order", "Order"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let rbs = model
            .signatures(
                "app/models/spree/line_item.rb",
                &Elsewhere {
                    known: &known,
                    models: &known,
                    relations: &relations,
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&["Spree"]))
            .rbs;
        assert!(
            rbs.contains("def adjustment: () -> Spree::Adjustment\n"),
            "{rbs}"
        );
        assert!(
            rbs.contains("def orders: () -> Spree::Order::Relation\n"),
            "{rbs}"
        );
    }

    /// A `class_name:` is walked exactly as a derived name is, and `::` is the one escape.
    ///
    /// Rails hands `compute_type` whichever name it has and the walk is inside it, so
    /// `class_name: "Order"` inside `module Spree` is `Spree::Order`. A leading `::` takes
    /// `compute_type`'s own first branch — an absolute reference, constantized with no
    /// candidates at all — which is why the two lines below answer differently.
    #[test]
    fn a_written_class_name_is_nested_too_and_a_leading_colon_pair_is_absolute() {
        let model = read_model(
            "\
module Spree
  class LineItem < ApplicationRecord
    belongs_to :nested, class_name: \"Order\"
    belongs_to :absolute, class_name: \"::Order\"
  end
end
",
        );
        let known: BTreeSet<String> = ["Spree", "Spree::LineItem", "Spree::Order", "Order"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let rbs = model
            .signatures(
                "app/models/spree/line_item.rb",
                &Elsewhere {
                    known: &known,
                    models: &known,
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&["Spree"]))
            .rbs;
        assert!(rbs.contains("def nested: () -> Spree::Order\n"), "{rbs}");
        assert!(rbs.contains("def absolute: () -> Order\n"), "{rbs}");
    }

    /// `belongs_to` is not only ActiveRecord's, and a prefix must not rescue a name.
    ///
    /// solidus' admin controllers write `belongs_to "spree/order"` ten times from
    /// `Spree::Admin::ResourceController`, and it is not the macro this reader reads. It
    /// declines because `Spree/order` is not a constant, and the nesting walk must not turn that
    /// into an answer: a prefix in front of a name that is not a constant leaves a name that is
    /// still not one, at every depth.
    ///
    /// The second macro is the other half of that, and it declines one step earlier: a name
    /// [`camelize`] cannot make a constant of at all never reaches the candidate list, so there
    /// is nothing to put a prefix on.
    #[test]
    fn a_name_that_is_not_a_constant_is_still_nothing_with_every_prefix() {
        let model = read_model(
            "\
module Spree
  module Admin
    class ResourceController < ApplicationController
      belongs_to \"spree/order\"
      belongs_to :\"1st_choice\"
    end
  end
end
",
        );
        assert_eq!(
            model.targets().collect::<Vec<_>>(),
            [
                "Spree::Admin::ResourceController::Spree/order",
                "Spree::Admin::Spree/order",
                "Spree::Spree/order",
                "Spree/order",
            ],
            "every candidate of the one macro that has any, and not one is a constant"
        );
        let known: BTreeSet<String> = [
            "Spree",
            "Spree::Admin",
            "Spree::Admin::ResourceController",
            "Spree::Order",
            "Order",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let rbs = model
            .signatures(
                "app/controllers/spree/admin/resource_controller.rb",
                &Elsewhere {
                    known: &known,
                    models: &known,
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&["Spree", "Spree::Admin"]))
            .rbs;
        assert!(rbs.is_empty(), "{rbs}");
    }

    /// [`relation_of`] and [`element_of`] are one mapping, and it has to be invertible.
    ///
    /// One copy for the project turns on it: the interface is declared once, so the only
    /// thing that says which model an answer is about is the receiver's **name**. A name that
    /// is not a relation answers `None` rather than itself, because the caller's next question
    /// would build a class out of it.
    #[test]
    fn a_relation_names_its_element_and_nothing_else_does() {
        for element in ["Story", "Spree::Order", "A::B::C"] {
            assert_eq!(element_of(&relation_of(element)), Some(element));
        }
        // Everything that is not one: a bare model, the last segment on its own, a name that
        // merely ends in the letters, and nothing at all.
        for other in ["Story", "Relation", "StoryRelation", ""] {
            assert_eq!(element_of(other), None, "{other}");
        }
    }

    /// One copy for the project, asked of the table rather than of a document.
    ///
    /// **No signature in the interface names a concrete class of the application's**, which is
    /// the whole mechanism: where a signature would name `Story` it names [`ELEMENT`], and where
    /// it would name `Story::Relation` it names [`COLLECTION`], so the list is written once for
    /// the project. Checked as a property of every row rather than of the four it was built from —
    /// a row added tomorrow that spells an element is a row that would have to be per model
    /// again, and this is what says so.
    #[test]
    fn no_signature_in_the_interface_names_what_the_collection_holds() {
        let queries = query_interface();
        let named = |want: &str| {
            queries
                .iter()
                .find(|query| query.name == want)
                .unwrap_or_else(|| panic!("{want}"))
        };
        // The three places an element can be named, each real and each receiver-relative now.
        assert_eq!(named("first").returns, format!("{ELEMENT}?"));
        assert_eq!(
            named("select").overloads,
            vec![(
                format!("() {{ ({ELEMENT}) -> untyped }}"),
                format!("Array[{ELEMENT}]")
            )]
        );
        assert_eq!(
            named("any?").parameters,
            format!("(*untyped) ?{{ ({ELEMENT}) -> untyped }}")
        );
        // And the relation, which is not `self`: on a class object `where` returns the
        // relation, which is a different type from the receiver.
        assert_eq!(named("where").returns, COLLECTION);

        for query in &queries {
            for text in [&query.parameters, &query.returns]
                .into_iter()
                .chain(query.overloads.iter().flat_map(|(p, r)| [p, r]))
            {
                assert!(
                    !text.contains("Story") && !text.contains("::Relation"),
                    "`{}` spells an element in {text:?}, which would put it back on every model",
                    query.name
                );
            }
        }
    }

    #[test]
    fn a_relation_class_is_written_where_the_caller_says_and_maps_to_nothing() {
        let model = read_model(MODEL);
        let emit: BTreeSet<String> = ["Comment"].into_iter().map(str::to_owned).collect();
        let declarations = model
            .signatures(
                "app/models/story.rb",
                &Elsewhere {
                    known: &known(),
                    models: &known(),
                    relations: &relations(),
                    emit: &emit,
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]));
        // A relation class is a superclass line and a note, and
        // every member of it is on `RELATION_BASE`, written once for the project.
        assert!(
            declarations.rbs.contains(&format!(
                "class Comment::Relation < {RELATION_BASE}\n  # A collection of `Comment`."
            )),
            "{}",
            declarations.rbs
        );
        // What this file's own macros declared, and not one more: the class side and the
        // callbacks are on the base, which this document was not asked to write. Seven readers,
        // four more for each of the three singular associations that name a class, and three
        // more for each of the three collections.
        assert_eq!(declarations.methods, 7 + 3 * 4 + 3 * 3);
        assert_eq!(declarations.spans.len(), 7 + 3 * 4 + 3 * 3);
        assert_eq!(declarations.classes, 2);
    }

    /// The base class the whole project's relations inherit.
    #[test]
    fn the_query_interface_is_written_once_and_names_no_model() {
        let mut facts = Facts::default();
        relation_base(&mut facts);
        let rbs = facts.render(&declaring(&[])).rbs;
        assert!(
            rbs.starts_with(&format!(
                "class {RELATION_BASE}\n  # ActiveRecord's query interface"
            )),
            "{rbs}"
        );
        // Rails' own line, and leaving it out costs down-moves: a relation with no `Enumerable`
        // is a dead end for the `.index_by` or `.map` that follows a `select`.
        assert!(rbs.contains("\n  include Enumerable\n"), "{rbs}");
        // The overload set, rendered on one line because a `Span` is a byte range: the
        // count decides the arm, so `first` is a record and `first(3)` is an array of them.
        assert!(
            rbs.contains(&format!(
                "def first: () -> {ELEMENT}? | (Integer) -> Array[{ELEMENT}]\n"
            )),
            "{rbs}"
        );
        assert!(
            rbs.contains(&format!(
                "def each: () {{ ({ELEMENT}) -> void }} -> {COLLECTION}\n"
            )),
            "{rbs}"
        );
        // `Persistence`'s own is the one name a relation does not answer.
        assert!(!rbs.contains("def instantiate:"), "{rbs}");
        // Nothing here is a place: no line of anybody's code declares any of it.
        assert!(facts.render(&declaring(&[])).spans.is_empty());
    }

    #[test]
    fn the_callback_names_are_rails_four_call_sites_and_not_a_product_of_three_by_ten() {
        // The count matters more than the spelling here, because the spelling is the part a
        // reader can check against Rails and the count is the part a wrong reading
        // silently changes. A `before`/`around`/`after` × ten product rule gives thirty from
        // ten events; Rails installs **twenty-three**, and every one of the seven missing is a
        // name Ruby raises on.
        let names: Vec<String> = callback_names().into_iter().map(|(name, _)| name).collect();
        assert_eq!(names.len(), 23);
        assert_eq!(
            names.iter().collect::<BTreeSet<_>>().len(),
            23,
            "and no name is installed twice"
        );
        for missing in [
            "before_initialize",
            "before_find",
            "before_touch",
            "around_validation",
            "before_commit",
            "around_commit",
            "before_rollback",
        ] {
            assert!(
                !names.iter().any(|name| name == missing),
                "{missing} is the product rule's invention and not a method ActiveRecord defines"
            );
        }
        // Exactly the four groups, counted where each is decided: `only: :after` is three,
        // every prefix on four events is twelve, validation is two by hand, and the
        // transactional pair plus its four shortcuts is six.
        assert_eq!(
            (
                names
                    .iter()
                    .filter(|name| name.starts_with("before_"))
                    .count(),
                names
                    .iter()
                    .filter(|name| name.starts_with("around_"))
                    .count(),
                names
                    .iter()
                    .filter(|name| name.ends_with("_commit"))
                    .count(),
            ),
            (5, 4, 5)
        );
    }

    #[test]
    fn the_class_side_says_what_the_relation_says_and_maps_to_nothing_either() {
        // The two halves are written from one list, so the test that matters is not
        // that the names are present but that both sides agree about every one they share:
        // `Story.where` and `Story.all.where` are one method reached two ways, and a table that
        // let them drift would type a chain differently depending on where it started.
        //
        // The list holds a second thing — which side each name is on — and this is where it is checked
        // both ways round: a name `QUERYING_METHODS` does not delegate must be on the relation
        // and must **not** be on the class side, because `Story.each` raises in Ruby.
        let model = read_model(MODEL);
        let bases: BTreeSet<String> = ["ApplicationRecord"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let class_side = model
            .signatures(
                "app/models/story.rb",
                &Elsewhere {
                    known: &known(),
                    models: &known(),
                    relations: &relations(),
                    bases: &bases,
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]))
            .rbs;
        let mut interface = Facts::default();
        relation_base(&mut interface);
        let relation = interface.render(&declaring(&[])).rbs;

        let (mut on_both, mut relation_only, mut class_only) = (0, 0, 0);
        for query in query_interface() {
            let mut signature =
                format!("{}: {} -> {}", query.name, query.parameters, query.returns);
            for (parameters, returns) in &query.overloads {
                signature.push_str(&format!(" | {parameters} -> {returns}"));
            }
            let on_relation = relation.contains(&format!("  def {signature}\n"));
            let on_class = class_side.contains(&format!("  def self.{signature}\n"));
            assert_eq!(
                (on_relation, on_class),
                match query.side {
                    Side::Both => (true, true),
                    Side::Relation => (true, false),
                    Side::Class => (false, true),
                },
                "{signature} is on the wrong side: relation={on_relation} class={on_class}"
            );
            match query.side {
                Side::Both => on_both += 1,
                Side::Relation => relation_only += 1,
                Side::Class => class_only += 1,
            }
        }
        // A tripwire on the bound rather than on the table: 113 is
        // `ActiveRecord::Querying::QUERYING_METHODS` counted, plus `Querying#with`, which is a
        // `def` beside the constant, plus the five `Persistence::ClassMethods` names that
        // `relation.rb` defines too. A name added here without a line of Rails behind it moves
        // this number and has to say which file it read.
        assert_eq!(
            on_both, 119,
            "QUERYING_METHODS, `with`, and `relation.rb`'s five"
        );
        assert_eq!(
            relation_only, 7,
            "`Relation`'s own — the five that raise on the model, `new`, which `relation.rb` aliases `build` to, \
             and `reload`"
        );
        assert_eq!(
            class_only, 1,
            "`instantiate`, which `Relation` does not define"
        );
        // Every class-side declaration carries its own provenance, because `class
        // ApplicationRecord` is the user's own class and a note attached to *it* would read as
        // a claim about their file. The relation base carries one note for the whole class,
        // which is an argument this side cannot make.
        assert_eq!(
            class_side
                .matches("ActiveRecord's query interface, on every model that inherits this.")
                .count(),
            119,
            "{class_side}"
        );
        assert_eq!(
            class_side
                .matches("ActiveRecord's `Persistence::ClassMethods`")
                .count(),
            1,
            "{class_side}"
        );
        assert!(
            !class_side.contains("ya-lsp writes this class"),
            "the class is not generated, only these members are: {class_side}"
        );
        // The base and never the model: `Story.where` is inherited, which is what makes one
        // copy answer for every model in the application. This file writes `class Story` and
        // its macros name `Comment`, and the interface is on neither of them — one copy, in
        // the body of the base.
        let (before, after) = class_side
            .split_once("class ApplicationRecord\n")
            .expect("the base's body");
        assert!(!before.contains("def self.where"), "{class_side}");
        assert!(after.contains("  def self.where:"), "{class_side}");
        assert_eq!(
            class_side.matches("def self.where:").count(),
            1,
            "{class_side}"
        );
    }

    #[test]
    fn a_macro_that_is_not_a_statement_of_a_class_body_is_not_a_macro() {
        // The bounding rule, one case per way of hiding a call from it. What is declined is
        // every block that is not one of the two hosts, and `included do` written where it is
        // not Ruby that runs.
        for source in [
            // `included do` is `ActiveSupport::Concern`'s and exists on a module. In a class
            // body it raises, and 0 of the corpus' 147 are written in one.
            "class Story\n  included do\n    has_many :tags\n  end\nend\n",
            "class Story\n  def setup\n    has_many :tags\n  end\nend\n",
            "class Story\n  if admin?\n    has_many :tags\n  end\nend\n",
            // Not a host, and the corpus' `class_methods do` blocks hold no macro at all.
            "class Story\n  class_methods do\n    has_many :tags\n  end\nend\n",
            "module Storyish\n  class_methods do\n    has_many :tags\n  end\nend\n",
            // Any other block, including one whose parameter is named like a host's.
            "class Story\n  transaction do |s|\n    s.has_many :tags\n  end\nend\n",
            // A receiver that is not the block's own parameter, inside a host that has one.
            "class Story\n  with_options dependent: :destroy do |s|\n    other.has_many :tags\n  end\nend\n",
            // …and a host with no parameter at all does not make a receiver mean nothing.
            "class Story\n  with_options dependent: :destroy do\n    s.has_many :tags\n  end\nend\n",
            "has_many :tags\n",
            "class Story\n  self.has_many :tags\nend\n",
            "class Story\n  has_many\nend\n",
            "class Story\n  has_many name\nend\n",
            "class Story\nend\n",
            "class Story; end\n",
        ] {
            let model = read_model(source);
            assert_eq!(
                model.targets().count(),
                0,
                "{source:?} declared something it should not have"
            );
        }
    }

    /// A concern, whole: its macros, on the module, with `with_options` merged in.
    ///
    /// The RBS is pinned rather than probed for the reason the model's and the schema's are:
    /// every rule shows up in this text, and `module` rather than `class` on the first line is
    /// the mechanism. What is deliberately *not* here is the `scope` and the
    /// `enum`, which are the two the module cannot own.
    #[test]
    fn the_rbs_a_concern_declares() {
        const CONCERN: &str = "\
module Storyish
  extend ActiveSupport::Concern

  included do
    has_many :comments
    scope :recent, -> { order(created_at: :desc) }
    enum :status, { draft: 0 }

    with_options class_name: \"Comment\" do
      belongs_to :first_note, optional: true
      has_many :notes
    end
  end
end
";
        let model = read_model(CONCERN);
        let declarations = model
            .signatures(
                "app/models/concerns/storyish.rb",
                &Elsewhere {
                    known: &known(),
                    models: &known(),
                    relations: &relations(),
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]));
        assert_eq!(
            declarations.rbs,
            "\
module Storyish
  # From `app/models/concerns/storyish.rb`, `has_many :comments`, which is a `Comment`.
  def comments: () -> Comment::Relation
  # From `app/models/concerns/storyish.rb`, `has_many :comments`, which also installs `comments=`.
  def comments=: (untyped) -> untyped
  # From `app/models/concerns/storyish.rb`, `has_many :comments`, which also installs `comment_ids`.
  def comment_ids: () -> Array[untyped]
  # From `app/models/concerns/storyish.rb`, `has_many :comments`, which also installs `comment_ids=`.
  def comment_ids=: (untyped) -> untyped
  # From `app/models/concerns/storyish.rb`, `belongs_to :first_note`, which is a `Comment`.
  def first_note: () -> Comment?
  # From `app/models/concerns/storyish.rb`, `belongs_to :first_note`, which also installs `first_note=`.
  def first_note=: (Comment?) -> Comment?
  # From `app/models/concerns/storyish.rb`, `belongs_to :first_note`, which also installs `build_first_note`.
  def build_first_note: (*untyped) ?{ (Comment) -> untyped } -> Comment
  # From `app/models/concerns/storyish.rb`, `belongs_to :first_note`, which also installs `create_first_note`.
  def create_first_note: (*untyped) ?{ (Comment) -> untyped } -> Comment
  # From `app/models/concerns/storyish.rb`, `belongs_to :first_note`, which also installs `create_first_note!`.
  def create_first_note!: (*untyped) ?{ (Comment) -> untyped } -> Comment
  # From `app/models/concerns/storyish.rb`, `has_many :notes`, which is a `Comment`.
  def notes: () -> Comment::Relation
  # From `app/models/concerns/storyish.rb`, `has_many :notes`, which also installs `notes=`.
  def notes=: (untyped) -> untyped
  # From `app/models/concerns/storyish.rb`, `has_many :notes`, which also installs `note_ids`.
  def note_ids: () -> Array[untyped]
  # From `app/models/concerns/storyish.rb`, `has_many :notes`, which also installs `note_ids=`.
  def note_ids=: (untyped) -> untyped
end
",
            "{}",
            declarations.rbs
        );
        // The two the module may not own, and neither is a relation it may ask for.
        assert!(!declarations.rbs.contains("recent"), "{}", declarations.rbs);
        assert!(!declarations.rbs.contains("status"), "{}", declarations.rbs);
        assert_eq!(
            model.collections(&known()).collect::<Vec<_>>(),
            ["Comment", "Comment"],
            "a concern asks for its elements' relations and never for one of its own"
        );
        assert_eq!(model.retyped_columns().count(), 0);
    }

    #[test]
    fn with_options_hands_its_keywords_to_the_macros_inside_it() {
        // The merge is not a refinement: without it `belongs_to :approved_by_account` names an
        // `ApprovedByAccount` no application defines, and the eleven `belongs_to`s the corpus
        // writes this way declare nothing at all. The call's own keyword wins over the
        // enclosing one, which is `ActiveSupport::OptionMerger`'s own `deep_merge` order.
        const SOURCE: &str = "\
class Story < ApplicationRecord
  with_options class_name: \"User\", optional: true do
    belongs_to :approved_by
    belongs_to :author, class_name: \"Comment\"
    belongs_to :owner, polymorphic: true
  end

  has_many :votes
  with_options through: :votes do |s|
    s.has_many :voters, source: :user
  end
end
";
        let mut known = known();
        known.insert("Vote".to_owned());
        let mut relations = relations();
        relations.insert("User".to_owned());
        relations.insert("Vote".to_owned());
        let facts = read_model(SOURCE).signatures(
            "app/models/story.rb",
            &Elsewhere {
                known: &known,
                models: &known,
                relations: &relations,
                ..Elsewhere::nothing()
            },
        );
        let rbs = facts.render(&declaring(&[])).rbs;
        assert!(rbs.contains("def approved_by: () -> User?"), "{rbs}");
        assert!(
            rbs.contains("def author: () -> Comment?"),
            "the call's own `class_name:` wins over the block's: {rbs}"
        );
        assert!(
            !rbs.contains("owner"),
            "`polymorphic: true` still declines, whoever wrote it: {rbs}"
        );
        assert!(
            rbs.contains("def voters: () -> User::Relation"),
            "a `through:` the block passed, on a macro called on the block's own parameter: {rbs}"
        );
    }

    #[test]
    fn a_namespaced_model_is_spelled_the_way_rubydex_spells_it() {
        // Unlike a table, an association on a namespaced class is answerable: the class's name
        // is written right there, and nothing about the member depends on a prefix that only
        // runs. Three spellings, one answer each.
        //
        // The *document* each writes is not the same, and that is the namespace rule rather than this
        // reader: the first spelling writes `module Admin` on a line of its own, so every
        // segment of the name is declared and it stays joined. The second leaves `Admin` to
        // Zeitwerk, so a joined name would introduce it — and the name is written out one body
        // per segment instead. The owner is `Admin::Setting` in both, which is the thing this
        // test is about.
        for (source, class, modules, opens) in [
            (
                "module Admin\n  class Setting\n    belongs_to :user\n  end\nend\n",
                "Admin::Setting",
                &["Admin"][..],
                "module Admin\nclass Setting\n",
            ),
            (
                "class Admin::Setting\n  belongs_to :user\nend\n",
                "Admin::Setting",
                &[][..],
                "class Admin::Setting\n",
            ),
            (
                "class ::Setting\n  belongs_to :user\nend\n",
                "Setting",
                &[][..],
                "class Setting\n",
            ),
        ] {
            let known = ["User".to_owned(), class.to_owned()].into_iter().collect();
            let rbs = read_model(source)
                .signatures(
                    "app/models/setting.rb",
                    &Elsewhere {
                        known: &known,
                        models: &known,
                        ..Elsewhere::nothing()
                    },
                )
                .render(&declaring(modules))
                .rbs;
            assert!(rbs.starts_with(opens), "{rbs}");
            assert!(rbs.contains("def user: () -> User\n"), "{rbs}");
        }
    }

    #[test]
    fn a_source_names_the_class_a_through_association_really_collects() {
        // `has_many :voters, through: :votes, source: :user` is a collection of `User`, and
        // `source:` is the only thing in the call that says so.
        let source = "class Story\n  has_many :votes\n  has_many :voters, through: :votes, source: :user\nend\n";
        let known = ["Story", "User", "Vote"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let relations = ["User", "Vote"].into_iter().map(str::to_owned).collect();
        let rbs = read_model(source)
            .signatures(
                "app/models/story.rb",
                &Elsewhere {
                    known: &known,
                    models: &known,
                    relations: &relations,
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]))
            .rbs;
        assert!(rbs.contains("def voters: () -> User::Relation\n"), "{rbs}");
    }

    #[test]
    fn what_the_relation_class_is_called() {
        assert_eq!(relation_of("Comment"), "Comment::Relation");
        assert_eq!(relation_of("Admin::Setting"), "Admin::Setting::Relation");
    }
}
