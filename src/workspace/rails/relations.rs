//! The relation class every model gets, and the query interface both sides of it share.
//!
//! Nothing here reads a macro — [`models`](super::models) does that. This is what a collection
//! *is* once one has been read: `Comment::Relation`, a class this crate writes and no file
//! declares, and the vocabulary ActiveRecord puts on it and on the model's class object.
//!
//! The two halves are one module because they are one argument. **A relation class is empty.**
//! Every name on one comes from [`RELATION_BASE`], which is written once for the whole project
//! — the difference between 77,128 generated members on discourse and a number that does not
//! grow with the model count. The class side is that same list on the model's own **base**, and
//! what lets the two share it is [`query_interface`], which knows what each name returns
//! without knowing which model asked. The only member of a relation class with a place of its
//! own is a `scope`, which is why [`Chained`] is here rather than beside the macro it is read
//! from.

use crate::analysis::types::{COLLECTION, ELEMENT};
use crate::generated::{Declared, Facts, Owner, Source};

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

/// The relation-side half of every `scope` one document writes, held back until the end of it.
///
/// # A scope is a class method and it is also a relation method
///
/// The second half is not a detail. `ActiveRecord::Delegation` builds a module per relation class
/// holding every scope the model defines, which is what makes `Story.recent.visible.limit(10)` the
/// ordinary spelling of a query rather than a clever one. A declaration on the model's singleton
/// alone answers the **first** call of such a chain and nothing after it, so the word *after* a
/// name that resolved falls back to the name-based list — the one place a chain gets worse the
/// further the code has already got.
///
/// The relation copy carries the same span, so `Story.recent.visible` jumps to the `scope
/// :visible` line exactly as `Story.visible` does. It is the only member of a relation class with
/// a place: everything else on one is the query interface, which no file declares — see
/// [`relation`].
///
/// # Why it is held back rather than written beside its twin
///
/// [`Facts`] renders in the order it was told things and reopens a body every time the owner
/// changes, so declaring `Story.recent` and `Story::Relation#recent` alternately writes one
/// `class Story::Relation ... end` per scope. Held to the end of the document, each relation class
/// is opened once — and where this document also writes that class' superclass line, the members
/// land in that same body.
///
/// [`flush`](Self::flush) sorts by owner, which is what makes one run per relation class out of a
/// concern whose scopes fan onto several includers in turn. The sort is **stable**, so within one
/// relation class the order is still the order the macros were written in — which is what
/// [`Facts`]' own collision rule reads.
#[derive(Default)]
pub(super) struct Chained(Vec<Declared>);

impl Chained {
    /// Say one `scope` on the class object now, and hold its relation copy back.
    ///
    /// Every caller goes through here — a `scope` on a class, the same `scope` fanned onto a
    /// concern's includers, and an `enum`'s class-side pair, which is a `scope` Rails writes
    /// itself — so no one of them can come apart from the others.
    pub(super) fn declare(&mut self, facts: &mut Facts, class: &str, declared: Declared) {
        self.0.push(Declared {
            owner: Owner::Instance(relation_of(class)),
            ..declared.clone()
        });
        facts.declare(declared);
    }

