//! The relation class every model gets, and the query interface both sides share.
//!
//! Nothing here reads a macro; [`models`](super::models) does that. This is what a collection *is*
//! once read: `Comment::Relation`, a class this crate writes and no file declares, plus the
//! vocabulary ActiveRecord puts on it and on the model's class object.
//!
//! One module, because it is one argument:
//!
//! - **A relation class is empty.** Every name on it comes from [`RELATION_BASE`], written once per
//!   project, so the member count does not grow with the model count.
//! - **The class side is the same list on the model's own base.** [`query_interface`] makes the
//!   sharing possible: it knows what each name returns without knowing which model asked.
//! - **The only relation member with its own place is a `scope`**, which is why [`Chained`] lives
//!   here and not beside the macro it is read from.

use crate::analysis::types::{COLLECTION, ELEMENT};
use crate::generated::{Declared, Facts, Owner, Source};

/// The class ya-lsp writes for a collection of `class`.
///
/// Nested under the model (`Comment::Relation`, not `CommentRelation`), for three reasons, most
/// important first:
///
/// 1. The name is *scoped*, so it cannot collide with an unrelated top-level constant.
/// 2. It reads right where a user meets it: a hover card saying `Comment::Relation#first`.
/// 3. A project that already has a `Comment::Relation` meant something by it, so a collision makes
///    the pass emit nothing instead of shadowing it.
#[must_use]
pub fn relation_of(class: &str) -> String {
    format!("{class}::{RELATION}")
}

/// The relation-side half of every `scope` one document writes, held back until the document ends.
///
/// # A scope is a class method and also a relation method
///
/// `ActiveRecord::Delegation` builds a module per relation class holding every scope the model
/// defines, which is what makes `Story.recent.visible.limit(10)` ordinary Ruby. Declared on the
/// model's singleton alone, a scope answers only the **first** call of a chain, and every word
/// after it falls to the name-based list.
///
/// The relation copy carries the same span, so `Story.recent.visible` jumps to the `scope :visible`
/// line just as `Story.visible` does. It is the only member of a relation class with a place;
/// everything else is the query interface, which no file declares (see [`relation`]).
///
/// # Why it is held back
///
/// [`Facts`] renders in the order it was told and reopens a body each time the owner changes.
/// Declaring `Story.recent` and `Story::Relation#recent` alternately would write one
/// `class Story::Relation ... end` per scope. Held to the end, each relation class opens once, in
/// the same body as its superclass line if this document writes one.
///
/// [`flush`](Self::flush) sorts by owner, so a concern whose scopes fan onto several includers
/// still makes one run per relation class. The sort is **stable**, so within a class the macros
/// keep their written order, which [`Facts`]' collision rule reads.
#[derive(Default)]
pub(super) struct Chained(Vec<Declared>);

