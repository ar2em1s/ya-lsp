//! One model file: which bodies it holds, which of them a macro may be written in, and what the
//! whole of it declares.
//!
//! [`read_model`] reads every macro in [`super::MACROS`] — and only where they are **statements
//! of a class or module body**, which is the bounding rule `synthesized.md` states as a safety
//! property: the body of an `included do`, of a `with_options`, of a `def` and of an `if` are all
//! Ruby that only runs. That rule is this module's whole subject. What each macro then *means* is
//! its own family's — [`associations`], [`enums`], [`attributes`], [`delegates`] and [`tail`],
//! one `read` apiece — and what a collection of one turns out to be is
//! [`relations`](super::relations)'.
//!
//! [`Model::signatures`] is where those come back together, because the order they declare in is
//! a property of the file rather than of any one of them: a `scope`'s relation half waits for the
//! end of the document, an `attribute` speaks last for a name, and the class side is written once
//! per base rather than once per model.

use std::collections::{BTreeMap, BTreeSet};

use ruby_prism::{CallNode, Node, StatementsNode};

use super::associations::{self, Association, Kind};
use super::attributes::{self, Attribute};
use super::concerns::{self, ClassMethod};
use super::delegates::{self, Delegate};
use super::enums::{self, Enum};
use super::relations::{Chained, callbacks, class_side, relation};
use super::syntax::{self, block_parameter, constant_spelling, reads_local, symbol_or_string};
use super::tail::{self, Host, Tail};
use crate::generated::{At, Facts, Namespaces, Owner};

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
    /// The modules this body's `included do` blocks `extend` onto every including class.
    ///
    /// The one list here whose members this directory cannot name: they are `def`s in the
    /// extended module's own file. [`Model::extended`] hands the names out and
    /// `analysis::synthesize` finishes it against the graph.
    extends: Vec<concerns::Extended>,
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
    /// Every `def` this body's `class_methods do` blocks and `module ClassMethods` bodies write.
    ///
    /// The only list here that is not declared on this body at all: a concern owns none of them.
    /// They go on the singleton of every class that includes it, which is where `base.extend` puts
    /// them and where an ordinary ancestor walk then finds them. See [`super::concerns`].
    class_methods: Vec<ClassMethod>,
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

/// ActiveRecord's own base class, which is where Rails puts the query interface.
///
/// The query interface reaches it only when the **bundle** declares it — see
/// `Analysis::model_declarations` — so this names a class somebody else wrote rather than one
/// this crate invents, exactly as [`MESSAGE_DELIVERY`](crate::workspace::rails::MESSAGE_DELIVERY)
/// does.
pub const RECORD_BASE: &str = "ActiveRecord::Base";