    /// Write the held-back half, grouped.
    pub(super) fn flush(mut self, facts: &mut Facts) {
        self.0
            .sort_by(|one, two| one.owner.name().cmp(two.owner.name()));
        for declared in self.0 {
            facts.declare(declared);
        }
    }
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

/// Where Rails itself writes the relation half of the query interface.
///
/// **One name and not ten.** `relation.rb` writes
/// `include FinderMethods, Calculations, SpawnMethods, QueryMethods, Batches, Explain, Delegation`
/// and rubydex has already walked it, so an ancestor walk from this one class *is* Ruby's own
/// method lookup and the answer it gives is `Method#owner`'s. Checked against
/// `Method#source_location` under activerecord 7.2, 8.0 and 8.1: of the 127 names this file
/// declares, **125 resolve and all 125 land on the line Ruby names** — the two that do not are
/// `instantiate`, which is the class side's alone, and `default_order`, which is in Rails' main
/// branch and in no released version.
///
/// The list is ordered and searched in order for [`RAILS_CLASS_SIDE`]'s sake, which needs three
/// names before this one.
pub const RAILS_RELATION: [&str; 1] = ["ActiveRecord::Relation"];

/// And the class half, which Ruby answers for nearly all of with a single line.
///
/// `Story.where` is `delegate(*QUERYING_METHODS, to: :all)` — `querying.rb:24` — and **no reader
/// can expand a splatted constant into ninety method names**, so `ActiveRecord::Querying` holds
/// no `def` for the graph to find. What is left is the ten names Rails does write a class-side
/// `def` for, which the three modules here own, and the rest, which take the relation's because
/// that is exactly what the `delegate` line says they are: `Story.where` is `Story.all.where`.
///
/// Measured the same way as [`RAILS_RELATION`] and across the same three activerecord versions:
/// **10 of the 10 class-side `def`s land where `Method#source_location` says**, and the other 110
/// are the delegated names.
///
/// `ActiveRecord::Base`'s own singleton is deliberately **not** on this list, although Ruby would
/// search it first. ya-lsp writes the class side onto that very singleton, so looking there would
/// find this crate's own declaration, which has no place, and stop — and the ten names that do
/// have one would lose it to the fall-through.
pub const RAILS_CLASS_SIDE: [&str; 4] = [
    "ActiveRecord::Persistence::ClassMethods",
    "ActiveRecord::Core::ClassMethods",
    "ActiveRecord::Inheritance::ClassMethods",
    "ActiveRecord::Relation",
];

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
/// **The four groups are Rails' own four call sites**, walked rather than remembered, for the
/// usual reason: a table this crate writes from a reading of the docs is a table that goes stale
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
pub(super) fn callbacks(facts: &mut Facts, base: &str) {
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
/// writes this text, so it never has to write an `ActiveRecord::Relation[Comment]` it cannot
/// then instantiate. The price is one class per element type and a name that must not collide,
/// and both are the caller's to hold.
///
/// **Nothing this function writes is mapped.** No line of anybody's code declares
/// `Comment::Relation#first`, so there is no span to record and
/// [`Origin::Unknown`](crate::analysis::synthesized::Origin) is the honest answer: it types a
/// chain and is never a jump target. A relation shared by four `has_many :comments` could be
/// pointed at one of them, and pointing at an arbitrary one of four is the confidently-wrong
/// answer this half of the release exists to avoid.
///
/// A **scope** is the one thing on a relation class that is mapped, and it is not written here:
/// a file really does declare `Comment::Relation#recent`, on the same line it declares
/// `Comment.recent`, so [`Chained`] puts the span on both.
///
/// One comment for the whole class, because the *class* is what ya-lsp invented: a reader who
/// reaches any member of it has already been told, by its name, that nobody wrote it.
/// [`class_side`] cannot say it that way and does not try.
pub(super) fn relation(facts: &mut Facts, element: &str) {
    let owner = Owner::Instance(relation_of(element));
    facts.note(
        owner.clone(),
        format!("A collection of `{element}`. ya-lsp writes this class; no file declares it."),
    );
    // And that is the whole of it. Every member of the query interface is on [`RELATION_BASE`],
    // written once for the project, because the two receiver-relative return types keep the
    // element out of the signatures — leaving each relation class the scopes [`Chained`] writes
    // onto it and nothing else. The body may well be opened by one of those instead: a document
    // that writes a scope onto a relation it was not asked to *emit* opens the class with no
    // superclass, and the two statements merge exactly as a reopened Ruby class does.
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
            from: Source::Query,
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
pub(super) fn class_side(facts: &mut Facts, base: &str) {
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
            from: Source::Query,
            overloads: query.overloads,
        });
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::BTreeSet;

    use super::super::{Elsewhere, MODEL, known, read_model, relation_classes};
    use super::*;
    use crate::analysis::testing::*;
    use crate::generated::declaring;
    use crate::workspace::rails;

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
                    relations: &relation_classes(),
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
        // callbacks are on the base, which this document was not asked to write. Eight readers,
        // four more for each of the three singular associations that name a class, three more
        // for each of the three collections, the writer of the polymorphic one, and the
        // `scope`'s second home on `Story::Relation` — see [`Chained`].
        assert_eq!(declarations.methods, 8 + 3 * 4 + 3 * 3 + 1 + 1);
        assert_eq!(declarations.spans.len(), 8 + 3 * 4 + 3 * 3 + 1 + 1);
        // `Story`, the `Story::Relation` the scope is chained onto, and the `Comment::Relation`
        // this caller asked for. The scope's own class is not among the bodies this document
        // opens for a superclass line, which is the point of the third: a relation class gets
        // members here whether or not this is the document that writes its superclass.
        assert_eq!(declarations.classes, 3);
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
                    relations: &relation_classes(),
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
    fn what_the_relation_class_is_called() {
        assert_eq!(relation_of("Comment"), "Comment::Relation");
        assert_eq!(relation_of("Admin::Setting"), "Admin::Setting::Relation");
    }

    #[test]
    fn a_collection_chains_through_a_relation_class_that_no_file_declares() {
        // The relation class's whole point. `story.comments` is a relation, `.first` is a
        // `Comment`, and
        // `.title` — hmm, `Comment` has no columns here, so the chain is checked one link
        // further along instead: `.first.story` is a `Story` again, which is the association
        // `belongs_to` wrote, reached through a class this pass invented.
        let source = "Story.new.comments.first.story\n";
        let (mut harness, _story, uri) = models_project(source);

        assert!(harness.has("Comment::Relation"));
        let card = card(&mut harness, &uri, source, "story");
        assert!(card.contains("Comment#story"), "{card}");
    }

    #[test]
    fn a_model_answers_the_query_interface_on_its_own_class() {
        // `Comment::Relation` is the hard half; without a class side nothing declares `first`,
        // `where` or `find` on the model *itself*, so a chain can be followed and never
        // started. `Story.recent.first.user` works off a macro alone and `Story.first.user`
        // does not, which is a strange thing for a server to be able to say.
        let source = "Story.first.user\n";
        let (mut harness, _story, uri) = models_project(source);

        assert!(
            harness.has("Story::<Story>#first()"),
            "the query interface is on the singleton, where rubydex files `def self.`"
        );
        let started = card(&mut harness, &uri, source, "user");
        assert!(started.contains("Story#user"), "{started}");
        assert!(!started.contains("guessed from the name"), "{started}");

        // `where` hands back the relation, which is the half that makes the two sides one fact:
        // `Story.where(...)` and `Story.all.where(...)` are the same method reached two ways.
        let relation = "Story.where(id: 1).first.user\n";
        let chained = harness.write("app/chained.rb", relation);
        harness.watch(&[&chained]);
        let card = card(&mut harness, &chained, relation, "user");
        assert!(card.contains("Story#user"), "{card}");
    }

    /// The word *after* a scope, which is the half a class object cannot answer.
    #[test]
    fn a_scope_is_declared_on_the_relation_as_well_as_on_the_class_object() {
        // `Story.recent.visible.first` is the ordinary spelling of a query and Rails makes it
        // work by delegating every scope to the relation — `ActiveRecord::Delegation` builds a
        // module per relation class holding them. Declared on the class object alone, the first
        // hop resolves and every hop after it falls to the name-based list: a chain that gets
        // *worse* the further the code has already got.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  \
             scope :recent, -> { order(:id) }\n  \
             scope :visible, -> { where(hidden: false) }\n\
             end\n",
        );
        let source = "Story.recent.visible.first\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        // The second hop, and it is the one this test exists for: same name, same line, and a
        // receiver that is the relation rather than the class object.
        let visible = card(&mut harness, &uri, source, "visible.");
        assert!(visible.contains("Story::Relation#visible"), "{visible}");
        assert!(
            visible.contains("`app/models/story.rb`, `scope :visible`"),
            "{visible}"
        );
        let definition = harness.definition_at(&uri, source, "visible.");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(story.as_str()),
            "{definition}"
        );
        assert_eq!(
            definition[0]["targetRange"]["start"]["line"],
            serde_json::json!(2),
            "{definition}"
        );