impl Chained {
    /// Say one `scope` on the class object now, and hold its relation copy back.
    ///
    /// Every caller goes through here (a `scope` on a class, the same `scope` fanned onto a
    /// concern's includers, and an `enum`'s class-side pair, which is a `scope` Rails writes
    /// itself), so none can drift from the others.
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

/// The class a relation is a collection of: [`relation_of`] read backwards.
///
/// Needed at *lookup* time, not generation time: `Story::Relation#first` is declared once for the
/// whole project, so only the receiver's name says which model the answer is about. See
/// [`Return::Element`](crate::analysis::types::Return::Element).
///
/// A name that is not a relation answers `None`, not itself: the caller's next question is "what is
/// that model's relation", and a wrong answer here would invent a class.
#[must_use]
pub fn element_of(relation: &str) -> Option<&str> {
    relation.strip_suffix(RELATION)?.strip_suffix("::")
}

/// The last segment of the name [`relation_of`] builds, and the one [`element_of`] takes off.
const RELATION: &str = "Relation";

/// The class every relation ya-lsp writes inherits from, and where the query interface lives.
///
/// **One copy for the project.** Many signatures name what the collection holds, which would force
/// one copy per element type. Two receiver-relative return types remove that:
/// [`Return::Element`](crate::analysis::types::Return::Element) means "the model this receiver is
/// about" and [`Return::Collection`](crate::analysis::types::Return::Collection) means "that
/// model's relation". So `def first: () -> ActiveRecordElement?`, written **once**, answers `Story`
/// on `Story::Relation` and `Comment` on `Comment::Relation`. Each model's document then holds one
/// line: `class Story::Relation < ActiveRecordRelation end`.
///
/// **A superclass, not an `include`.** A superclass carries both sides, because a class object's
/// singleton chain follows the class chain. A module could serve only the instance side.
///
/// **It works because nothing else declares `Story::Relation`.** A generated superclass on a class
/// whose own file already names one is **silently ignored**: an RBS `class Widget < SpikeBase`
/// beside a Ruby `class Widget < ApplicationRecord` leaves `SpikeBase`'s members unreachable, with
/// no error. That is why the class side inherits through the model's **own** base (see
/// [`class_side`]), not through one this crate invents.
///
/// Top-level for [`ROUTE_HELPERS`](crate::workspace::rails::ROUTE_HELPERS)' reason, with the same
/// collision rule: a project that already declares this name means something by it, and the pass
/// writes nothing.
///
/// **The cost is the hover card.** `story.comments.where(...)` shows `ActiveRecordRelation#where`,
/// not `Comment::Relation#where`. The element is gone from the card but still in the *answer*,
/// which is what a reader chains off.
pub const RELATION_BASE: &str = "ActiveRecordRelation";

/// Where Rails itself writes the relation half of the query interface.
///
/// **One name, not ten.** `relation.rb` writes
/// `include FinderMethods, Calculations, SpawnMethods, QueryMethods, Batches, Explain, Delegation`,
/// and rubydex has walked it, so an ancestor walk from this class *is* Ruby's method lookup and
/// gives `Method#owner`'s answer. Checked against `Method#source_location`: every name this file
/// declares that resolves lands on the line Ruby names. The exceptions are `instantiate` (class
/// side only) and `default_order` (not in a released Rails).
///
/// Ordered and searched in order for [`RAILS_CLASS_SIDE`]'s sake, which needs three names before
/// this one.
pub const RAILS_RELATION: [&str; 1] = ["ActiveRecord::Relation"];

/// The class half, which Ruby answers almost entirely with one line.
///
/// `Story.where` is `delegate(*QUERYING_METHODS, to: :all)` in `querying.rb`, and **no reader can
/// expand a splatted constant into ninety method names**, so `ActiveRecord::Querying` holds no
/// `def` for the graph to find. What remains: the ten names Rails does write a class-side `def`
/// for, owned by the three modules here, and the rest, which take the relation's because that is
/// exactly what the `delegate` line says (`Story.where` is `Story.all.where`). All ten class-side
/// `def`s land where `Method#source_location` says.
///
/// `ActiveRecord::Base`'s own singleton is deliberately **not** on this list, although Ruby
/// searches it first. ya-lsp writes the class side onto that singleton, so looking there would find
/// this crate's own place-less declaration and stop, and the ten names that have a place would lose
/// it.
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

/// Where ActiveRecord puts a name: the whole reason [`Query`] carries a side.
///
/// `ActiveRecord::Querying::QUERYING_METHODS` is the class side **by construction**:
/// `delegate(*QUERYING_METHODS, to: :all)` is the line that makes `Story.where` mean
/// `Story.all.where`. So membership is read out of Rails, not assumed. A name it does not hold
/// exists on the relation and **raises on the model**. It also runs the other way: `instantiate` is
/// a class method no relation answers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    /// In `QUERYING_METHODS`: the relation defines it and the model delegates to `all`.
    Both,
    /// `Relation`'s own: on the model it is a `NoMethodError` or somebody else's method. `size`,
    /// `length`, `empty?`, `to_a` and `each` are relation-only, which keeps the last two off the
    /// singleton. `new` is the sixth, for another reason: a model answers it from `Class`, and a
    /// class-side declaration would shadow that.
    Relation,
    /// `Persistence::ClassMethods`', and nothing on a relation answers it.
    ///
    /// **One name.** It is easy to arrive at six, but `relation.rb` also defines `create`,
    /// `create!`, `update`, `update!` and `build` (an alias of `new`), so `story.comments.create!`
    /// really is a call. `instantiate` is the one `Relation` does not define.
    Class,
}

/// Every name in `ActiveRecord::Querying::QUERYING_METHODS` that hands back a relation.
///
/// One list, not one entry each, because the name is the whole row: these take anything and return
/// the relation, which is what makes a query chainable. `with` is on both sides like the delegated
/// names: `QueryMethods#with` is on the relation and `Querying#with` is a `def` beside the list.
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
/// Rails writes them one by one in `FinderMethods`, and so does this: `second` through `fifth`,
/// `forty_two` (a long-standing joke, and a real method) and the two counted from the end. `first`,
/// `last` and `take` are **not** here: each takes an optional count that changes what it returns,
/// which is [`Query::overloads`]' job.
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