/// Whether a class writing this superclass is an ActiveRecord model, with no walk left to do.
///
/// The end of the chain `knowledge::rails::Projection::is_model` climbs, and the two spellings
/// are the only two an application writes: Rails has generated an
/// `ApplicationRecord` since 5.0 and everything older says `ActiveRecord::Base`. It is a
/// **suffix** test for the first, because `Spree::ApplicationRecord` is one and a lexical
/// nesting the reference does not carry would only make the string longer — which is how a
/// mailer's base is recognised too, asked here about a model.
///
/// A chain rather than one hop, and solidus is why: 101 of its models are two hops from the
/// base and 15 are four or five. The direction of a miss is the direction of every miss in this
/// directory — a class this does not recognise claims no table, and a table nobody claims
/// declares nothing.
#[must_use]
pub fn is_record_base(superclass: &str) -> bool {
    superclass == "ActiveRecord::Base"
        || superclass
            .rsplit("::")
            .next()
            .is_some_and(|last| last == "ApplicationRecord")
}

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

    /// The class methods every concern in this file installs on the classes that include it.
    ///
    /// **A second entry point beside [`Model::signatures`], driven by a list of its own**, and the
    /// split is the reason the list exists. Everything `signatures` declares is a member of a
    /// class the file itself writes, so a gem's copy of it is about a project that is not this
    /// one. A concern's class methods are the opposite: `ActiveModel::Validations::ClassMethods`
    /// holds `validates`, which every model in *this* application calls, and the file it is
    /// written in belongs to Rails. So the concern half is asked of a wider set of documents than
    /// the rest of this reader will ever be, and asking it through one function is what keeps the
    /// two sets from being confused for each other.
    #[must_use]
    pub fn class_methods(
        &self,
        file: &str,
        includers: &BTreeMap<String, BTreeSet<String>>,
        namespaces: &Namespaces,
    ) -> Facts {
        let mut facts = Facts::default();
        for class in &self.classes {
            concerns::declare(
                &mut facts,
                &concerns::From {
                    file,
                    concern: &class.name,
                    via: None,
                },
                &class.class_methods,
                includers,
                namespaces,
            );
        }
        facts
    }

    /// `(the concern, one module its `included do` extends onto every includer)`.
    ///
    /// The one thing this directory reads and cannot finish. `extend ActiveModel::Naming` says
    /// *which* module, and what a class gains by it is that module's own instance methods — `def`s
    /// in a file this reader was never handed. So the names travel out and
    /// `Analysis::concern_declarations` asks the graph, which is the one place in the crate that
    /// may.
    pub fn extended(&self) -> impl Iterator<Item = (&str, &str, At)> {
        self.classes.iter().flat_map(|class| {
            class
                .extends
                .iter()
                .map(|extended| (class.name.as_str(), extended.name.as_str(), extended.at))
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
        // Every `scope` in this file goes on two classes, and the second half of each waits here
        // until the file is done — [`Chained`] has both halves of the argument.
        let mut chained = Chained::default();
        for class in &self.classes {
            let body = associations::Body {
                class: &class.name,
                module: class.module,
                siblings: &class.associations,
            };
            for association in &class.associations {
                association.declare(&mut facts, &mut chained, file, &body, elsewhere);
            }
            for declared in &class.enums {
                declared.declare(
                    &mut facts,
                    &mut chained,
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
        // After every class, so one relation class is one body however many of this file's models
        // and concerns wrote a scope onto it; before `emit`, which only ever adds a superclass
        // line and a note, so the two cannot fight over which opens the body.
        chained.flush(&mut facts);
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
    /// What the project declares, so a generated name may be joined onto it.
    ///
    /// Asked by one generator here — [`concerns`], whose owner is a name **nested inside**
    /// another the application wrote, and the only one in this file whose spelling is not simply
    /// a class the reader already read.
    pub namespaces: &'a Namespaces,
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
        // Empty, and that is a **decline** rather than a blank: a project declaring nothing
        // cannot be joined onto, so `concerns` writes no `ClassMethods` module for a test that
        // did not say the concern's own `module` line exists. A test about that generator states
        // it, which is exactly what this default is for.
        static NO_NAMESPACES: LazyLock<Namespaces> = LazyLock::new(Namespaces::default);

        Self {
            known: &NOTHING,
            framework: &NOTHING,
            models: &NOTHING,
            relations: &NOTHING,
            emit: &NOTHING,
            bases: &NOTHING,
            includers: &NOBODY,
            namespaces: &NO_NAMESPACES,
            columns: &NO_COLUMNS,
        }
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
/// A third possible host is deliberately absent, and it is a host of something else.
/// `class_methods do`
/// holds **no macro at all** in any of the six corpora — its calls are `private` (19),
/// `delegate` (3) and `attr_reader` (2) — and one written there would be broken Ruby too:
/// `ActiveSupport::Concern` builds a nested `ClassMethods` module and `module_eval`s the block
/// on it, and a plain `Module` has no `has_many`. Nor would it declare on the module's own
/// singleton, which is wrong in
/// the other direction, which `synthesized.md` argues: those methods reach the includer by
/// `extend`, so the module's own singleton is exactly the place they are *not*.
///
/// **What the block does host is its own `def`s**, which is a question this test never asked:
/// they are members of the `ClassMethods` module Rails builds, not statements of the body around
/// it, so they are read by [`super::concerns`] and declared on that module rather than merged in
/// here. A host makes a block's statements the body's; that one makes them somebody else's.
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
    /// The modules every `included do` block `extend`s onto its includers, **part of
    /// [`Macros::is_empty`]** for `class_methods`' reason: `included do` is
    /// `ActiveSupport::Concern`'s own method, so a body holding one is a concern by construction.
    ///
    /// A name and never a member — the `def`s are in the extended module's own file, and
    /// `analysis::synthesize` is what asks the graph for them.
    extends: Vec<concerns::Extended>,
    /// The `def`s of every `class_methods do` block, and **part of [`Macros::is_empty`]** where
    /// `defined` is not.
    ///
    /// The difference is what the line of Ruby says. A `def` in a body is a method and evidence
    /// of nothing else; `class_methods do` is `ActiveSupport::Concern`'s own method and is
    /// written nowhere else, so a body holding one is a concern by construction and recording it
    /// widens the generated list by exactly the blocks that exist — 120 over six corpora.
    class_methods: Vec<ClassMethod>,
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
            && self.class_methods.is_empty()
            && self.extends.is_empty()
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
/// mastodon writes the second, and lobsters writes the first in the one class where reading
/// only the other spelling was a visible defect.
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
            // The other spelling of the same module, and it is read here rather than in
            // [`Self::collect`] because it is not a call: `module ClassMethods` is a statement of
            // this body that the loop below is about to descend into for its own sake. Both
            // spellings land in one list, which is Ruby — `class_methods` reopens the module a
            // hand-written one declares.
            found
                .class_methods
                .extend(concerns::nested(self.source, &statements, module));
            found
                .extends
                .extend(concerns::extended(self.source, &statements, module));
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
                    class_methods: found.class_methods,
                    extends: found.extends,
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
            } else if let Some(inner) = concerns::body(&call, &called, module) {
                // Not a [`HOSTS`] entry and not a second one in disguise: those two blocks make
                // their statements statements of the body around them, and this one makes its
                // `def`s members of a module that does not otherwise exist. Nothing else in the
                // block is read — a macro written there is broken Ruby, which is the argument
                // [`HOSTS`] already makes.
                into.class_methods
                    .extend(concerns::read(self.source, &inner, concerns::BLOCK));
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
                // written in a concern, and `included do` is a statement of this body by the
                // rule that keeps a concern's block in the class body, so both spellings arrive
                // here without a case for either.
                into.exports
                    .extend(syntax::positional_names(self.source, &call));
            } else if let Some(read) = tail::read(self.source, &call, &called) {
                into.tail.push(read);
            } else if let Some(association) =
                associations::read(self.source, &self.nesting, &call, hosts)
            {
                into.associations.push(association);
            }
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use crate::analysis::testing::*;

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

class NothingElse < ActionMailer::Base
  helper :application
end

class Story < ApplicationRecord
  has_many :comments
end
";
        let model = super::read_model(source);
        assert_eq!(
            model.helper_modules().collect::<Vec<_>>(),
            [
                (
                    "ApplicationMailer",
                    [
                        "ApplicationHelper".to_owned(),
                        "FrontendUrlsHelper".to_owned(),
                        "AuthenticationHelper".to_owned(),
                        "Spree::Admin::OrdersHelper".to_owned(),
                    ]
                    .as_slice()
                ),
                // `NothingElse` is here for the last clause of `Macros::is_empty`: a body whose
                // **only** macro is a `helper` is still a body this reader recorded, and it is the
                // one shape that reaches that clause with every earlier one already true.
                ("NothingElse", ["ApplicationHelper".to_owned()].as_slice())
            ]
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

    use super::super::{MODEL, known, relation_classes};
    use super::*;
    use crate::generated::declaring;

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
            // The three parameter lists this reader cannot take a name out of: an empty one, one
            // with no required parameter, and one whose first is destructured rather than named.
            // Each yields a receiver nothing can be attributed to, so a call on any name inside
            // declines exactly as a call on the wrong name does.
            "class Story\n  with_options dependent: :destroy do ||\n    s.has_many :tags\n  end\nend\n",
            "class Story\n  with_options dependent: :destroy do |*rest|\n    rest.has_many :tags\n  end\nend\n",
            "class Story\n  with_options dependent: :destroy do |(a, b)|\n    a.has_many :tags\n  end\nend\n",
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
                    relations: &relation_classes(),
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
        let mut relations = relation_classes();
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
            rbs.contains("def owner: () -> untyped"),
            "`polymorphic: true` wins over the block's `class_name:`, whoever wrote it: {rbs}"
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

    /// The union half, end to end: a model whose base class this pass cannot see is still one.
    ///
    /// `Tag`'s superclass lives in a gem's `lib/`, which `Context::models`' walk does not reach,
    /// so inheritance says nothing about it. `Story` says `has_many :tags`, which makes `Tag` a
    /// collection element, and that is the other half of the union the host test asks. Both of forem's
    /// two gem-rooted models are this shape and both would have lost their macros to a narrower
    /// rule.
    #[test]
    fn a_model_rooted_in_a_gem_keeps_its_macros() {
        let source = "Story.new.tags\n";
        let (mut harness, _story, _uri) = models_project(source);
        let tag = harness.write(
            "app/models/tag.rb",
            "class Tag < ActsAsTaggableOn::Tag\n  belongs_to :user\nend\n",
        );
        harness.watch(&[&tag]);

        assert!(
            harness.has("Tag#user()"),
            "the collection half of the union is what keeps this one"
        );
    }

    /// The property every concern body rests on, recorded as a test.
    ///
    /// A concern's macros are declared on the **module** and the `include` the user already
    /// wrote carries them: an RBS `module Storyish` and a Ruby `module Storyish`
    /// are one constant to rubydex, `find_member_in_ancestors` crosses the `include`, the
    /// member types a receiver in a class that never mentions it, and the chain continues
    /// through it. Nothing about resolution was added for any of that.
    #[test]
    fn a_concerns_macros_reach_every_class_that_includes_it() {
        let source = "Spiked.new.notes.first.story\n";
        let (mut harness, _story, uri) = models_project(source);
        let concern = harness.write(
            "app/models/concerns/storyish.rb",
            "\
module Storyish
  extend ActiveSupport::Concern

  included do
    has_many :notes, class_name: \"Comment\"
  end
end
",
        );
        let includer = harness.write(
            "app/models/spiked.rb",
            "class Spiked < ApplicationRecord\n  include Storyish\nend\n",
        );
        harness.watch(&[&concern, &includer]);

        assert_eq!(
            harness.declarations_of("Storyish#notes()"),
            1,
            "the member hangs on the module, once, not on any class"
        );
        assert!(
            !harness.has("Spiked#notes()"),
            "and it is not copied onto the includer"
        );
        let member = card(&mut harness, &uri, source, "notes");
        assert!(member.contains("Storyish#notes"), "{member}");
        assert!(
            member.contains("`has_many :notes`"),
            "the provenance names the macro in the concern: {member}"
        );
        assert!(!member.contains("guessed from the name"), "{member}");

        // …and the chain runs through it, so a concern's collection is worth exactly what a
        // class's is.
        let chained = card(&mut harness, &uri, source, "story");
        assert!(chained.contains("Comment#story"), "{chained}");

        // The corpus' dominant spelling is a concern nested under the class it is written for
        // — mastodon's `module Account::Interactions`, included by `class Account` — so the
        // generated `module` has a class in its own path. RBS holds that as happily as Ruby does.
        let nested = "Story.new.followers.first.story\n";
        let under = harness.write(
            "app/models/concerns/story/interactions.rb",
            "\
module Story::Interactions
  extend ActiveSupport::Concern

  included do
    has_many :followers, class_name: \"Comment\"
  end
end
",
        );
        let inside = harness.write("app/inside.rb", nested);
        let opened = harness.write(
            "app/models/story_opened.rb",
            "class Story\n  include Story::Interactions\nend\n",
        );
        harness.watch(&[&under, &inside, &opened]);
        assert!(harness.has("Story::Interactions#followers()"));
        let through = card(&mut harness, &inside, nested, "story");
        assert!(through.contains("Comment#story"), "{through}");

        // The jump lands on the macro in the concern, which is what the side table is for.
        let definition = harness.definition_at(&uri, source, "notes");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(concern.as_str()),
            "{definition}"
        );
        assert_eq!(
            definition[0]["targetRange"]["start"]["line"],
            serde_json::json!(4),
            "{definition}"
        );
    }

    /// The two halves of a concern a module cannot own — one fanned out to the includers, the
    /// other declared nowhere.
    ///
    /// A `scope` in a concern is a class method of the *includer*, so the module owns nothing
    /// and `Storyish.recent` must still answer nothing — it raises in Ruby. What the fan-out
    /// adds is the other end: `Spiked.recent` is real, returns a relation of `Spiked`, and is
    /// written into the *concern's* document so the jump lands on the one `scope` line.
    ///
    /// An `enum` is not fanned out with it, and the reason is not the same as the reason it was
    /// declined in the first place. mastodon's `Status::Visibility` declares `enum :visibility`
    /// and `statuses.visibility` is an `integer` column, so declaring the label would put a
    /// `String` and an `Integer` for one member into two generated documents with nothing able
    /// to see the pair, which is the defect the withdrawal exists to prevent. The column an
    /// `enum` re-types is a
    /// question about a *table*, and a concern claims none; that is still true with the
    /// includers in hand, because a concern included by two models re-types a column in each.
    /// It is **one call in six corpora**.
    #[test]
    fn what_a_concern_may_not_declare_it_declares_nowhere() {
        let (mut harness, _story, _uri) = models_project("");
        let concern = harness.write(
            "app/models/concerns/storyish.rb",
            "\
module Storyish
  included do
    scope :recent, -> { order(created_at: :desc) }
    enum :status, { draft: 0 }
    has_many :comments
  end
end
",
        );
        let includer = harness.write(
            "app/models/spiked.rb",
            "class Spiked < ApplicationRecord\n  include Storyish\nend\n",
        );
        harness.watch(&[&concern, &includer]);

        assert!(
            harness.has("Storyish#comments()"),
            "the instance half is still declared"
        );
        assert!(
            harness.has("Spiked::<Spiked>#recent()"),
            "the class side lands on the includer"
        );
        for absent in [
            "Storyish::<Storyish>#recent()",
            "Storyish#status()",
            "Spiked#status()",
            "Storyish#draft?()",
            "Spiked::<Spiked>#draft()",
            "Storyish::Relation#first()",
        ] {
            assert!(!harness.has(absent), "{absent} was declared");
        }
    }

    #[test]
    fn a_class_body_reaches_the_class_methods_of_every_concern_it_includes() {
        // The edge is walked at resolution rather than declared, because the
        // class it would have to be declared on is a gem's. Both spellings of the convention
        // land, `Plain#helper` does not — a module with no nested `ClassMethods` is not
        // extended onto anything — and every one of these was on the name rung before.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        // The `include` naming nothing puts an `Ancestor::Partial` in the chain, which the walk
        // has to step over rather than stop at — a class that includes one unresolvable module
        // still reaches every concern under it.
        let source = "\
class Story < ApplicationRecord
  include Nowhere::AtAll
  validates :title
  scope :recent, -> { all }
  belongs_to :author
  counts_by :author
  helper
end
";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        // The card names the **class** the member was declared onto and the concern that
        // installed it, because that is what the declaration says: the walk that used to answer
        // here named the `ClassMethods` module, which is a namespace no file declares.
        for (needle, owner) in [
            ("validates", "in `ActiveModel::Validations`"),
            ("scope", "in `ActiveRecord::Scoping::Named`"),
            ("belongs_to", "in `ActiveRecord::Associations`"),
            ("counts_by", "in `Countable`"),
        ] {
            let found = card(&mut harness, &uri, source, needle);
            assert!(found.contains(owner), "{found}");
            assert!(
                !found.contains("Matched on the method name alone"),
                "{needle} is resolved, not guessed: {found}"
            );
        }

        let guessed = card(&mut harness, &uri, source, "helper");
        assert!(
            guessed.contains("Matched on the method name alone"),
            "a module with no nested ClassMethods extends nothing: {guessed}"
        );
    }

    /// The third spelling, whose `def`s are in a file the concern only names.
    ///
    /// `extend ActiveModel::Naming` inside an `included do` is what installs `model_name` on every
    /// Rails model, and it is the one shape in this directory that cannot be finished where it is
    /// read: `Analysis::concern_declarations` asks the graph which document declares `Naming` and
    /// hands that document's text back.
    ///
    /// **The place is the `def` in the module's own file**, which is what makes the generated
    /// document keyed by that file rather than by the concern's.
    #[test]
    fn a_module_an_included_block_extends_lands_on_every_includer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/nameable.rb", EXTENDING_CONCERN);
        let naming = harness.write("app/models/naming.rb", EXTENDED_MODULE);
        let source = "\
class Story < ApplicationRecord
  include Nameable
end

Story.model_name
Story.not_installed
Story.hidden
";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        let found = card(&mut harness, &uri, source, "model_name");
        assert!(
            found.contains(
                "`def model_name` in `Naming`, which `Nameable`'s `included do … \
                            extend` puts on every including class — here `Story`"
            ),
            "the card names the module, the concern and the class: {found}"
        );
        assert!(
            !found.contains("Matched on the method name alone"),
            "resolved, not guessed: {found}"
        );

        let jump = harness.definition_at(&uri, source, "model_name");
        let spelled = serde_json::to_string(&jump).expect("json");
        assert!(
            spelled.contains(naming.as_str()),
            "the jump lands in the module's own file: {spelled}"
        );
        let line = EXTENDED_MODULE
            .lines()
            .position(|text| text.trim() == "def model_name")
            .expect("the fixture writes it");
        assert!(
            spelled.contains(&format!("\"line\":{line}")),
            "on the `def` itself, line {line}: {spelled}"
        );

        // A `def self.` is a singleton method of the module being extended, which `extend`
        // installs nowhere — so nothing resolves and the name rung is what is left.
        let guessed = card(&mut harness, &uri, source, "not_installed");
        assert!(
            guessed.contains("Matched on the method name alone"),
            "not_installed is not installed: {guessed}"
        );

        // **And a `private` one is not extended onto anybody either, nor guessed at.** It used
        // to come back as a name match — the same `def` the concern edge had just declined,
        // handed over one tier down. `Story.hidden` raises `NoMethodError` in Ruby whatever rung
        // produced it, and `Naming#hidden` is the only `hidden` in this workspace, so there is
        // nowhere to send a reader and no card is the honest answer. See `locator::Privacy`.
        assert!(
            harness
                .hover_at(&uri, source, "hidden")
                .get("contents")
                .is_none(),
            "a private method is not reachable through a receiver that is written"
        );
    }

    /// The shape the whole convention is worth, and it is written entirely in a gem.
    ///
    /// Rails' own concerns are where `validates`, `scope`, `belongs_to` and `has_many` come from,
    /// and `ActiveModel::API`'s two `extend` lines are what put `model_name` on every model in
    /// every application. None of that is in the user's code, so the `rails.concerns` list is the one
    /// projection in the pass a gem's `lib/` may join — and the chain it has to follow runs
    /// through `ActiveRecord::Base`, a class no file of the project's declares.
    ///
    /// The application writes one line: `class Story < Shouty::Base` — and the namespace above
    /// that class is one **no file of the project's declares**, which is the case
    /// `wanted_namespaces` has to reach for `Owner::Singleton` to be spellable at all.
    #[test]
    fn a_concern_a_gem_writes_reaches_the_application_through_a_class_in_the_gem() {
        let (dir, _elsewhere, env) = project_with_gem(
            "\
module Counting
  module ClassMethods
    def counts_by(column)
    end
  end
end

module Nameable
  included do
    extend Naming
  end
end

module Naming
  def model_name
  end
end

module Shouty
  class Base
    include Counting
    include Nameable
  end
end
",
        );
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let source = "\
class Story < Shouty::Base
end

Story.counts_by :author
Story.model_name
";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();
        harness.index_gems();

        for (needle, said) in [
            (
                "counts_by",
                "`def counts_by` in `module ClassMethods` in `Counting`",
            ),
            ("model_name", "`def model_name` in `Naming`"),
        ] {
            let found = card(&mut harness, &uri, source, needle);
            assert!(found.contains(said), "{needle}: {found}");
            assert!(
                found.contains("`Shouty::Base`"),
                "{needle} is declared on the gem's class, which `Story` inherits: {found}"
            );
            assert!(
                !found.contains("Matched on the method name alone"),
                "{needle} is resolved, not guessed: {found}"
            );
        }
    }

    #[test]
    fn a_def_written_inside_a_block_does_not_shadow_a_concerns_class_method() {
        // The question is answered before the fixture: **a block-owned `def` cannot be told
        // from a true top-level one.** rubydex's nesting stack
        // holds lexical scopes, `Class.new`/`Module.new` owners and methods, and a `describe
        // "x" do` pushes none of the three — so a `def` inside one is recorded exactly as a
        // `def` at the true top level, a private method of `Object`. There is no flag, no
        // variant and no nesting id that separates them, so declining to treat such a `def` as
        // `Object`'s is closed at the graph level.
        //
        // What is left is the **order**, and it was wrong rather than approximate: a module
        // `extend`ed onto a class object sits above `Class`, `Module` and `Object` in the
        // singleton chain, so the concern edge has to be reached before them. On
        // discourse this cost seven RSpec helpers' worth of `validate` in every model.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        harness.write(
            "spec/models/story_spec.rb",
            "describe \"the counter\" do\n  def counts_by(column)\n  end\nend\n",
        );
        let source = "\
class Story < ApplicationRecord
  counts_by :author
end
";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        let found = card(&mut harness, &uri, source, "counts_by");
        assert!(
            found.contains("`def counts_by` in `module ClassMethods` in `Countable`"),
            "the concern is above `Object` in the singleton chain: {found}"
        );
        assert!(
            !found.contains("Object#counts_by"),
            "and the spec helper is not a method of `Object` at all: {found}"
        );
    }

    #[test]
    fn a_top_level_def_still_answers_for_a_class_body_below_it() {
        // The regression declining a block-owned `def` would have risked and reordering does
        // not. A `def` at the true top level of a file **is** a private method of `Object`, so it really is
        // reachable from every class body in the workspace, and the root answer is kept
        // wherever the concern edge has nothing to say.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        harness.write("app/boot.rb", "def configure_everything\nend\n");
        let source = "\
class Story < ApplicationRecord
  configure_everything
end
";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        let found = card(&mut harness, &uri, source, "configure_everything");
        assert!(found.contains("Object#configure_everything"), "{found}");
        assert!(
            !found.contains("Matched on the method name alone"),
            "it resolves rather than falling to the name rung: {found}"
        );
    }

    #[test]
    fn a_concerns_class_method_never_displaces_one_the_class_really_declares() {
        // The edge is asked **after** the ordinary ancestor search and only when it found
        // nothing, which is the same rule `resolve_typed` holds for a derived receiver: a
        // worse answer may never displace a better one.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        let source = "\
class Story < ApplicationRecord
  def self.validates(*names)
  end

  validates :title
end
";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        let found = card(&mut harness, &uri, source, "validates :title");
        assert!(found.contains("Story.validates"), "{found}");
        assert!(!found.contains("ActiveModel"), "{found}");
    }

    #[test]
    fn a_scope_is_a_class_method_and_its_body_is_never_read() {
        // The declarative rule at its hardest case. `-> { order(created_at: :desc) }` is Ruby
        // that only runs, and this reads the macro's *name* and the class it is written in and
        // nothing else — which is enough, because a scope returns a relation of its own class
        // whatever the lambda does.
        let source = "Story.recent.first.user\n";
        let (mut harness, _story, uri) = models_project(source);

        assert!(
            harness.has("Story::<Story>#recent()"),
            "a scope is a singleton method"
        );
        let card = card(&mut harness, &uri, source, "user");
        assert!(card.contains("Story#user"), "{card}");
    }

    #[test]
    fn a_macro_that_is_not_a_statement_of_the_class_body_is_not_read() {
        // The bounding rule, sharpened for the one block that is a host. `included do` is
        // `ActiveSupport::Concern`'s and exists on a **module**; written in a `class` body it is
        // a `NoMethodError`, and 0 of the corpus' 147 macro-bearing ones are in one. Being
        // *inside* a module does not make a class body a module body.
        let (mut harness, _story, _uri) = models_project("");
        let concern = harness.write(
            "app/models/concerns/taggable.rb",
            "module Taggable\n  class Holder\n    included do\n      has_many :tags\n    end\n  end\nend\n",
        );
        harness.watch(&[&concern]);

        assert!(!harness.has("Taggable::Holder#tags()"));
        assert!(!harness.has("Taggable#tags()"));
    }
}
