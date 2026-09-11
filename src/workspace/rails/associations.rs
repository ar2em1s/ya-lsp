//! One association macro, read and then declared: `belongs_to`, `has_one`, `has_many`,
//! `has_and_belongs_to_many` and `scope`.
//!
//! The fifth [`read`] in this directory and the last of them to get a module of its own — `enums`,
//! `attributes`, `delegates` and `tail` have had one since they were written, and these five
//! macros stayed inside the walk that finds them. What separates a family here is what separates
//! one there: reading a call is one question and deciding what it may declare is another, and
//! between the two sits the only gate in the directory that asks about the class a macro is
//! written **on** rather than about the class it names — because `has_many :comments` is
//! byte-identical in a model and in a serializer, where it stores an attribute and defines no
//! method at all.
//!
//! The collections are monomorphic. `has_many :comments` returns a `Comment::Relation` this
//! crate writes — see [`relations`](super::relations) — one per element type rather than one per
//! association, and **nothing in it is mapped**: no line of anybody's code declares
//! `Comment::Relation#first`, and a relation four models share could only be pointed at an
//! arbitrary one of them.
//!
//! Which bodies these calls are read out of is [`models`](super::models)' question, not this
//! module's: [`read`] is handed a call and the nesting it was written in, and says what the call
//! means.

use std::collections::BTreeSet;

use ruby_prism::CallNode;

use super::ASSOCIATIONS;
use super::inflect::{camelize, singularize};
use super::models::Elsewhere;
use super::relations::{Chained, relation_of};
use super::syntax::{first_symbol_or_string, header, inherited, string_literal, symbol_or_string};
use crate::generated::candidates;
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

/// Why an association's own call says no single class can be named.
///
/// Not the same silence as a name the application does not define, and that difference is the
/// whole of why this exists. `belongs_to :parent_comment` naming no `ParentComment` is a *lookup* that
/// came back empty — the reader may have camelized the wrong word, or the class may be in a gem
/// this pass cannot see — and declining it whole is right. These two are the call telling this
/// reader outright that the question has no answer, and that is knowledge: the member exists,
/// it is reached at the macro line, and its type is the one thing nobody can write down.
///
/// **Each is also positive evidence that the call is Rails'.** `polymorphic:`, `class_name:` and
/// `source_type:` are ActiveRecord's own keywords, which is what lets these declare a member
/// where a bare `belongs_to` with an unknown class still declares nothing — the same argument
/// [`attributes`](super::attributes) makes from a cast type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Undecided {
    /// `polymorphic: true` — a companion `*_type` column names the class, one row at a time.
    Polymorphic,
    /// One of [`NAMES_A_CLASS`] written as anything but a string literal. Solidus lets the host
    /// application supply its own user class through a runtime object, and no reading of the
    /// text can turn one into a name. Which keyword was written is carried because the card
    /// says it: it is the word the reader has to go and look at.
    Unreadable(&'static str),
}

impl Undecided {
    /// The clause the hover card ends with, after the file and the macro.
    fn because(self) -> String {
        match self {
            Self::Polymorphic => "whose class a `_type` column names one row at a time".to_owned(),
            Self::Unreadable(keyword) => {
                format!("whose `{keyword}:` is not a literal, so no class can be read out of it")
            }
        }
    }
}

/// What a member whose class nobody can name hands back.
const UNTYPED: &str = "untyped";

/// The keywords that name an association's class outright, in Rails' order of precedence.
///
/// `class_name:` is every association's own and wins. `source_type:` is ActiveRecord's
/// disambiguator for a `through:` whose source association is polymorphic, and where it is
/// written it is the answer and `source:` is not — which is the whole difference between the two
/// neighbouring keywords: `source:` names a *member* of the joined model and has to be camelized
/// to guess at a class, `source_type:` names the class. Rails rejects `source_type:` as an unknown
/// key on any macro without a `through:`, so it is read wherever it is written rather than gated
/// on one; a line that could confuse the two does not boot.
///
/// Both take `compute_type`'s walk over the nesting, and both are read **only** as a string
/// literal — anything else is an [`Undecided::Unreadable`], not a fall through to the name.
const NAMES_A_CLASS: [&str; 2] = ["class_name", "source_type"];

/// One macro call, read.
#[derive(Debug)]
pub(super) struct Association {
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
    pub(super) candidates: Vec<String>,
    pub(super) kind: Kind,
    /// Why the call itself says no class can be named, when it does. [`candidates`] is empty
    /// whenever this is `Some`, and [`Association::declare`] writes an untyped member instead
    /// of nothing at all.
    ///
    /// [`candidates`]: Association::candidates
    undecided: Option<Undecided>,
    optional: bool,
    /// The association this one reads through, when it is a `has_many :through`.
    through: Option<String>,
    at: (u32, u32),
    name_at: (u32, u32),
}

pub(super) fn read<'pr>(
    source: &str,
    nesting: &[String],
    node: &CallNode<'pr>,
    hosts: &[CallNode<'pr>],
) -> Option<Association> {
    let called = String::from_utf8_lossy(node.name().as_slice()).into_owned();
    let (spelled, kind) = ASSOCIATIONS.iter().find(|(name, _)| *name == called)?;
    let (name, name_at) = first_symbol_or_string(source, node)?;
    let undecided = undecided(source, node, hosts);
    let candidates = match kind {
        // A scope returns a relation of the class it is written in, and its own name says
        // nothing about a type. The lambda's body is never read: that is the declarative
        // rule at its hardest case, and `-> { where(user: Current.user) }` is exactly the
        // Ruby this crate refuses to run.
        Kind::Scope => vec![nesting.join("::")],
        // No list to look up, and none is wanted: camelizing the association's own name
        // here would be a class that does not exist at best and the wrong one at worst,
        // which is the sentence this decline has always carried. What is new is that the
        // decline is now written down rather than dropped.
        _ if undecided.is_some() => Vec::new(),
        _ => target(source, nesting, node, hosts, &name, *kind)?,
    };
    Some(Association {
        spelled,
        name,
        candidates,
        kind: *kind,
        undecided,
        optional: inherited(node, hosts, "optional")
            .is_some_and(|value| value.as_true_node().is_some()),
        through: inherited(node, hosts, "through")
            .and_then(|value| Some(symbol_or_string(source, &value)?.0)),
        at: header(node)?,
        name_at,
    })
}