/// The names that return a `Promise` and nothing else: `ActiveRecord::Promise`, resolved by
/// `#value`, a class no generator here writes.
///
/// Declared anyway, as `untyped`, because this table's bound is `QUERYING_METHODS`, not a judgement
/// about which of its names deserve a place. The type declines and
/// [`Types::harvest`](crate::analysis::types::Types::harvest) drops an `untyped`. Each costs a
/// completion entry, and each buys `Story.async_count` resolving instead of falling to the name
/// rung.
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
/// Each entry is a signature *without* its `def`, because most facts are true twice: as instance
/// methods on the relation class and as class methods on the model's singleton. One list makes "the
/// singleton and the relation agree on what `where` returns" a property of this file, not of
/// memory.
///
/// # The bound is Rails' own list
///
/// A table chosen for being *typeable* is a bound nobody can check. **The list is
/// `QUERYING_METHODS`**, plus the two places Rails puts a class method outside it:
/// `Persistence::ClassMethods` (where `create!` lives) and `Querying#with`. A name is here because
/// Rails put it on a model, not because ya-lsp could type it; that is why the async family and the
/// bulk writers are here.
///
/// **The type can still be refused**, which keeps the width safe. `pick`, `calculate`, `minimum`,
/// `maximum`, every `async_*` and every bulk writer return `untyped`: the name resolves, the chain
/// stops, and [`Types::harvest`](crate::analysis::types::Types::harvest) drops the claim instead of
/// carrying a wrong one.
///
/// # `Enumerable`'s names, instantiated
///
/// [`relation_base`] writes Rails' own `include Enumerable`, so every name in that module **already
/// resolves** on a relation. But its signatures are written in `E`, a type variable, and `class_of`
/// refuses a type variable, so the block a reader just wrote gets nothing.
/// `story.comments.map { |row| ... }` is the shape.
///
/// So the second half of this table is `Enumerable`'s list with the element in `E`'s place, read
/// off `vendor/rbs/core/enumerable.rbs`. A row exists **only where instantiating changes the
/// answer**:
///
/// - **A block handed an element**: `map`, `reject`, `sort_by`, `each_with_object`, `group_by` and
///   the rest.
/// - **A return that is the element itself**: `detect`, `min_by`, `max`.
///
/// **`Array[E]` is not one of them.** `Array[E]` and `Array[ActiveRecordElement]` both answer
/// `Array`, so `entries`, `compact`, `drop` and `tally` are left to the `include`; copying them
/// would shadow a declaration that has a place with one that does not. The nine names ActiveRecord
/// defines itself (`count`, `find`, `first`, `take`, `to_a`, `sum`, `any?`, `none?`, `one?`) are
/// above and not copied: Ruby's lookup prefers the class's own over the module's, and so does this
/// table.
///
/// Every row is [`Side::Relation`]: a model's class object reaches `Class` and `Object` but no
/// `Enumerable`, so `Story.map` raises where `Story.all.map` does not.
///
/// # The approximations
///
/// - **A scalar or an array in one argument.** `find`, `create`, `create!`, `build`, `instantiate`
///   and `destroy` return a record for a scalar and an `Array` for an array, both with **one**
///   positional argument, so [`Arity`](crate::analysis::cursor::Arity) cannot tell them apart. Each
///   types the singular. `update` and `update!` return `untyped`: their first parameter *defaults
///   to `:all`*, so the array is not even unlikely.
/// - **`count` after a `group` is a `Hash`**, but this always says `Integer`: the same inexactness
///   as `where` always returning a relation.
/// - **`pluck` and `ids` are `Array[untyped]`**, not `Array[Element]`: `Story.pluck(:title)` is an
///   array of *columns*.
///
/// # What `where` cannot say
///
/// `where` with **no argument** returns a `QueryMethods::WhereChain` (home of `not`, `missing` and
/// `associated`). An arity split like `first`'s **cannot express** this: `arity_of` does not count
/// a keyword hash as a positional argument (so `3.7.round(half: :up)` reaches the zero-argument
/// arm), which makes `where()` and `where(title: "x")` the same call here. A `WhereChain` arm at
/// arity 0 would answer the commonest call in Rails. So `where` returns a relation on every arm,
/// `WhereChain` is not generated, and `where.not` stays on the name rung.
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
        // `Enumerable#select` reached through `super`: the block is handed an element and the
        // result is an `Array` of them, not a relation. The block is *required*, which puts this
        // arm on the other side of the partition from the one above.
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
    // No block arm: `Enumerable#sum` with a block returns whatever the block summed, while a
    // relation's own `sum` is a number. Stating only the blockless arm makes `Story.sum { ... }`
    // answer nothing instead of something wrong.
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

    // `Relation`'s own, which raise on the model. This is why [`Side`] exists instead of a flag
    // that could only subtract.
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

    // `relation.rb` has `def reload; reset; load; end`, and `load` returns `self`, so a reloaded
    // relation is the relation. [`Side::Relation`] like the four above: `ActiveRecord::Base#reload`
    // is an *instance* method, so `Story.reload` reaches `Class`, finds nothing and raises.
    // `CollectionProxy` overrides it and also returns the receiver, so one row is right for both.
    queries.push(plain(
        "reload",
        "()".to_owned(),
        relation.clone(),
        Side::Relation,
    ));

    // `Persistence::ClassMethods`: names not in `QUERYING_METHODS` at all. **Five of these six are
    // on the relation too**, per `relation.rb`; see [`Side::Class`].
    for name in ["create", "create!", "build"] {
        queries.push(both(name, taking_element(), element.to_owned()));
    }
    // `relation.rb` has `alias build new`, the *same method*, so declaring one without the other
    // would be incoherent. It is the relation's alone because a model gets `new` from `Class`,
    // which a class-side declaration would shadow.
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

    // `Enumerable`, instantiated — and every row here is [`Side::Relation`], because a model's
    // class object reaches `Class` and `Object` and no `Enumerable` at all.
    let on_relation = |name, parameters: String, returns: String| {
        plain(name, parameters, returns, Side::Relation)
    };
    let yielding = || format!("() {{ ({element}) -> untyped }}");
    let maybe_yielding = || format!("() ?{{ ({element}) -> untyped }}");
    let comparing = || format!("() ?{{ ({element}, {element}) -> untyped }}");
    let grouped = format!("Enumerator[Array[{element}]]");
    let ends = format!("[{nilable}, {nilable}]");

    // What the block made, one per element.
    for name in ["map", "collect", "flat_map", "collect_concat", "filter_map"] {
        queries.push(on_relation(name, yielding(), "Array[untyped]".to_owned()));
    }
    // The ones that hand back some of the elements they were given. `select` is not here: it is
    // ActiveRecord's own above, declared with this exact block arm, and `filter` is the alias
    // Rails does not override.
    for name in [
        "find_all",
        "filter",
        "reject",
        "drop_while",
        "take_while",
        "sort_by",
    ] {
        queries.push(on_relation(name, yielding(), records.clone()));
    }
    queries.push(on_relation("uniq", maybe_yielding(), records.clone()));
    queries.push(on_relation("sort", comparing(), records.clone()));

    // One element, or none.
    queries.push(on_relation(
        "detect",
        format!("(*untyped) {{ ({element}) -> untyped }}"),
        nilable.clone(),
    ));
    for name in ["min_by", "max_by"] {
        queries.push(on_relation(name, yielding(), nilable.clone()));
    }
    for name in ["min", "max"] {
        queries.push(on_relation(name, comparing(), nilable.clone()));
    }
    queries.push(on_relation("minmax", comparing(), ends.clone()));
    queries.push(on_relation("minmax_by", maybe_yielding(), ends));

    // The blocks handed an element that hand something else back.
    queries.push(on_relation(
        "group_by",
        yielding(),
        format!("Hash[untyped, Array[{element}]]"),
    ));
    queries.push(on_relation(
        "partition",
        yielding(),
        format!("[{records}, {records}]"),
    ));
    queries.push(on_relation(
        "to_h",
        maybe_yielding(),
        "Hash[untyped, untyped]".to_owned(),
    ));
    queries.push(on_relation("all?", taking_element(), "bool".to_owned()));
    queries.push(on_relation(
        "find_index",
        format!("(*untyped) ?{{ ({element}) -> untyped }}"),
        "Integer?".to_owned(),
    ));
    for name in ["grep", "grep_v"] {
        queries.push(on_relation(
            name,
            format!("(untyped) ?{{ ({element}) -> untyped }}"),
            "Array[untyped]".to_owned(),
        ));
    }
    // The memo comes first and the element second: the one row whose element is not at position
    // zero. Stated as the arm with an initial value: the arm without one hands the block two
    // elements, and two arms that disagree about a block are refused together, so this states only
    // the position that is an element in both.
    for name in ["inject", "reduce"] {
        queries.push(on_relation(
            name,
            format!("(*untyped) {{ (untyped, {element}) -> untyped }}"),
            "untyped".to_owned(),
        ));
    }
    queries.push(on_relation(
        "each_with_object",
        format!("(untyped) {{ ({element}, untyped) -> untyped }}"),
        "untyped".to_owned(),
    ));

    // And the walks, which hand back what they walked over.
    queries.push(on_relation(
        "each_with_index",
        format!("() {{ ({element}, Integer) -> untyped }}"),
        relation.clone(),
    ));
    queries.push(on_relation(
        "each_entry",
        maybe_yielding(),
        relation.clone(),
    ));
    queries.push(on_relation("reverse_each", yielding(), "void".to_owned()));
    queries.push(on_relation(
        "cycle",
        format!("(*untyped) {{ ({element}) -> untyped }}"),
        "NilClass".to_owned(),
    ));

    // The four that cut the walk into runs. Two are handed a pair and two a single element,
    // and all four hand back an enumerator of arrays however they were called.
    for name in ["chunk_while", "slice_when"] {
        queries.push(on_relation(
            name,
            format!("() {{ ({element}, {element}) -> untyped }}"),
            grouped.clone(),
        ));
    }
    for name in ["slice_after", "slice_before"] {
        queries.push(on_relation(
            name,
            format!("(*untyped) ?{{ ({element}) -> untyped }}"),
            grouped.clone(),
        ));
    }
    queries.push(on_relation(
        "chunk",
        yielding(),
        format!("Enumerator[[untyped, Array[{element}]]]"),
    ));

    queries
}