        // And the query interface still answers after it: the relation class the scope was
        // written onto is the same one that inherits [`rails::RELATION_BASE`].
        let first = card(&mut harness, &uri, source, "first");
        assert!(first.contains("ActiveRecordRelation#first"), "{first}");
        assert!(harness.has("Story::Relation#visible()"));
        assert!(harness.has("Story::<Story>#visible()"));
    }

    /// The same, where the two halves are written into **two different generated documents**.
    #[test]
    fn a_concerns_scope_reaches_the_relation_whose_class_another_document_wrote() {
        // A concern's `scope` is declared on each includer and lives in the *concern's*
        // document; the includer's relation class is written wherever it was first asked for,
        // which for a model no macro collects is the model's own. So `Poll::Relation` is opened
        // in `poll.rb`'s document with its superclass and reopened in `expireable.rb`'s with a
        // member — the one shape where a member and the class it hangs on are written by two
        // generators that never see each other.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let concern = harness.write(
            "app/models/concerns/expireable.rb",
            "module Expireable\n  included do\n    scope :expired, -> { all }\n  end\nend\n",
        );
        harness.write(
            "app/models/poll.rb",
            "class Poll < ApplicationRecord\n  include Expireable\nend\n",
        );
        let source = "Poll.expired.expired.first\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        // The member is in the concern's document and the superclass line is not.
        let rbs = harness.generated_rbs("app/models/concerns/expireable.rb");
        assert!(
            rbs.contains("class Poll::Relation\n  # From `app/models/concerns/expireable.rb`"),
            "{rbs}"
        );
        assert!(
            !rbs.contains(&format!("Poll::Relation < {}", rails::RELATION_BASE)),
            "{rbs}"
        );

        let chained = card(&mut harness, &uri, source, "expired.first");
        assert!(chained.contains("Poll::Relation#expired"), "{chained}");
        let definition = harness.definition_at(&uri, source, "expired.first");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(concern.as_str()),
            "{definition}"
        );

        // The reopening cost the class nothing: what `poll.rb`'s document said about its
        // superclass still stands, so the query interface answers after the second scope.
        let first = card(&mut harness, &uri, source, "first");
        assert!(first.contains("ActiveRecordRelation#first"), "{first}");
    }

    #[test]
    fn a_callback_macro_that_declares_nothing_still_answers_nothing() {
        // A bound rather than a gap: `define_model_callbacks` writes `before_save` at run time
        // and no file anywhere holds a `def` for it, so there is nothing for either half of the
        // concern edge to find. Declaring one is `models.rs`' job, and this asserts it was not
        // quietly done here.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        let source = "class Story < ApplicationRecord\n  before_save :normalize\nend\n";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        assert!(
            harness.definition_at(&uri, source, "before_save").is_null(),
            "nothing declares it, so there is nowhere to go"
        );
    }

    #[test]
    fn the_query_interface_is_read_by_arity_like_every_other_signature() {
        // The arity partition doing work it already does, on text this crate wrote rather than on
        // `vendor/rbs`. `find` requires an argument and `find_by` does not, so `Story.find(1)`
        // is a `Story` and `Story.find` is nothing — the arm that would have answered is one no
        // call reached, which is the whole of why a generated signature is safe to write.
        let source = "Story.find(1).user\n";
        let (mut harness, _story, uri) = models_project(source);
        let found = card(&mut harness, &uri, source, "user");
        assert!(found.contains("Story#user"), "{found}");
        assert!(
            !found.contains("Matched on the method name alone"),
            "{found}"
        );

        // `find_by` is declared `Story?`, and optional takes the inner type — so the chain off
        // it is the same one, which is the entry `types.md` calls the one inexact one.
        let maybe = "Story.find_by(id: 1).user\n";
        let by = harness.write("app/by.rb", maybe);
        harness.watch(&[&by]);
        let optional = card(&mut harness, &by, maybe, "user");
        assert!(optional.contains("Story#user"), "{optional}");

        let bare = "Story.find.user\n";
        let other = harness.write("app/bare.rb", bare);
        harness.watch(&[&other]);
        let missed = card(&mut harness, &other, bare, "user");
        assert!(
            missed.contains("Matched on the method name alone"),
            "a call no arm accepts is answered for by none of them: {missed}"
        );
    }

    #[test]
    fn the_query_interface_is_not_a_place_a_user_is_sent() {
        // The no-place clause, in a stronger version. A relation's members could at least
        // have been pointed at one of the `has_many`s that asked for the class; `Story.where`
        // could be pointed nowhere at all, because no line of anybody's code declares it.
        let source = "Story.first\n";
        let (mut harness, _story, uri) = models_project(source);
        assert!(
            harness.definition_at(&uri, source, "first").is_null(),
            "a declaration this crate invented must not be a place"
        );
    }

    #[test]
    fn every_model_answers_the_query_interface_and_nothing_else_does() {
        // The bound is the superclass chain and not "a class some macro made a collection",
        // which is evidence a file states and is also the wrong evidence: ActiveRecord answers `where` on a model because it is a model, and
        // `has_many` has nothing to do with it. So the bound is now the superclass chain, which
        // is evidence a file states too — and declaring the names on *everything* is still
        // how a convention table starts being wrong, which is what the last assertion holds.
        let (mut harness, _story, _uri) = models_project("");
        assert!(harness.has("Story::<Story>#where()"));

        let widget = harness.write(
            "app/models/widget.rb",
            "class Widget < ApplicationRecord\n  belongs_to :story\nend\n",
        );
        harness.watch(&[&widget]);
        assert!(harness.has("Widget#story()"), "the association still reads");
        assert!(
            harness.has("Widget::<Widget>#where()"),
            "nothing collects a Widget and it is a model regardless"
        );
        assert!(
            harness.has("Widget::Relation"),
            "and the relation the class side returns exists"
        );

        let gadget = harness.write("app/lib/gadget.rb", "class Gadget\nend\n");
        harness.watch(&[&gadget]);
        assert!(
            !harness.has("Gadget::<Gadget>#where()"),
            "a class that inherits nothing is not a model"
        );
    }

    #[test]
    fn the_callbacks_resolve_on_a_model_and_on_nothing_else() {
        // `before_create` is a `def` in activesupport that
        // `define_model_callbacks` wrote at boot, so no file in the workspace declares it, the
        // graph correctly found nothing, and the name rung answered with the only
        // `before_create` anybody's file *does* write —
        // `Fabrication::Schematic::Evaluator#before_create`, in a gem, in a fixture library.
        //
        // The last assertion is the bound, and it is the entry points' shape: a class that is
        // not a model gets none of these, so a gem that really does define one keeps
        // every position it had.
        let (mut harness, _story, _uri) = models_project("");
        let source = "class Widget < ApplicationRecord\n                        after_initialize :a\n                        before_create :b\n                        before_save :c\n                        after_create_commit :d\n                      end\n";
        let widget = harness.write("app/models/widget.rb", source);
        let plain = "class Gadget\n  before_create :b\nend\n";
        let gadget = harness.write("app/lib/gadget.rb", plain);
        harness.watch(&[&widget, &gadget]);

        for name in [
            "after_initialize",
            "before_create",
            "before_save",
            "after_create_commit",
        ] {
            assert!(
                harness.has(&format!("Widget::<Widget>#{name}()")),
                "{name} is not on the model's singleton"
            );
            let found = card(&mut harness, &widget, source, name);
            assert!(
                found.contains("no file declares it"),
                "{name} does not carry the generated provenance: {found}"
            );
            assert!(
                !found.contains("possible definitions"),
                "{name} is still a candidate list: {found}"
            );
        }
        // Nothing is mapped, which is the no-place rule: `define_model_callbacks` is a `def` in a
        // gem that nobody's file wrote, so the honest answer to "where" is nowhere at all.
        assert!(
            harness
                .definition_at(&widget, source, "before_create")
                .is_null(),
            "a declaration this crate invented must not be a place"
        );
        assert!(
            !harness.has("Gadget::<Gadget>#before_create()"),
            "a class that inherits nothing is not a model and gets none of them"
        );
    }

    #[test]
    fn a_receiverless_call_answers_what_the_same_call_on_self_answers() {
        // Asserted as an **equality** rather than as a list of expected types, because the
        // right answer is known before the server is asked. `self.comments.first` resolves;
        // `comments.first`, one word shorter and the same Ruby, reaches the name rung and
        // offers a list unless a receiverless call is read as `self`.
        //
        // Two files rather than two lines of one, because `position_of` takes the first
        // occurrence of a word — and because that is the shape the probe which found this used:
        // the same expression, written twice, with one difference.
        let (mut harness, _story, _uri) = models_project("");
        let explicit_source = "class Widget < ApplicationRecord\n                                 has_many :comments\n                                 def a\n    self.comments.first\n  end\n                               end\n";
        let bare_source = "class Widget\n  def b\n    comments.first\n  end\nend\n";
        let explicit = harness.write("app/models/widget.rb", explicit_source);
        let bare = harness.write("app/models/widget_more.rb", bare_source);
        harness.watch(&[&explicit, &bare]);

        let with_self = card(&mut harness, &explicit, explicit_source, "first");
        let without = card(&mut harness, &bare, bare_source, "first");
        assert!(
            with_self.contains("ActiveRecordRelation#first"),
            "the twin the equality is against has to be the answer it always was: {with_self}"
        );
        assert_eq!(
            without, with_self,
            "an implicit receiver is a `self` the writer did not type, and the two must not \
             answer differently"
        );

        // The widened guard, measured on its own because it is the half that can reach a method
        // the file does not declare. `find(1)` writes an argument, so a guard that refuses
        // arguments makes it `Receiver::Unknown` and ends the chain — the guard is a bound on
        // the *guess* and must not be spent on the lookup as well.
        let with_argument = "class Gizmo < ApplicationRecord\n                               has_many :comments\n                               def self.c\n    find(1).comments.first\n  end\n                             end\n";
        let gizmo = harness.write("app/models/gizmo.rb", with_argument);
        harness.watch(&[&gizmo]);
        let chained = card(&mut harness, &gizmo, with_argument, "first");
        assert!(
            chained.contains("ActiveRecordRelation#first"),
            "a receiverless call that wrote an argument still resolves: {chained}"
        );
        assert!(
            !chained.contains("Matched on the method name alone"),
            "{chained}"
        );
    }

    #[test]
    fn the_collection_predicates_are_on_the_side_rails_puts_them_on() {
        // The collection predicates, both halves. `api_key_scopes.size` is the shape a user hits:
        // an association that types, a relation that exists, and a member of it that nothing
        // declared — so a chain which had already resolved twice fell to the name rung at its
        // third hop and offered 192 possible definitions.
        //
        // The other half is a subtraction, and it is a finding rather than a feature.
        // `ActiveRecord::Querying::QUERYING_METHODS` *is* the class side, and it names
        // neither `each` nor `to_a` — both a `NoMethodError` on a model in Ruby — nor `size`,
        // `length` and `empty?`, which `Relation` defines and nothing delegates. A generated
        // declaration no legal call can reach is the same defect as an inherited class side.
        let source = "Story.first.comments.size
";
        let (mut harness, _story, uri) = models_project(source);

        let counted = card(&mut harness, &uri, source, "size");
        // One copy for the project puts the whole interface onto one class the project's
        // relations inherit, so the card names that rather than `Comment::Relation` — the
        // stated cost, and it is the third hop of a chain that still resolves, which is what the
        // assertion below is about.
        assert!(counted.contains("ActiveRecordRelation#size"), "{counted}");
        assert!(
            !counted.contains("Matched on the method name alone"),
            "the third hop of the chain resolves rather than guessing: {counted}"
        );

        assert!(harness.has("ActiveRecordRelation#empty?()"));
        assert!(
            harness.has("ActiveRecordRelation#any?()"),
            "a predicate hands its block the element, which is receiver-relative now"
        );
        assert!(
            harness.has("Story::<Story>#count()"),
            "`count` is delegated and starts a chain on the model"
        );
        assert!(harness.has("Story::<Story>#exists?()"));
        assert!(
            !harness.has("Story::<Story>#size()"),
            "`QUERYING_METHODS` does not name `size`, and `Story.size` raises"
        );
        assert!(
            !harness.has("Story::<Story>#each()"),
            "nor `each`, which the model class side does not get at all"
        );
        assert!(
            harness.has("ActiveRecordRelation#each()"),
            "the relation keeps every one of them, once for the project"
        );
        assert!(
            !harness.has("Story::Relation#each()"),
            "and declares none of them itself — one copy for the project"
        );
    }

    #[test]
    fn a_relation_is_an_enumerable_and_a_select_chain_does_not_end_at_one() {
        // Rails writes `include Enumerable` in `ActiveRecord::Relation`, and leaving it out
        // costs measurable down-moves: `select` correctly answers a relation, and a relation
        // with nothing of `Enumerable` in it is a **dead end** for the `.index_by` or `.map`
        // that habitually follows one. Four of twelve
        // down-moves were this and nothing else — the first hop made right and the second made
        // impossible.
        let source = "Story.where(id: 1).select(:id).sort\n";
        let (mut harness, _story, uri) = models_project(source);

        let sorted = card(&mut harness, &uri, source, "sort");
        assert!(sorted.contains("Enumerable#sort"), "{sorted}");
        assert!(
            !sorted.contains("Matched on the method name alone"),
            "the hop after a `select` resolves rather than guessing: {sorted}"
        );
        // One `include` for the whole project, on the class every relation inherits.
        let rbs = harness.generated_rbs("app/models/story.rb");
        assert!(
            rbs.contains("class Story::Relation < ActiveRecordRelation\n"),
            "{rbs}"
        );
    }

    /// A model whose superclass chain leaves the application pays for its own class side.
    ///
    /// One copy for the project puts the query interface on the **base**, and the base has
    /// to be a class this pass may declare on. forem writes `Tag < ActsAsTaggableOn::Tag` and
    /// `EmailMessage < Ahoy::Message`, which are real models whose base class is in a gem: the
    /// walk stops at the model itself, and it gets the copy every model used to get. The
    /// alternative — a base ya-lsp invents — does not work at all, because a generated
    /// superclass on a class the user's own file already gives one is silently ignored.
    #[test]
    fn a_model_whose_base_is_not_the_applications_declares_its_own_class_side() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base\nend\n",
        );
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  has_many :comments\n  has_many :tags\nend\n",
        );
        harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\nend\n",
        );
        harness.write(
            "app/models/tag.rb",
            "class Tag < ActsAsTaggableOn::Tag\n  has_many :comments\nend\n",
        );
        harness.index();

        // The application has a base and every model under it inherits one copy.
        assert!(harness.has("ApplicationRecord::<ApplicationRecord>#where()"));
        assert!(!harness.has("Story::<Story>#where()"));
        assert!(!harness.has("Comment::<Comment>#where()"));
        // `Tag` is a collection element rather than a model by the walk — its chain leaves the
        // application at a class in a gem — so it is its own base and pays for its own copy.
        assert!(harness.has("Tag::<Tag>#where()"));
        // And the callbacks travel with it, because they are inherited for the same reason.
        assert!(harness.has("ApplicationRecord::<ApplicationRecord>#before_save()"));
        assert!(harness.has("Tag::<Tag>#before_save()"));
        assert!(!harness.has("Story::<Story>#before_save()"));
    }

    #[test]
    fn the_vocabulary_is_rails_own_list_and_the_call_decides_the_arm() {
        // The table is `ActiveRecord::Querying::QUERYING_METHODS` — all 113 — rather than the
        // names somebody could type a signature for, plus the two places
        // Rails puts a class method that is not in it. What the widening rests on is that a
        // name may still refuse a *type*: `pick` and every `async_*` are declared `untyped`, so
        // the name resolves and the chain stops: the type declines, the name never does.
        let source = "Story.first.comments.pluck(:body)\n";
        let (mut harness, _story, uri) = models_project(source);

        // `create!` is the interesting one: class-side, in `Persistence::ClassMethods`
        // and **not** in `QUERYING_METHODS`, so a table read out of that constant alone could
        // never have had it — and it is on no relation, because no relation answers it.
        assert!(harness.has("Story::<Story>#create!()"));
        // …and `relation.rb` defines it too, which a completion sweep is what catches: five of
        // that block's six names are on both sides, and
        // `instantiate` is the one that is genuinely the class's alone.
        assert!(harness.has("ActiveRecordRelation#create!()"));
        assert!(harness.has("Story::<Story>#instantiate()"));
        assert!(!harness.has("ActiveRecordRelation#instantiate()"));
        // And the traffic runs the other way for the five that raise on a model, which is why
        // the side is a three-way answer rather than a flag.
        assert!(harness.has("ActiveRecordRelation#empty?()"));
        assert!(!harness.has("Story::<Story>#empty?()"));

        // A name Rails names and this crate cannot type is declared anyway. `Story.async_count`
        // resolves to something instead of falling to the name rung; what it hands back is
        // `ActiveRecord::Promise`, which no generator here writes, so the type declines and
        // `Types::harvest` drops it.
        assert!(harness.has("Story::<Story>#async_count()"));
        assert!(harness.has("Story::<Story>#upsert_all()"));

        let plucked = card(&mut harness, &uri, source, "pluck");
        assert!(plucked.contains("ActiveRecordRelation#pluck"), "{plucked}");
        assert!(
            !plucked.contains("Matched on the method name alone"),
            "{plucked}"
        );

        // The arity split, which is the half that needed a new shape in the fact table: one
        // declaration, two arms, and the count at the call site decides. `Array` is what
        // `class_at` reads off the members offered, so this is the answer a user would see.
        assert_eq!(class_at(&mut harness, &uri, "Story.first(3).~"), "Array");
        assert_eq!(class_at(&mut harness, &uri, "Story.pluck(:id).~"), "Array");
        // `select` is the same shape decided by the block instead: with column names it is a
        // query method and with a block it is `Enumerable`'s, reached through `super`. Both
        // arms are asserted, because an overload that answered the block arm for every call
        // would pass a test that only looked at one of them.
        assert_eq!(
            class_at(&mut harness, &uri, "Story.select { |s| s }.~"),
            "Array"
        );
        assert_eq!(
            class_at(&mut harness, &uri, "Story.select(:id).count.~"),
            "Integer",
            "with column names it is still a relation, so the chain runs on through it"
        );
    }

    #[test]
    fn where_never_answers_the_chain_a_keyword_hash_cannot_be_told_from() {
        // A bound stated as a test, because it is a decision rather than an omission. `where` with no argument returns a `QueryMethods::WhereChain`, which is
        // where `not`, `missing` and `associated` live — 843 call sites of `not` in the six
        // corpora. An arity split beside `first`'s is **not expressible** for it: `arity_of` deliberately does not count a keyword hash as a positional
        // argument, so `3.7.round(half: :up)` reaches the zero-argument arm — and so does
        // `Story.where(title: "x")`, the commonest call in Rails. An arm answering `WhereChain`
        // at arity 0 would answer it for that call too.
        //
        // So `where` answers a relation on every arm. The assertion that matters is the second
        // one: the keyword form keeps the answer it has always had.
        let source = "Story.where.not(id: 1)\nStory.where(title: \"x\").first.user\n";
        let (mut harness, _story, uri) = models_project(source);
        assert!(
            !harness.has("Story::Relation#not()") && !harness.has("ActiveRecordRelation#not()"),
            "`not` is `WhereChain`'s and putting it on a relation would make `Story.all.not` \
             resolve, which raises"
        );
        let kept = card(&mut harness, &uri, source, "user");
        assert!(kept.contains("Story#user"), "{kept}");
    }

    #[test]
    fn a_model_that_writes_no_macro_answers_on_its_own_relation_and_not_its_parents() {
        // The defect the per-model class side exists to fix, and the reason the relation set
        // is a **union** rather than a rule about abstract classes. lobsters writes `scope :select_fix` in its
        // `ApplicationRecord`; that made `ApplicationRecord` a collection element, wrote an
        // `ApplicationRecord::Relation`, and put the ten names on a singleton **every model in
        // the application inherits** — so `Category.order(...)` answered a relation of a class
        // no row is ever an instance of, and every chain off it was wrong rather than absent.
        // 107 lobsters positions name `ApplicationRecord` in their card and 64 of them resolve
        // or derive through exactly this.
        //
        // Declining to give an abstract class a relation deletes the wrong answer and supplies
        // nothing: `Category.select_fix` goes with it, because a `scope` declares nothing at
        // all when its class has no relation. That was built, measured at **64** down-moves
        // against the 11 it was meant to repair, and reverted. What fixes it is `Category`
        // owning the ten names itself, so the inherited pair is never reached.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/channel.rb", "module Channel\nend\n");
        harness.write(
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base\n  \
             self.abstract_class = true\n  \
             scope :select_fix, -> { all }\n\
             end\n",
        );
        harness.write(
            "app/models/category.rb",
            "class Category < ApplicationRecord\n  has_many :things\nend\n",
        );
        harness.write(
            "app/models/thing.rb",
            "class Thing < ApplicationRecord\nend\n",
        );
        let source = "Category.order(:id).first.things\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        // **Asserted on the place rather than on the card**, which one copy per project makes
        // necessary and which is the sharper test either way. `order` is declared once on the
        // base, so the card names `ApplicationRecord.order` whatever it
        // returns; what has to be right is the *chain*, and every link of it is the defect:
        // `Category.order` is a `Category::Relation`, its `first` is a `Category`, and only a
        // `Category` has `things`. The defect is each of those answering `ApplicationRecord`
        // instead.
        let things = card(&mut harness, &uri, source, "things");
        assert!(
            things.contains("Category#things"),
            "a model answers its own relation, not the one its abstract parent owns: {things}"
        );
        assert!(
            harness.has("ApplicationRecord::<ApplicationRecord>#select_fix()"),
            "and the parent keeps the scope it really does install on every subclass"
        );
        // The other half, and the receiver-relative return types **reverse** it. Taking the
        // class side off an abstract class is the right rule while the interface names a
        // concrete class, because it is inherited — so a model whose own
        // class side was out of reach for some other reason answered `ApplicationRecord` —
        // chatwoot's `Captain::Assistant.find` is the position that measured it. The two
        // receiver-relative return types fix that at its source rather than by withholding the
        // declaration, so the interface is on the base *deliberately* now and the chain above
        // is what says it is safe. What is left of the trade is stated: `ApplicationRecord.order`
        // resolves and raises in Ruby, on a receiver nobody writes.
        assert!(harness.has("ActiveRecordRelation#order()"));
        assert!(harness.has("ApplicationRecord::<ApplicationRecord>#order()"));
        assert!(
            !harness.has("Category::<Category>#order()"),
            "and no model declares its own copy, which is the declaration count"
        );
    }

    #[test]
    fn the_chain_that_promoted_this_item_lands_on_one_target() {
        // The chain this exists for: `Story.first.comments.first.user.username`, which without
        // the generators answers with a name-based candidate list that merely happens to
        // contain the right target.
        //
        // Five links and four generators: the class side, a `has_many`, the relation it
        // returns, a `belongs_to`, and then a column — with the last two documents reopening
        // `class User` from two different files, which is the arrangement no other test here
        // puts together.
        let source = "Story.first.comments.first.user.username\n";
        let (mut harness, _story, uri) = models_project(source);
        // The shared fixture's `Comment` has no author, and lobsters' does — the link the
        // benchmark's chain turns on.
        let comment = harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\n  belongs_to :story\n  \
             belongs_to :user\n  has_many :comments\nend\n",
        );
        harness.watch(&[&comment]);
        let schema = harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[7.1].define(version: 1) do\n  \
             create_table \"users\", force: :cascade do |t|\n    \
             t.string \"username\", null: false\n  end\nend\n",
        );
        harness.watch(&[&schema]);

        let card = card(&mut harness, &uri, source, "username");
        assert!(card.contains("User#username"), "{card}");
        assert!(!card.contains("guessed from the name"), "{card}");
        assert!(!card.contains("Matched on the method name alone"), "{card}");

        let definition = harness.definition_at(&uri, source, "username");
        assert_eq!(definition.as_array().map(Vec::len), Some(1), "{definition}");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(schema.as_str()),
            "{definition}"
        );
    }
}