/// What the call says about its own class when what it says is "nobody can name it".
///
/// **Asked before the candidate list rather than after it**, because neither of these is a
/// lookup that failed: both are readable from the call alone, and reading them second would
/// mean camelizing a name first and then throwing the answer away.
///
/// `polymorphic:` is asked first and wins, which is Rails' own order — `compute_type` is
/// never reached for a polymorphic reflection, whatever else the line carries. The corpus
/// writes the pair together inside a `with_options class_name: "User"`, so the order is not
/// hypothetical.
///
/// Below it, **the first of [`NAMES_A_CLASS`] the call writes decides and the rest are not
/// read** — the same precedence [`target`] walks, stated once so the two halves
/// cannot drift. A readable `class_name:` therefore leaves nothing undecided even when the
/// `source_type:` beside it is a method call, because that `source_type:` was never going to
/// be asked.
fn undecided<'pr>(
    source: &str,
    node: &CallNode<'pr>,
    hosts: &[CallNode<'pr>],
) -> Option<Undecided> {
    if inherited(node, hosts, "polymorphic").is_some_and(|value| value.as_true_node().is_some()) {
        return Some(Undecided::Polymorphic);
    }
    let (keyword, value) = NAMES_A_CLASS
        .iter()
        .find_map(|&keyword| Some((keyword, inherited(node, hosts, keyword)?)))?;
    string_literal(source, &value)
        .is_none()
        .then_some(Undecided::Unreadable(keyword))
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
///
/// `source_type:` is read on the same terms and immediately after it, and the two of them are
/// [`NAMES_A_CLASS`]. It has to be read *before* `source:`, which is the only reason this read
/// is worth anything: eight lines across four corpora write both, and `source:` on every one
/// of them camelizes to a class the application does not have.
fn target<'pr>(
    source: &str,
    nesting: &[String],
    node: &CallNode<'pr>,
    hosts: &[CallNode<'pr>],
    name: &str,
    kind: Kind,
) -> Option<Vec<String>> {
    if let Some(written) = NAMES_A_CLASS
        .iter()
        .find_map(|&keyword| Some(string_literal(source, &inherited(node, hosts, keyword)?)?.0))
    {
        return Some(match written.strip_prefix("::") {
            Some(absolute) => vec![absolute.to_owned()],
            None => nested(nesting, &written),
        });
    }
    // `has_many :voters, through: :votes, source: :user` is a collection of `User`, and
    // `source:` is the only thing in the call that says so.
    let through = inherited(node, hosts, "source")
        .and_then(|value| Some(symbol_or_string(source, &value)?.0));
    let spelled = through.as_deref().unwrap_or(name);
    let bare = match kind {
        Kind::Many => camelize(&singularize(spelled)),
        _ => camelize(spelled),
    }?;
    Some(nested(nesting, &bare))
}

/// The same list [`syntax::candidates`] builds, for the body this reader is inside.
///
/// [`syntax::candidates`]: super::syntax::candidates
fn nested(nesting: &[String], name: &str) -> Vec<String> {
    candidates(&nesting.join("::"), name)
}

/// The body a macro was written in, as much of one as an association needs.
///
/// [`tail::Host`](super::tail::Host)'s shape and for its reason: three things that always
/// travel together, so that adding a fourth is one line here rather than a seventh parameter
/// on [`Association::declare`]. Which bodies exist and which of them may declare is
/// [`models`](super::models)' question — what arrives here is one that already passed it.
pub(super) struct Body<'a> {
    /// Spelled with its lexical nesting, exactly as rubydex spells it.
    pub(super) class: &'a str,
    /// Whether it is a `module` rather than a `class` — a concern.
    pub(super) module: bool,
    /// Every association in the same body, which is what a `through:` reads to find the
    /// intermediate it names.
    pub(super) siblings: &'a [Association],
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
    pub(super) fn resolved<'a>(&'a self, known: &BTreeSet<String>) -> Option<&'a str> {
        self.candidates
            .iter()
            .find(|candidate| known.contains(*candidate))
            .map(String::as_str)
    }

    /// Say this macro's member, or decline and say nothing.
    pub(super) fn declare(
        &self,
        facts: &mut Facts,
        chained: &mut Chained,
        file: &str,
        body: &Body<'_>,
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
        if !body.module && !models.contains(body.class) {
            return;
        }
        // An undecidable class is not a missing one: `candidates` is empty by construction and
        // `resolved` would decline every time, so the two are separated here rather than
        // conflated into one empty list. `untyped` is what the member hands back, and it is the
        // honest type — the alternative is not a better type, it is no member.
        let target = match (self.resolved(known), self.undecided) {
            (Some(target), _) => target,
            (None, Some(_)) => UNTYPED,
            (None, None) => return,
        };
        // `has_many :voters, through: :votes` reads a second association, and an intermediate
        // that is not declared on this class is a macro whose meaning is somewhere this reader
        // cannot see. Declining is the same answer a missing class gets.
        if let Some(through) = &self.through
            && !body.siblings.iter().any(|other| other.name == *through)
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
        if body.module && self.kind == Kind::Scope {
            for includer in includers.get(body.class).into_iter().flatten() {
                if !relations.contains(includer) {
                    continue;
                }
                chained.declare(
                    facts,
                    includer,
                    Declared {
                        owner: Owner::Singleton(includer.clone()),
                        name: self.name.clone(),
                        returns: relation_of(includer),
                        parameters: "(*untyped)".to_owned(),
                        because: format!(
                            "From `{file}`, `{} :{}` in `{}`, which `{includer}` includes.",
                            self.spelled, self.name, body.class
                        ),
                        at: Some((self.at, self.name_at)),
                        from: Source::Association,
                        overloads: Vec::new(),
                    },
                );
            }
            return;
        }
        let returns = match self.kind {
            // Before every other arm, and before the relation gate below it: `untyped?` is not
            // a type worth writing and `untyped::Relation` is not a class.
            _ if self.undecided.is_some() => UNTYPED.to_owned(),
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
            Kind::Scope => (Owner::Singleton(body.class.to_owned()), "(*untyped)"),
            _ if body.module => (Owner::Module(body.class.to_owned()), "()"),
            _ => (Owner::Instance(body.class.to_owned()), "()"),
        };
        let declared = Declared {
            owner: owner.clone(),
            name: self.name.clone(),
            returns,
            parameters: parameters.to_owned(),
            because: format!(
                "From `{file}`, `{} :{}`{}.",
                self.spelled,
                self.name,
                match (self.undecided, self.kind) {
                    (Some(why), _) => format!(", {}", why.because()),
                    (None, Kind::Scope) => String::new(),
                    (None, _) => format!(", which is a `{target}`"),
                }
            ),
            at: Some((self.at, self.name_at)),
            from: Source::Association,
            overloads: Vec::new(),
        };
        match self.kind {
            // The relation half — [`chainable`] has the argument. The gate it needs is the one
            // `returns` already applied: a `scope` whose class owns no relation class returned
            // above, so reaching here is itself the proof that there is a class to write on.
            Kind::Scope => chained.declare(facts, body.class, declared),
            _ => facts.declare(declared),
        }
        // Everything else the one macro line installs, and the list is Rails' own rather than
        // this reader's: `associations/builder/` — `Association::define_readers`/`define_writers`
        // for the pair every macro writes, `SingularAssociation::define_accessors` for the five a
        // `belongs_to` or a `has_one` adds, `CollectionAssociation`'s pair for `_ids`, and
        // `BelongsTo::define_change_tracking_methods`.
        //
        // Two things the builders say that a reading of the macro names would not. The
        // constructors are written `unless reflection.polymorphic?`, which is the rule the
        // undecidable branch below applies. And `_changed?` is `belongs_to`'s alone, `BelongsTo`
        // being the only builder that overrides `define_change_tracking_methods`.
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
        // **What an undecidable class costs is the type and never the member.** Everything the
        // macro installs that does not have to name a class is installed exactly as it would
        // be: `untyped` is already every value, so the `?` a nilable reader would carry says
        // nothing on top of it.
        let assigned = match self.undecided {
            Some(_) => UNTYPED.to_owned(),
            None => format!("{target}?"),
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
                    format!("({assigned})"),
                    assigned.clone(),
                );
                // The three that *do* name a class, and the one group an undecidable macro
                // loses. Rails writes them `unless reflection.polymorphic?` — there is nothing
                // to instantiate — and a `(*untyped) -> untyped` constructor for the
                // `class_name:` half states nothing the association's own name did not.
                if self.undecided.is_some() {
                    return;
                }
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::BTreeSet;

    use super::super::{MODEL, known, read_model, relation_classes};
    use super::*;
    use crate::analysis::testing::*;
    use crate::generated::declaring;

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
                    relations: &relation_classes(),
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
  # From `app/models/story.rb`, `belongs_to :owner`, whose class a `_type` column names one row at a time.
  def owner: () -> untyped
  # From `app/models/story.rb`, `belongs_to :owner`, which also installs `owner=`.
  def owner=: (untyped) -> untyped
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
class Story::Relation
  # From `app/models/story.rb`, `scope :recent`.
  def recent: (*untyped) -> Story::Relation
end
"
        );
        // Two: `Story`, and the `Story::Relation` its one `scope` is chained onto — this
        // document writes no superclass line for either, which is what [`Chained`] relies on.
        assert_eq!(declarations.classes, 2);
        // Eight readers; a writer and three constructors for each of the three **singular**
        // associations whose class this project defines; a writer and the `_ids` pair for each
        // of the three collections, whose constructors are the relation's; and a writer alone
        // for `owner`, which is the polymorphic one — it names the member and the line, and
        // Rails' own `unless reflection.polymorphic?` is why it names no constructor. `ghost`
        // names a class nothing defines and still declares nothing at all. The last one is the
        // `scope`'s relation-side copy, which carries the same span as its twin — both halves of
        // `Story.recent.recent` land on the one `scope :recent` line.
        assert_eq!(declarations.spans.len(), 8 + 3 * 4 + 3 * 3 + 1 + 1);
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
                    relations: &relation_classes(),
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
        // The macro that names no class points at its line exactly as the ones that do, which
        // is the whole of what an undecidable association buys: two members, one place, and a
        // `*_type` column deciding the type at run time where no reader can.
        for span in &declarations.spans[10..12] {
            assert_eq!(at(span), ("belongs_to :owner, polymorphic: true", "owner"));
        }
        // And a collection's three do the same: `has_many :comments` is one line and
        // `comments`, `comments=`, `comment_ids` and `comment_ids=` are four members of it.
        for span in &declarations.spans[17..21] {
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

    /// A collection whose class the call refuses to name — and the three of its four members
    /// that never needed one.
    #[test]
    fn a_collection_that_names_no_class_keeps_every_name_that_does_not_need_one() {
        // One corpus writes this exactly: a `has_many :through` whose `class_name:` is a runtime
        // object the host application supplies. What the call declines is the **type**; the
        // members are Rails' own and three of the four are type-independent already — the
        // writer takes anything, and what a primary key holds was never this reader's to say.
        //
        // It matters because the fall-through used to answer, and answer *absurdly*: singularize
        // and camelize `users` inside `Spree::Promotion::Rules::User` and the first candidate
        // the project defines is the class the macro is written on, so the collection was
        // declared as a relation of itself.
        const SOURCE: &str = "\
module Shop
  class Rule < ApplicationRecord
    has_many :memberships
    has_many :users, through: :memberships, class_name: Shop::UserHandle.new
  end
end
";
        let known: BTreeSet<String> = ["Shop::Rule", "Shop::Membership"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let rbs = read_model(SOURCE)
            .signatures(
                "app/models/shop/rule.rb",
                &Elsewhere {
                    known: &known,
                    models: &known,
                    relations: &known,
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]))
            .rbs;
        assert!(rbs.contains("def users: () -> untyped"), "{rbs}");
        assert!(rbs.contains("def users=: (untyped) -> untyped"), "{rbs}");
        // Singularized from the **association's own name**, exactly as a decidable collection
        // does — the class was never where that name came from.
        assert!(rbs.contains("def user_ids: () -> Array[untyped]"), "{rbs}");
        assert!(rbs.contains("def user_ids=: (untyped) -> untyped"), "{rbs}");
        // And the absurd answer the fall-through used to give is nowhere in the document.
        assert!(
            !rbs.contains("def users: () -> Shop::Rule::Relation"),
            "{rbs}"
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
                    relations: &relation_classes(),
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]))
            .rbs;
        // `polymorphic:` names a class only a column knows at run time — and says so, which is
        // why the member is here and untyped rather than absent.
        assert!(rbs.contains("def owner: () -> untyped"), "{rbs}");
        assert!(!rbs.contains("def build_owner"), "{rbs}");
        // `belongs_to :ghost` names a class nothing in the project defines, and nothing in the
        // call says that was deliberate. A lookup that came back empty is still a decline.
        assert!(!rbs.contains("def ghost"), "{rbs}");
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
    fn the_class_a_polymorphic_source_names_beats_the_source_and_loses_to_the_class_name() {
        // All three keywords on one association, and `Record` is deliberately a class this
        // application *does* define: a reader that fell through to `source:` would answer here
        // rather than decline, and answer wrong. Eight lines in four of the reference
        // repositories are this shape, and `source:` camelizes to the wrong class on every one.
        let source = "\
class Concept
  has_many :concept_memberships
  has_many :articles, through: :concept_memberships, source: :record, source_type: \"Article\"
  has_many :both, through: :concept_memberships, source: :record, source_type: \"Article\", class_name: \"Comment\"
  has_many :unknowable, through: :concept_memberships, source: :record, source_type: Constants::RECORD
end
";
        let known = [
            "Concept",
            "Article",
            "Comment",
            "Record",
            "ConceptMembership",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let relations = ["Article", "Comment", "Record", "ConceptMembership"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let rbs = read_model(source)
            .signatures(
                "app/models/concept.rb",
                &Elsewhere {
                    known: &known,
                    models: &known,
                    relations: &relations,
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]))
            .rbs;
        assert!(
            rbs.contains("def articles: () -> Article::Relation\n"),
            "{rbs}"
        );
        assert!(rbs.contains("def both: () -> Comment::Relation\n"), "{rbs}");
        assert!(rbs.contains("def unknowable: () -> untyped\n"), "{rbs}");
        assert!(!rbs.contains("Record::Relation"), "{rbs}");
        assert!(rbs.contains("`source_type:` is not a literal"), "{rbs}");
    }

    #[test]
    fn a_belongs_to_is_a_member_that_types_its_chain_and_jumps_to_the_macro() {
        // A `belongs_to` in one expression, and the three things that have to happen at once
        // are the three a column's first test asks for: the member exists, the chain off it is typed,
        // and the jump lands on the macro that said so rather than anywhere in the class.
        let source = "Story.new.user\n";
        let (mut harness, story, uri) = models_project(source);

        assert!(
            harness.has("Story#user()"),
            "the association is not a member"
        );

        let definition = harness.definition_at(&uri, source, "user");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(story.as_str()),
            "{definition}"
        );
        // `  belongs_to :user` on line 1, revealed whole, with the name selected past its colon.
        assert_eq!(
            (
                &definition[0]["targetRange"]["start"]["line"],
                &definition[0]["targetRange"]["start"]["character"],
                &definition[0]["targetSelectionRange"]["start"]["character"],
            ),
            (
                &serde_json::json!(1),
                &serde_json::json!(2),
                &serde_json::json!(14),
            ),
            "{definition}"
        );
    }

    #[test]
    fn what_an_association_declares_and_what_it_refuses_to() {
        // The options table, as behaviour. `class_name:` wins over the association's own name
        // and it is not a refinement — 32 of the corpus's 76 carry one, and without it
        // `parent_story` camelizes to a class no application has ever defined. What is declined
        // is a *lookup* that came back empty; a call that says outright that no class can be
        // named — `polymorphic:`, or a `class_name:` that is not a literal — declares the member
        // anyway and leaves the type open.
        let (harness, _story, _uri) = models_project("");

        assert!(harness.has("Story#user()"));
        assert!(
            harness.has("Story#parent_story()"),
            "class_name: was not read"
        );
        assert!(harness.has("Story#draft()"));
        assert!(harness.has("Story#comments()"));
        assert!(
            harness.has("Story#tags()"),
            "has_many :through was not read"
        );

        assert!(
            harness.has("Story#owner()"),
            "`polymorphic: true` says the member exists and names no class; it must not take \
             the member with it"
        );
        assert!(
            harness.has("Story#keeper()"),
            "a `class_name:` that is not a literal says the same thing"
        );
        assert!(
            !harness.has("Story#ghost()"),
            "a class nobody defines must decline"
        );
        assert!(
            !harness.has("Story#voters()"),
            "a through: naming no association on this class must decline"
        );
    }

    #[test]
    fn a_hover_on_an_association_says_which_file_and_which_class_it_came_from() {
        // The provenance rule, and the boundary it exists to hold: the card names the file, the
        // macro and the class it resolved to, and it does so because the *generated RBS* carries
        // a comment above the `def`. Nothing in `hover.rs` knows the word `belongs_to`.
        let source = "Story.new.parent_story\n";
        let (mut harness, _story, uri) = models_project(source);

        let card = card(&mut harness, &uri, source, "parent_story");
        assert!(card.contains("app/models/story.rb"), "{card}");
        assert!(card.contains("belongs_to :parent_story"), "{card}");
        assert!(card.contains("which is a `Story`"), "{card}");
    }

    #[test]
    fn a_macro_that_names_no_class_still_names_the_member_and_the_line() {
        // The whole of the trade, in one expression. A polymorphic `belongs_to` has no class to
        // name and never had one — but it has a **member**, and the place a reader wants is the
        // line that declared it. Emitting nothing said both of those at once, and one rung down
        // nothing is indistinguishable from having never looked: the cursor fell through to the
        // name-based list, which answered with whichever unrelated `def owner` the workspace
        // happened to hold. So the decline is written down as a member with no type.
        let source = "Story.new.owner\n";
        let (mut harness, story, uri) = models_project(source);

        let definition = harness.definition_at(&uri, source, "owner");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(story.as_str()),
            "{definition}"
        );
        // `  belongs_to :owner, polymorphic: true` on line 3, with the name selected past its
        // colon — the same span a `belongs_to` that does name a class answers with.
        assert_eq!(
            (
                &definition[0]["targetRange"]["start"]["line"],
                &definition[0]["targetSelectionRange"]["start"]["character"],
            ),
            (&serde_json::json!(3), &serde_json::json!(14)),
            "{definition}"
        );
        assert_eq!(definition.as_array().map(Vec::len), Some(1), "{definition}");

        // And the card says which line and why there is no type, rather than claiming one.
        let card = card(&mut harness, &uri, source, "owner");
        assert!(card.contains("belongs_to :owner"), "{card}");
        assert!(card.contains("`_type` column"), "{card}");
        assert!(
            !card.contains("Matched on the method name alone"),
            "the member is the answer, so the name rung is never reached: {card}"
        );
    }

    #[test]
    fn a_class_name_nobody_can_read_a_class_out_of_is_the_same_answer() {
        // Solidus' `class_name: Spree::UserClassHandle.new` — the host application supplies its
        // own user class through a runtime object, and no reading of the text turns one into a
        // name. Measured, the two positions this shape holds answered a *spec* file before this:
        // the fall-through camelized `user`, found no `Keeper` either, and handed the name to
        // the list. The keyword is ActiveRecord's own, which is the evidence that makes this a
        // deliberate refusal rather than a lookup that failed.
        let source = "Story.new.keeper\n";
        let (mut harness, story, uri) = models_project(source);

        let definition = harness.definition_at(&uri, source, "keeper");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(story.as_str()),
            "{definition}"
        );
        let card = card(&mut harness, &uri, source, "keeper");
        assert!(card.contains("belongs_to :keeper"), "{card}");
        assert!(card.contains("not a literal"), "{card}");
    }

    #[test]
    fn a_member_with_no_type_ends_the_chain_rather_than_guessing_one() {
        // The other half of "no type": what is *not* claimed. `owner` answers its own line, and
        // what a `.` after it can reach is nothing at all — an untyped member is the end of a
        // chain, and inventing an `Owner` class from six letters is exactly the answer that
        // must not come back.
        let source = "Story.new.owner.title\n";
        let (mut harness, _story, uri) = models_project(source);

        let hover = harness.hover_at(&uri, source, "title");
        assert!(
            hover.is_null(),
            "an untyped receiver answered something: {hover}"
        );
    }

    #[test]
    fn optional_is_what_makes_a_belongs_to_nilable_and_a_has_one_always_is() {
        // The nullability half. Rails 5 made `belongs_to` non-`nil` by default, so the
        // presence of the option is what makes the member optional — and `has_one` is optional
        // whatever anyone writes, because nothing in the file says the other record exists.
        let (harness, _story, _uri) = models_project("");
        let rbs = harness.generated_rbs("app/models/story.rb");

        assert!(rbs.contains("def user: () -> User\n"), "{rbs}");
        assert!(rbs.contains("def parent_story: () -> Story?\n"), "{rbs}");
        assert!(rbs.contains("def draft: () -> Comment?\n"), "{rbs}");
    }

    /// The nesting walk, end to end, and the fixture is the point: both spellings exist.
    ///
    /// Rails resolves an association's class against the module nesting of the class the macro
    /// is written on — `Spree::LineItem` naming `Adjustment` tries `Spree::LineItem::Adjustment`,
    /// then `Spree::Adjustment`, and the bare `Adjustment` **last** — so a workspace holding
    /// both answers with the nested one. Taking the bare one is wrong at real sites, which is why
    /// the assertion is on the *place* rather than on whether an answer exists: where both
    /// classes declare a `total`, the wrong order sends the jump to the wrong file, silently.
    ///
    /// The `has_many` is the other half and it is not a repetition: `Model::collections` asks
    /// which relation classes are needed and `Model::signatures` asks what each member returns,
    /// and two different answers put a `Spree::Order` member behind an `Order::Relation`.
    #[test]
    fn an_association_names_the_class_the_nesting_reaches_and_not_the_bare_one() {
        let source =
            "Spree::LineItem.new.adjustment.total\nSpree::LineItem.new.orders.first.number\n";
        let (mut harness, _story, uri) = models_project(source);
        let bare_adjustment = harness.write(
            "app/models/adjustment.rb",
            "class Adjustment < ApplicationRecord\n  def total\n  end\nend\n",
        );
        let adjustment = harness.write(
            "app/models/spree/adjustment.rb",
            "module Spree\n  class Adjustment < ApplicationRecord\n    def total\n    end\n  \
             end\nend\n",
        );
        let bare_order = harness.write(
            "app/models/order.rb",
            "class Order < ApplicationRecord\n  def number\n  end\nend\n",
        );
        let order = harness.write(
            "app/models/spree/order.rb",
            "module Spree\n  class Order < ApplicationRecord\n    def number\n    end\n  end\nend\n",
        );
        let line_item = harness.write(
            "app/models/spree/line_item.rb",
            "module Spree\n  class LineItem < ApplicationRecord\n    belongs_to :adjustment\n    \
             has_many :orders\n  end\nend\n",
        );
        harness.watch(&[
            &bare_adjustment,
            &adjustment,
            &bare_order,
            &order,
            &line_item,
        ]);

        assert!(
            harness.has("Spree::LineItem#adjustment()") && harness.has("Spree::LineItem#orders()"),
            "both macros are members"
        );
        let singular = harness.definition_at(&uri, source, "total");
        assert_eq!(
            (singular.as_array().map(Vec::len), &singular[0]["targetUri"]),
            (Some(1), &serde_json::json!(adjustment.as_str())),
            "one place, and it is the nested class: {singular}"
        );
        let collection = harness.definition_at(&uri, source, "number");
        assert_eq!(
            (
                collection.as_array().map(Vec::len),
                &collection[0]["targetUri"]
            ),
            (Some(1), &serde_json::json!(order.as_str())),
            "and the relation is a relation of the same class: {collection}"
        );
    }

    /// The host test end to end: the same macro in three bodies, and only two of them mean it.
    ///
    /// `active_model_serializers` spells `has_many`, `has_one` and `belongs_to`, stores an
    /// `Attribute` and defines **no method** — a serializer answers `respond_to?` false for
    /// every name its own macro wrote — and `has_many :comments` is byte-identical in a model
    /// and in one, so there is no shape to gate on and the host has to be asked. This runs the
    /// whole pass so that `Context::models` is what answers, rather than a set a unit test
    /// handed in.
    #[test]
    fn a_serializer_writing_an_association_macro_declares_nothing() {
        let source = "Story.new.comments\n";
        let (mut harness, _story, _uri) = models_project(source);
        let serializer = harness.write(
            "app/serializers/story_serializer.rb",
            "class StorySerializer < ActiveModel::Serializer\n  \
             has_many :comments\n  belongs_to :user\nend\n",
        );
        harness.watch(&[&serializer]);

        assert!(
            harness.has("Story#comments()"),
            "the model still declares its own"
        );
        assert!(
            !harness.has("StorySerializer#comments()"),
            "and the serializer declares nothing"
        );
        assert!(
            !harness.has("StorySerializer#user()"),
            "for the singular macros as well as the collection"
        );
    }

    /// The same rule, reached from a body that is not a serializer at all.
    ///
    /// Solidus' `Spree::Admin::ResourceController` defines its own class-side `belongs_to` for
    /// nested-resource routing and defines no method either. A blocklist of serializer names
    /// would have to be told about it; an admit list already declines it, because a controller
    /// is neither a model nor a module.
    #[test]
    fn a_controller_writing_an_association_macro_declares_nothing() {
        let source = "Story.new.comments\n";
        let (mut harness, _story, _uri) = models_project(source);
        let controller = harness.write(
            "app/controllers/admin/stories_controller.rb",
            "class Admin::StoriesController < ApplicationController\n  \
             belongs_to :story\nend\n",
        );
        harness.watch(&[&controller]);

        assert!(
            !harness.has("Admin::StoriesController#story()"),
            "a controller is not a macro host"
        );
    }

    /// The includer fan-out in one expression: two includers, two relation types, one line.
    #[test]
    fn a_concerns_scope_is_a_class_method_of_every_class_that_includes_it() {
        // `scope :expired` in `Expireable` is `Poll.expired` **and** `Invite.expired`, and the
        // two answers are two different types — which is the whole reason a module body reads
        // the macro and writes nothing. The declaration goes into the *concern's* generated
        // document, once per pair, so both jumps land on the one `scope` line that said so and
        // no bookkeeping is added anywhere.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let concern = harness.write(
            "app/models/concerns/expireable.rb",
            "\
module Expireable
  extend ActiveSupport::Concern

  included do
    scope :expired, -> { where(\"expires_at < ?\", Time.now) }
  end
end
",
        );
        harness.write(
            "app/models/poll.rb",
            "class Poll < ApplicationRecord\n  include Expireable\nend\n",
        );
        harness.write(
            "app/models/invite.rb",
            "class Invite < ApplicationRecord\n  include Expireable\nend\n",
        );
        // Parenthesised on the first line only, so that each needle picks out one call.
        let source = "Poll.expired()\nInvite.expired\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        // The two types, from the text: one member declared twice, once per includer, in the
        // concern's own generated document. No hover card prints a return type, and the RBS is
        // where the two `Relation`s are visible at all.
        let rbs = harness.generated_rbs("app/models/concerns/expireable.rb");
        assert!(
            rbs.contains("def self.expired: (*untyped) -> Poll::Relation"),
            "{rbs}"
        );
        assert!(
            rbs.contains("def self.expired: (*untyped) -> Invite::Relation"),
            "{rbs}"
        );

        let poll = card(&mut harness, &uri, source, "expired()");
        assert!(poll.contains("Poll.expired"), "{poll}");
        assert!(poll.contains("which `Poll` includes"), "{poll}");
        let invite = card(&mut harness, &uri, source, "expired\n");
        assert!(invite.contains("Invite.expired"), "{invite}");
        assert!(invite.contains("which `Invite` includes"), "{invite}");

        // Both jump to the one `scope` line — line 4, the only line in the file that declares
        // anything — and neither lands in the model that includes the concern.
        for needle in ["expired()", "expired\n"] {
            let definition = harness.definition_at(&uri, source, needle);
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
        assert!(harness.has("Poll::<Poll>#expired()"));
        assert!(harness.has("Invite::<Invite>#expired()"));
    }

    /// The three declines, and the one that is a closure rather than a decline.
    #[test]
    fn which_classes_a_concerns_scope_reaches_and_which_it_does_not() {
        // A concern nobody includes declares nothing **and does not fall back to itself** —
        // `Orphan.forgotten` raises in Ruby, and the argument against writing it on the
        // module's own singleton is unchanged by having the includers in hand.
        //
        // An `include` naming a constant the application does not define resolves to nothing,
        // which is the decline every reader in this pass makes: `include Sidekiq::Worker` is a
        // real `include` of a class this workspace has never seen.
        //
        // A class that is not a model and owns no collection declines too, and the gate is
        // `relations` rather than a rule of its own: a `scope` fanned onto a PORO would need a
        // `Plain::Relation`, which is a class with no table behind it.
        //
        // And the closure. `ActiveSupport::Concern` hands an inner concern's `included` block
        // to whatever includes the outer one, so `Deep.forgotten` is real through two hops. No
        // corpus writes one, so this test is the only evidence for that property.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "app/models/concerns/orphan.rb",
            "module Orphan\n  included do\n    scope :forgotten, -> { all }\n  end\nend\n",
        );
        harness.write(
            "app/models/concerns/bigger.rb",
            "module Bigger\n  include Orphan\nend\n",
        );
        harness.write(
            "app/models/plain.rb",
            "class Plain\n  include Orphan\nend\n",
        );
        // An `include` that resolves to a **class** is not an edge: only a module can be
        // included, and a name that resolves to one of the application's classes says nothing
        // about where a concern's macros land.
        harness.write("app/models/widget.rb", "class Widget\nend\n");
        harness.write(
            "app/models/boxed.rb",
            "class Boxed < ApplicationRecord\n  include Widget\nend\n",
        );
        harness.write(
            "app/models/stranger.rb",
            "class Stranger < ApplicationRecord\n  include Sidekiq::Worker\nend\n",
        );
        // A module that includes itself is a `NoMethodError` at run time and an infinite loop
        // in a closure, and this walks the text rather than the run.
        harness.write(
            "app/models/concerns/knot.rb",
            "module Knot\n  include Knot\n  included do\n    scope :tied, -> { all }\n  end\nend\n",
        );
        harness.index();

        for absent in [
            "Orphan::<Orphan>#forgotten()",
            "Bigger::<Bigger>#forgotten()",
            "Plain::<Plain>#forgotten()",
            "Plain::Relation#first()",
        ] {
            assert!(!harness.has(absent), "{absent} was declared");
        }

        // Now a model two hops down, and nothing else changes.
        let deep = harness.write(
            "app/models/deep.rb",
            "class Deep < ApplicationRecord\n  include Bigger\nend\n",
        );
        harness.watch(&[&deep]);
        assert!(
            harness.has("Deep::<Deep>#forgotten()"),
            "a concern reached through another concern still lands"
        );
        assert!(!harness.has("Bigger::<Bigger>#forgotten()"));
    }

    /// The collision worth arguing before believing, and the argument is that **both are
    /// real**.
    #[test]
    fn a_scope_written_in_both_a_concern_and_its_includer_is_two_places_and_one_type() {
        // `include Expireable` runs `included do … scope :recent … end` on `Poll` and `scope
        // :recent` in `Poll`'s own body runs on `Poll` too: two lines of Ruby, both of which
        // really do install `Poll.recent`, and Ruby keeps whichever ran last. Neither is a
        // guess and neither can be preferred by any evidence a file states, so both stand —
        // and they *may* stand, because the two declarations agree about the type by
        // construction. A `scope` returns a relation of the class it is installed on, and a
        // concern's is installed on the includer, which is the same class.
        //
        // That is the condition, and it is narrower than "two generators collided": where two
        // documents disagree about a type the rank has to be spent by the loser declining —
        // `Source::outranks`, or the column-versus-`enum` withdrawal. Here
        // there is nothing to decide, so the honest card is the one that names both lines.
        //
        // It measures **0** over six corpora; this test is the only place it happens.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "app/models/concerns/expireable.rb",
            "module Expireable\n  included do\n    scope :recent, -> { all }\n  end\nend\n",
        );
        harness.write(
            "app/models/poll.rb",
            "class Poll < ApplicationRecord\n  \
             include Expireable\n  \
             scope :recent, -> { order(:id) }\n\
             end\n",
        );
        let source = "Poll.recent.first\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let recent = card(&mut harness, &uri, source, "recent");
        assert!(
            recent.contains("Defined in 2 places"),
            "two places, and the card says so: {recent}"
        );
        assert!(recent.contains("which `Poll` includes"), "{recent}");
        assert!(
            recent.contains("`app/models/poll.rb`, `scope :recent`"),
            "{recent}"
        );

        // One type, and the chain is what proves it: two declarations of one member disagreeing
        // about what they return is the thing the rank exists to prevent, and here they cannot.
        let first = card(&mut harness, &uri, source, "first");
        assert!(first.contains("ActiveRecordRelation#first"), "{first}");
    }

    /// The writer half of an association, on both sides of it.
    ///
    /// A collection's constructors are the **relation's** — `relation.rb` defines `create` at
    /// 155, `create!` at 170 and `new` at 126 with `alias build new` at 134 — and a singular
    /// association's are three `def`s Rails writes onto the model from the macro line, in
    /// `associations/builder/singular_association.rb`. ya-lsp declared `build` and not `new`,
    /// which is an incoherence rather than a bound since they are one method, and declared none
    /// of the singular three at all.
    #[test]
    fn an_association_answers_what_it_builds_and_creates() {
        let source = "Story.first.comments.create!.story\n\
                      Story.first.comments.new.story\n\
                      Story.first.create_user\n\
                      Story.first.create_parent_story.comments\n\
                      Story.first.build_ghost\n";
        let (mut harness, _story, uri) = models_project(source);

        // The collection half, through the relation every model's inherits.
        for word in ["create!", "new"] {
            let card = card(&mut harness, &uri, source, word);
            assert!(
                card.contains(&format!("ActiveRecordRelation#{word}")),
                "{card}"
            );
        }
        assert!(harness.has("ActiveRecordRelation#new()"));
        // …and `new` is the relation's alone, because a model gets its own from `Class` and a
        // declaration on the class side would shadow something real.
        assert!(!harness.has("Story::<Story>#new()"));

        // The singular half, on the model, from the macro line.
        assert!(harness.has("Story#create_user()"));
        assert!(harness.has("Story#build_user()"));
        assert!(harness.has("Story#create_user!()"));
        let created = card(&mut harness, &uri, source, "create_user");
        assert!(created.contains("Story#create_user"), "{created}");
        assert!(
            created.contains("`belongs_to :user`"),
            "the macro line is the place: {created}"
        );
        // It chains, and it is **not** nilable where the reader is: `belongs_to :parent_story,
        // optional: true` reads a `Story?` and `create_parent_story` makes one, so it is a
        // `Story`.
        let chained = card(&mut harness, &uri, source, "comments");
        assert!(chained.contains("Story#comments"), "{chained}");
        assert!(
            !chained.contains("Matched on the method name alone"),
            "{chained}"
        );

        // A collection macro installs none of the three, because a collection's are the
        // relation's — and `has_many :comments` is in the same file as the `belongs_to` above.
        assert!(!harness.has("Story#build_comments()"));
        assert!(!harness.has("Story#create_comment()"));
        // A polymorphic `belongs_to` names no class, so Rails writes no constructors and
        // neither does this — `owner` is the fixture's polymorphic one.
        assert!(!harness.has("Story#build_owner()"));
        // …and `belongs_to :ghost` names a class the project does not define.
        assert!(!harness.has("Story#build_ghost()"));
        // `reload_x` and `reset_x` come off the same Rails method and are declined on their
        // measurement: 1 call site and 0 in six corpora.
        assert!(!harness.has("Story#reload_user()"));
        assert!(!harness.has("Story#reset_user()"));
    }

    /// The rest of what one association line installs — Rails' own "Auto-generated methods"
    /// table, and every row of it this reader can name.
    #[test]
    fn an_association_answers_its_writer_and_the_ids_of_a_collection() {
        let source = "Story.first.user = User.first\n\
                      Story.first.comments = []\n\
                      Story.first.comment_ids = []\n\
                      Story.first.tag_ids.join\n\
                      Story.first.comments.reload.first.story\n";
        let (mut harness, _story, uri) = models_project(source);

        // The singular writer, which is nilable whatever the reader is: `belongs_to :user` is
        // required and `story.user = nil` is still ordinary Ruby — the validation is what
        // fails, at save.
        assert!(harness.has("Story#user=()"));
        let written = card(&mut harness, &uri, source, "user =");
        assert!(written.contains("Story#user="), "{written}");
        assert!(
            written.contains("`belongs_to :user`"),
            "the macro line is the place: {written}"
        );

        // The collection's three. `has_many` singularizes the **association's own name** rather
        // than the class it resolves to, which is what `has_many :tags, through: :taggings`
        // shows: it collects a `Tag` and its ids are `tag_ids`.
        assert!(harness.has("Story#comments=()"));
        assert!(harness.has("Story#comment_ids()"));
        assert!(harness.has("Story#comment_ids=()"));
        assert!(harness.has("Story#tag_ids()"));
        // …and a singular association installs neither: `has_one :draft` is a `Comment`.
        assert!(!harness.has("Story#draft_ids()"));
        assert!(!harness.has("Story#comment_ids_ids()"));

        // The name comes from the **association** and the card says which macro wrote it —
        // `has_many :tags, through: :taggings` collects a `Tag`, and `tag_ids` is singularized
        // from `tags` rather than from the class.
        let ids = card(&mut harness, &uri, source, "tag_ids");
        assert!(ids.contains("`has_many :tags`"), "{ids}");
        // `Array[untyped]` and not a bare `untyped`. No hover card prints a return type, so the
        // chain is what states it: what a primary key holds is the schema's to say and is in
        // another generated document this reader cannot ask, but the *array* is known and is
        // what the call sites want.
        let joined = card(&mut harness, &uri, source, "join");
        assert!(joined.contains("Array#join"), "{joined}");

        // `Relation#reload` hands the relation back, so a chain runs on through it. It is the
        // relation's alone — `ActiveRecord::Base#reload` is an instance method, so
        // `Story.reload` reaches `Class`, finds nothing and raises.
        assert!(harness.has("ActiveRecordRelation#reload()"));
        assert!(!harness.has("Story::<Story>#reload()"));
        let reloaded = card(&mut harness, &uri, source, "story\n");
        assert!(reloaded.contains("Comment#story"), "{reloaded}");

        // The four Rails installs that are declined on their measurement: eight call sites in
        // six applications against a name per association.
        assert!(!harness.has("Story#user_changed?()"));
        assert!(!harness.has("Story#user_previously_changed?()"));
        // A polymorphic `belongs_to` names no class, and the writer is one of the two names
        // that do not need one: `story.owner = account` is what the column is for.
        assert!(harness.has("Story#owner=()"));
    }
}