/// Where each of [`callback_names`]' four groups comes from, for the provenance line.
///
/// Named, not described: "which callbacks exist" has a file that answers it, and the card should
/// name that file.
const ONLY_AFTER: &str = "`define_model_callbacks :initialize, :find, :touch, only: :after`";
const EVERY_PREFIX: &str = "`define_model_callbacks :save, :create, :update, :destroy`";
const VALIDATION: &str = "`ActiveModel::Validations::Callbacks`";
const TRANSACTION: &str = "`ActiveRecord::Transactions`";

/// Every class-side callback registrar ActiveRecord installs on a model, and what installed it.
///
/// A convention with **no macro behind it**. `before_create` is a `def` that activesupport's
/// `define_model_callbacks` writes at boot, so no workspace file declares it and the graph finds
/// nothing. Without this, the name rung answers with an unrelated gem's `before_create`.
///
/// **Rails' own four call sites, walked, not remembered**: a table written from the docs goes stale
/// silently. That gives **twenty-three** names, not the thirty a `before`/`around`/`after` × ten
/// events rule would give. Seven of those thirty do not exist, and Ruby raises on each:
///
/// - `initialize`, `find` and `touch` are `only: :after`.
/// - `ActiveModel::Validations::Callbacks` writes `before_validation` and `after_validation` by
///   hand; there is no `around_validation`.
/// - `commit` and `rollback` are not `define_model_callbacks` calls: `ActiveRecord::Transactions`
///   writes six `def`s, four of them the `after_*_commit` shortcuts.
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

/// The callbacks, on the singleton of one base class.
///
/// **Nothing here is mapped** (the no-place rule), so this cannot make a jump worse: no line of
/// user code wrote these methods, so the honest answer to "where" is nowhere. What it removes is a
/// jump into an unrelated gem.
///
/// `(*untyped)` because a callback takes symbols, a condition hash, or neither; `-> void` because
/// nobody chains off one. The optional block is handed the **record** (`ActiveSupport::Callbacks`'
/// behaviour for a proc with an argument), so `before_save { |story| ... }` types `story`, and a
/// call without a block reaches the same arm.
///
/// **Declared on the base and inherited**, like [`class_side`]: one copy on `ApplicationRecord`
/// answers for every model under it. The block parameter is
/// [`Return::Element`](crate::analysis::types::Return::Element), so `story` still types as the
/// model the call was written on, not as the base.
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
/// No type parameter anywhere, the decision every collection rests on: ya-lsp writes this text, so
/// it never writes an `ActiveRecord::Relation[Comment]` it cannot instantiate. The price is one
/// class per element type and a name that must not collide; the caller holds both.
///
/// **Nothing written here is mapped.** No line of code declares `Comment::Relation#first`, so
/// [`Origin::Unknown`](crate::analysis::synthesized::Origin) is the honest answer: it types a chain
/// and is never a jump target. Pointing at one of four `has_many :comments` would be confidently
/// wrong.
///
/// A **scope** is the one mapped thing on a relation class, and it is not written here: a file
/// really declares `Comment::Relation#recent` on the same line as `Comment.recent`, so [`Chained`]
/// puts the span on both.
///
/// One comment for the whole class, because the *class* is what ya-lsp invented: its name already
/// tells a reader nobody wrote it. [`class_side`] cannot say that and does not try.
pub(super) fn relation(facts: &mut Facts, element: &str) {
    let owner = Owner::Instance(relation_of(element));
    facts.note(
        owner.clone(),
        format!("A collection of `{element}`. ya-lsp writes this class; no file declares it."),
    );
    // That is all. Every query-interface member is on [`RELATION_BASE`], written once, because the
    // receiver-relative return types keep the element out of signatures. A relation class gets only
    // the scopes [`Chained`] writes. One of those may open the body first: a document writing a
    // scope onto a relation it was not asked to *emit* opens the class with no superclass, and the
    // two merge like a reopened Ruby class.
    //
    // This is never reached with a superclass the pass did not write: if a project declares
    // [`RELATION_BASE`] itself, `knowledge::rails` withdraws every relation instead of letting one
    // inherit whatever the user meant.
    facts.inherits(owner, RELATION_BASE.to_owned());
}

/// The query interface, written once for the whole project.
///
/// [`RELATION_BASE`] has the argument. Every relation class inherits from this one and declares
/// nothing itself, so the interface costs *one* copy per project however many models it has.
///
/// **`include Enumerable`**, Rails' own line (`ActiveRecord::Relation` includes it). Without it,
/// `User.where(...).select(:id)` answers a relation that is a **dead end** for the `.index_by` or
/// `.map` that usually follows. It goes on the class every relation inherits: one `include` per
/// project.
///
/// Names go on in [`query_interface`]'s order, skipping the ones only `Persistence` has. [`Side`]
/// says which; `instantiate` is the only one skipped.
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

/// The delegated half of the same list, on a class object, so a chain can *start*.
///
/// `Story.recent.first.title` works off a macro alone, but `Story.first.title` needs this: nothing
/// declares `first`, `where` or `find` on a model, because that half of the query interface has no
/// macro to read. No new convention and no new reading: [`query_interface`] already knows what each
/// name returns.
///
/// # On the **base** class
///
/// `base` is the topmost class of the model's own superclass chain that the application declares,
/// or, where the bundle is indexed, `ActiveRecord::Base` itself, where Rails really installs the
/// interface. `knowledge::rails::base_of` is the walk. Ruby follows a class object's singleton
/// chain up the class chain, so one copy on the base answers for every model under it: the project
/// pays per *base*, not per model.
///
/// **An invented base would be simpler, and does not work.** A generated
/// `class Story < ActiveRecordModel` is silently ignored wherever the user's file already writes a
/// superclass, which is every Rails model (see [`RELATION_BASE`]; it is safe there only because
/// nothing else declares a relation class). So the inheritance must be the one the application
/// wrote.
///
/// # Why inheriting is safe
///
/// An inherited interface naming a *concrete* class would answer `Category.order(...)` with a
/// relation of a class no row belongs to. The receiver-relative returns fix that: `Category.order`
/// reads [`Return::Collection`](crate::analysis::types::Return::Collection) and answers
/// `Category::Relation`, and `Captain::Assistant.find` answers `Captain::Assistant`.
///
/// The remaining trade: **`ApplicationRecord.where` resolves here and raises in Ruby.** It is the
/// same kind of wrongness as the callbacks on an abstract class, and it costs a name on a receiver
/// nobody writes. The alternative is a wrong *type* on receivers everybody writes.
///
/// **Nothing here is mapped** (the no-place rule): no line of code declares `Story.where`.
pub(super) fn class_side(facts: &mut Facts, base: &str) {
    for query in query_interface() {
        let because = match query.side {
            Side::Relation => continue,
            // Deliberately short. This sentence sits above **every** class-side declaration, so its
            // length dominated the generated RBS. It may not disappear: `class ApplicationRecord`
            // is the user's own class, and a generated `def self.pluck` with nothing above it reads
            // as something their file declared. A relation class needs none, because the *class* is
            // what ya-lsp invented, an argument this side cannot make.
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

    /// [`relation_of`] and [`element_of`] are one mapping, and it must be invertible.
    ///
    /// The interface is declared once per project, so only the receiver's **name** says which model
    /// an answer is about. A non-relation name answers `None`, not itself, because the caller's
    /// next question would build a class out of it.
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

    /// One copy per project, checked on the table, not on a document.
    ///
    /// **No interface signature names a concrete application class**, which is the whole mechanism:
    /// where it would name `Story` it names [`ELEMENT`], and where it would name `Story::Relation`
    /// it names [`COLLECTION`]. Checked on every row, so a row added later that spells an element
    /// (and would need a copy per model again) fails here.
    #[test]
    fn no_signature_in_the_interface_names_what_the_collection_holds() {
        let queries = query_interface();
        // And how many rows do it: what [`RELATION_BASE`] would cost per model if the
        // receiver-relative returns were removed. A tripwire, since nothing else would notice that
        // number going stale.
        let naming_the_element = queries
            .iter()
            .filter(|query| {
                let mut text = format!("{} {}", query.parameters, query.returns);
                for (parameters, returns) in &query.overloads {
                    text.push_str(&format!(" {parameters} {returns}"));
                }
                text.contains(ELEMENT)
            })
            .count();
        assert_eq!(naming_the_element, 90, "of {} rows", queries.len());
        let named = |want: &str| {
            queries
                .iter()
                .find(|query| query.name == want)
                .unwrap_or_else(|| panic!("{want}"))
        };
        // The three places an element can be named, each receiver-relative.
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
        // A relation class is a superclass line and a note; every member is on `RELATION_BASE`,
        // written once for the project.
        assert!(
            declarations.rbs.contains(&format!(
                "class Comment::Relation < {RELATION_BASE}\n  # A collection of `Comment`."
            )),
            "{}",
            declarations.rbs
        );
        // What this file's own macros declared, and not one more: the class side and the callbacks
        // are on the base, which this document was not asked to write. Eight readers, four more for
        // each of the three singular associations that name a class, three more for each of the
        // three collections, the polymorphic one's writer, and the `scope`'s second home on
        // `Story::Relation` (see [`Chained`]).
        assert_eq!(declarations.methods, 8 + 3 * 4 + 3 * 3 + 1 + 1);
        assert_eq!(declarations.spans.len(), 8 + 3 * 4 + 3 * 3 + 1 + 1);
        // `Story`, the `Story::Relation` the scope is chained onto, and the `Comment::Relation`
        // this caller asked for. The third shows that a relation class gets members here whether or
        // not this document writes its superclass.
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
        // Rails' own line. Without it, a relation is a dead end for the `.index_by` or `.map` that
        // follows a `select`.
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
        // The count matters more than the spelling: a reader can check spellings against Rails, but
        // a wrong reading silently changes the count. A `before`/`around`/`after` × ten events rule
        // gives thirty; Rails installs **twenty-three**, and Ruby raises on each of the missing
        // seven.
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
        // Both halves come from one list, so what matters is that both sides agree on every name
        // they share: `Story.where` and `Story.all.where` are one method reached two ways.
        //
        // The list also says which side each name is on, checked both ways here: a name
        // `QUERYING_METHODS` does not delegate must be on the relation and **not** on the class
        // side, because `Story.each` raises in Ruby.
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
        // A tripwire on the bound: `ActiveRecord::Querying::QUERYING_METHODS` counted, plus
        // `Querying#with`, plus the five `Persistence::ClassMethods` names that `relation.rb` also
        // defines. A name added without a line of Rails behind it moves this number and must say
        // which file it read.
        assert_eq!(
            on_both, 119,
            "QUERYING_METHODS, `with`, and `relation.rb`'s five"
        );
        assert_eq!(
            relation_only, 46,
            "`Relation`'s own — the five that raise on the model, `new`, which `relation.rb` aliases `build` to, \
             `reload`, and `Enumerable`'s 39"
        );
        assert_eq!(
            class_only, 1,
            "`instantiate`, which `Relation` does not define"
        );
        // Every class-side declaration carries its own provenance, because
        // `class ApplicationRecord` is the user's own class and a note on *it* would read as a
        // claim about their file. The relation base carries one note for the whole class, an
        // argument this side cannot make.
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
        // The base, never the model: `Story.where` is inherited, so one copy answers for every
        // model. This file writes `class Story` and its macros name `Comment`; the interface is on
        // neither, only in the base's body.
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

    /// `Enumerable`'s half of the interface: what instantiating `E` buys.
    ///
    /// Both shapes a row exists for. The block parameter is `row`, not `comment`, on purpose: a
    /// name that camelizes onto a class is answered by the guess rung whatever the signature says,
    /// so `|comment|` would pass with the feature removed.
    #[test]
    fn a_block_over_a_relation_is_handed_the_model() {
        // A block handed an element.
        let source = "Story.new.comments.map { |row| row.story }\n";
        let (mut harness, _dump, uri) = models_project(source);
        let mapped = card(&mut harness, &uri, source, "story");
        assert!(mapped.contains("Comment#story"), "{mapped}");
        assert!(
            !mapped.contains("Matched on the method name alone"),
            "{mapped}"
        );

        // And a return that is the element itself.
        let source = "Story.new.comments.detect { |one| one }.story\n";
        let (mut harness, _dump, uri) = models_project(source);
        let found = card(&mut harness, &uri, source, "story");
        assert!(found.contains("Comment#story"), "{found}");
        assert!(
            !found.contains("Matched on the method name alone"),
            "{found}"
        );
    }

    /// A name the interface copies from `Enumerable` is offered **once**.
    ///
    /// The relation has both its own declaration and the module's (through Rails' `include`). A
    /// member walk that did not hide the second behind the first would list each copied name twice
    /// in completion and push the wanted name down. Ruby's lookup hides it, and so does the walk
    /// this reads.
    #[test]
    fn a_name_the_interface_copies_out_of_enumerable_is_offered_once() {
        let (mut harness, _dump, uri) = models_project("");
        let answer = harness.complete(&uri, "Story.new.comments.~\n");
        let (labels, precise) = offered(&answer);
        assert!(
            precise,
            "the receiver is a relation, not name-matched: {labels:?}"
        );
        assert_eq!(
            labels.iter().filter(|label| *label == "map").count(),
            1,
            "{labels:?}"
        );
        assert_eq!(
            labels.iter().filter(|label| *label == "entries").count(),
            1,
            "the one the interface leaves to the `include` is there exactly once too: {labels:?}"
        );
    }

    #[test]
    fn what_the_relation_class_is_called() {
        assert_eq!(relation_of("Comment"), "Comment::Relation");
        assert_eq!(relation_of("Admin::Setting"), "Admin::Setting::Relation");
    }

    #[test]
    fn a_collection_chains_through_a_relation_class_that_no_file_declares() {
        // The relation class's whole point: `story.comments` is a relation and `.first` is a
        // `Comment`. `Comment` has no columns here, so the chain goes one link further:
        // `.first.story` is a `Story` again, through the `belongs_to`, reached via a class this
        // pass invented.
        let source = "Story.new.comments.first.story\n";
        let (mut harness, _story, uri) = models_project(source);

        assert!(harness.has("Comment::Relation"));
        let card = card(&mut harness, &uri, source, "story");
        assert!(card.contains("Comment#story"), "{card}");
    }

    #[test]
    fn a_model_answers_the_query_interface_on_its_own_class() {
        // Without a class side, nothing declares `first`, `where` or `find` on the model *itself*,
        // so a chain could be followed but never started: `Story.recent.first.user` would work off
        // a macro alone and `Story.first.user` would not.
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
        // `Story.recent.visible.first` is ordinary Rails: `ActiveRecord::Delegation` builds a
        // module per relation class holding every scope. Declared on the class object alone, the
        // first hop resolves and every later hop falls to the name-based list.
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
        // A concern's `scope` is declared on each includer and lives in the *concern's* document.
        // The includer's relation class is written where it was first asked for, which for a model
        // no macro collects is the model's own document. So `Poll::Relation` is opened in
        // `poll.rb`'s document with its superclass and reopened in `expireable.rb`'s with a member:
        // the one shape where a member and its class come from two generators that never see each
        // other.
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
        // No file holds a `def` for `before_save`: `define_model_callbacks` writes it at run time.
        // The concern edge must not invent one, and the generated callback has no place, so
        // `definition` answers nothing.
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
        // The arity partition, on text this crate wrote instead of `vendor/rbs`. `find` requires an
        // argument and `find_by` does not, so `Story.find(1)` is a `Story` and `Story.find` is
        // nothing. The arm that would have answered is one no call reached, which is why a
        // generated signature is safe to write.
        let source = "Story.find(1).user\n";
        let (mut harness, _story, uri) = models_project(source);
        let found = card(&mut harness, &uri, source, "user");
        assert!(found.contains("Story#user"), "{found}");
        assert!(
            !found.contains("Matched on the method name alone"),
            "{found}"
        );

        // `find_by` is declared `Story?`, and an optional takes its inner type, so the chain off it
        // is the same. `types.md` lists this as the one inexact entry.
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
        // The no-place rule, stronger. A relation's members could at least point at one of the
        // `has_many`s that asked for the class; `Story.where` can point nowhere, because no line of
        // code declares it.
        let source = "Story.first\n";
        let (mut harness, _story, uri) = models_project(source);
        assert!(
            harness.definition_at(&uri, source, "first").is_null(),
            "a declaration this crate invented must not be a place"
        );
    }

    #[test]
    fn every_model_answers_the_query_interface_and_nothing_else_does() {
        // The bound is the superclass chain: ActiveRecord answers `where` on a model because it is
        // a model, and `has_many` has nothing to do with it. Declaring the names on *everything* is
        // how a convention table starts being wrong, which the last assertion guards.
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
        // `before_create` is a `def` that `define_model_callbacks` writes at boot, so no workspace
        // file declares it. Without this, the graph finds nothing and the name rung answers with
        // the only `before_create` anybody's file writes: a fixture library's, in a gem.
        //
        // The last assertion is the bound, shaped like the entry points': a class that is not a
        // model gets none of these, so a gem that really defines one keeps its answers.
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
        // Asserted as an **equality**, because the right answer is known before asking.
        // `self.comments.first` resolves; `comments.first` is the same Ruby and must answer the
        // same, which it does only if a receiverless call is read as `self`.
        //
        // Two files, not two lines of one, because `position_of` takes a word's first occurrence.
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

        // The widened guard, checked alone because it is the half that can reach a method the file
        // does not declare. `find(1)` writes an argument, so a guard that refused arguments would
        // make it `Receiver::Unknown` and end the chain. The guard bounds the *guess*, not the
        // lookup.
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
        // The collection predicates, both halves. `api_key_scopes.size` is what a user hits: an
        // association that types, a relation that exists, and a member nothing declared, so a chain
        // that had resolved twice fell to the name rung at the third hop.
        //
        // The other half is a subtraction. `ActiveRecord::Querying::QUERYING_METHODS` *is* the
        // class side, and it names neither `each` nor `to_a` (both a `NoMethodError` on a model),
        // nor `size`, `length` and `empty?`, which `Relation` defines and nothing delegates. A
        // declaration no legal call can reach is a defect.
        let source = "Story.first.comments.size
";
        let (mut harness, _story, uri) = models_project(source);

        let counted = card(&mut harness, &uri, source, "size");
        // One copy per project puts the interface on one class every relation inherits, so the card
        // names that class, not `Comment::Relation`. That is the stated cost; the assertion below
        // checks the chain still resolves.
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
        // Rails writes `include Enumerable` in `ActiveRecord::Relation`. Without it, `select`
        // correctly answers a relation that is a **dead end** for the `.index_by` or `.map` that
        // usually follows.
        //
        // Asked of `entries`, which the interface deliberately does **not** copy from `Enumerable`
        // (its `Array[E]` erases to `Array` either way), so only the `include` can answer it.
        let source = "Story.where(id: 1).select(:id).entries\n";
        let (mut harness, _story, uri) = models_project(source);

        let sorted = card(&mut harness, &uri, source, "entries");
        assert!(sorted.contains("Enumerable#entries"), "{sorted}");
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
    /// The query interface goes on the **base**, and the base must be a class this pass may declare
    /// on. `Tag < ActsAsTaggableOn::Tag` and `EmailMessage < Ahoy::Message` are real models with a
    /// base class in a gem, so the walk stops at the model itself and it gets its own copy. An
    /// invented base does not work: a generated superclass on a class whose own file names one is
    /// silently ignored.
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
        // The table is all of `ActiveRecord::Querying::QUERYING_METHODS`, not just the names
        // someone could type, plus the two places Rails puts a class method outside it. The width
        // is safe because a name may still refuse a *type*: `pick` and every `async_*` are
        // `untyped`, so the name resolves and the chain stops.
        let source = "Story.first.comments.pluck(:body)\n";
        let (mut harness, _story, uri) = models_project(source);

        // `create!` is class-side, in `Persistence::ClassMethods` and **not** in
        // `QUERYING_METHODS`, so a table read from that constant alone would miss it.
        assert!(harness.has("Story::<Story>#create!()"));
        // …and `relation.rb` defines it too: five of that block's six names are on both sides, and
        // `instantiate` is the class's alone.
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

        // The arity split: one declaration, two arms, and the call's argument count decides.
        // `class_at` reads `Array` off the members offered, so this is what a user would see.
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
        // A bound stated as a test, because it is a decision. `where` with no argument returns a
        // `QueryMethods::WhereChain` (home of `not`, `missing` and `associated`). An arity split
        // like `first`'s **cannot express** it: `arity_of` does not count a keyword hash as a
        // positional argument, so `Story.where(title: "x")`, the commonest call in Rails, also
        // reaches the zero-argument arm. A `WhereChain` arm would answer it for that call too.
        //
        // So `where` answers a relation on every arm. The second assertion matters most: the
        // keyword form keeps its answer.
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
        // Why the relation set is a **union**, not a rule about abstract classes. A project writing
        // `scope :select_fix` in its `ApplicationRecord` made `ApplicationRecord` a collection
        // element and put the class side on a singleton **every model inherits**, so
        // `Category.order(...)` answered a relation of a class no row belongs to, and every chain
        // off it was wrong.
        //
        // Refusing an abstract class a relation deletes the wrong answer and supplies nothing:
        // `Category.select_fix` goes too, because a `scope` declares nothing when its class has no
        // relation. That lost more answers than it fixed. The fix is `Category` owning the names
        // itself, so the inherited pair is never reached.
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

        // **Asserted on the place, not the card.** `order` is declared once on the base, so the
        // card says `ApplicationRecord.order` whatever it returns. What must be right is the
        // *chain*: `Category.order` is a `Category::Relation`, its `first` is a `Category`, and
        // only a `Category` has `things`. The defect was each of those answering
        // `ApplicationRecord`.
        let things = card(&mut harness, &uri, source, "things");
        assert!(
            things.contains("Category#things"),
            "a model answers its own relation, not the one its abstract parent owns: {things}"
        );
        assert!(
            harness.has("ApplicationRecord::<ApplicationRecord>#select_fix()"),
            "and the parent keeps the scope it really does install on every subclass"
        );
        // The other half. While the interface named a concrete class, keeping the class side off an
        // abstract class was right, because it is inherited. The receiver-relative return types fix
        // that at the source, so the interface is on the base *deliberately*, and the chain above
        // proves it safe. The stated trade: `ApplicationRecord.order` resolves here and raises in
        // Ruby, on a receiver nobody writes.
        assert!(harness.has("ActiveRecordRelation#order()"));
        assert!(harness.has("ApplicationRecord::<ApplicationRecord>#order()"));
        assert!(
            !harness.has("Category::<Category>#order()"),
            "and no model declares its own copy, which is the declaration count"
        );
    }

    #[test]
    fn the_chain_that_promoted_this_item_lands_on_one_target() {
        // The chain this exists for: `Story.first.comments.first.user.username`, which without the
        // generators answers with a name-based list that merely contains the right target.
        //
        // Five links, four generators: the class side, a `has_many`, the relation it returns, a
        // `belongs_to`, then a column. The last two documents reopen `class User` from two files,
        // which no other test here combines.
        let source = "Story.first.comments.first.user.username\n";
        let (mut harness, _story, uri) = models_project(source);
        // The shared fixture's `Comment` has no author; this chain needs one.
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
