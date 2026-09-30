//! The relation class every model gets, and the query interface both sides share.
//!
//! Nothing here reads a macro; [`models`](super::models) does that. This is what a collection *is*
//! once read: `Comment::Relation`, a class this crate writes and no file declares, plus the
//! vocabulary ActiveRecord puts on it and on the model's class object.
//!
//! One module, because it is one argument:
//!
//! - **A relation class is almost empty.** Every name on it comes from [`RELATION_BASE`], written
//!   once per project, but one: [`pick`], whose answer is a column's, not the element's. So the
//!   member count grows by one per model with a table.
//! - **The class side is the same list on the model's own base.** [`query_interface`] makes the
//!   sharing possible: it knows what each name returns without knowing which model asked.
//! - **The only relation member with its own place is a `scope`**, which is why [`Chained`] lives
//!   here and not beside the macro it is read from.

use std::collections::BTreeSet;

use crate::generated::{COLLECTION, ELEMENT, FORWARDED, collection_of, grouped_of};
use crate::generated::{Declared, Facts, Owner, Source};

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
            owner: Owner::Instance(collection_of(class)),
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
/// **The cost is the hover card.** `story.comments.where(...)` shows `ActiveRecord::Relation#where`
/// ([`SHOWN`](super::SHOWN)), not `Comment::Relation#where`. The element is gone from the card but
/// still in the *answer*, which is what a reader chains off.
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
/// Ordered and searched in order for [`RAILS_CLASS_SIDE`]'s sake, which needs five names before
/// this one.
pub const RAILS_RELATION: [&str; 1] = ["ActiveRecord::Relation"];

/// The class half, which Ruby answers almost entirely with one line.
///
/// `Story.where` is `delegate(*QUERYING_METHODS, to: :all)` in `querying.rb`, and **no reader can
/// expand a splatted constant into ninety method names**, so `ActiveRecord::Querying` holds no
/// `def` for the graph to find. What remains: the twelve names Rails does write a class-side `def`
/// for, owned by the five modules here, and the rest, which take the relation's because that is
/// exactly what the `delegate` line says (`Story.where` is `Story.all.where`). All twelve
/// class-side `def`s land where `Method#source_location` says.
///
/// **The two `Scoping` modules come before the relation** because `all` is on both: Ruby answers
/// `Story.all` from `Scoping::Named::ClassMethods`, and Rails 8's `QueryMethods#all` is only the
/// relation's.
///
/// `ActiveRecord::Base`'s own singleton is deliberately **not** on this list, although Ruby
/// searches it first. ya-lsp writes the class side onto that singleton, so looking there would find
/// this crate's own place-less declaration and stop, and the twelve names that have a place would
/// lose it.
pub const RAILS_CLASS_SIDE: [&str; 6] = [
    "ActiveRecord::Persistence::ClassMethods",
    "ActiveRecord::Core::ClassMethods",
    "ActiveRecord::Inheritance::ClassMethods",
    "ActiveRecord::Scoping::Named::ClassMethods",
    "ActiveRecord::Scoping::Default::ClassMethods",
    "ActiveRecord::Relation",
];

/// One name of ActiveRecord's query interface, and the evidence for each side it goes on.
struct Query {
    name: &'static str,
    /// Everything before the `->`: the positional parameters, and the block where the method
    /// hands one the element.
    parameters: String,
    returns: String,
    /// Every arm after the first, for the names one signature cannot state.
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
    /// In `QUERYING_METHODS`: the relation defines it and the model delegates to `all`. Also the
    /// two names the other way round, `all` and `unscoped`: the model defines them (`Scoping`)
    /// and a relation reaches them too.
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
    /// In `QUERYING_METHODS`, but the model's answer is not the relation's, so each side has a row
    /// of its own: this is the model's. `count`, `average` and `sum` are a `Hash` on a grouped
    /// relation and never on a model.
    Model,
}

/// Every name in `ActiveRecord::Querying::QUERYING_METHODS` that hands back a relation.
///
/// One list, not one entry each, because the name is the whole row: these take anything and return
/// the relation, which is what makes a query chainable. `with` is on both sides like the delegated
/// names: `QueryMethods#with` is on the relation and `Querying#with` is a `def` beside the list.
const RELATIONAL: [&str; 39] = [
    "reselect",
    "order",
    "regroup",
    "in_order_of",
    "reorder",
    "default_order",
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

/// The bulk writers. Each runs `InsertAll.execute`, which hands back the connection's
/// `ActiveRecord::Result` (an empty one where there was nothing to insert), whatever the adapter.
const WRITES: [&str; 6] = [
    "insert",
    "insert_all",
    "insert!",
    "insert_all!",
    "upsert",
    "upsert_all",
];

/// What `ActiveRecord::Delegation` delegates to a relation's records, the same in activerecord 7.2,
/// 8.0 and 8.1, less `length` and `each`, which a relation answers itself ([`query_interface`]).
const RECORDS: [&str; 24] = [
    "to_xml",
    "encode_with",
    "join",
    "intersect?",
    "[]",
    "&",
    "|",
    "+",
    "-",
    "sample",
    "reverse",
    "rotate",
    "compact",
    "in_groups",
    "in_groups_of",
    "to_sentence",
    "to_fs",
    "to_formatted_s",
    "as_json",
    "shuffle",
    "split",
    "slice",
    "index",
    "rindex",
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
/// `QUERYING_METHODS`**, plus the three places Rails puts a class method outside it:
/// `Persistence::ClassMethods` (where `create!` lives), `Querying#with`, and `Scoping`'s `all`
/// and `unscoped`, the two every other name starts from. A name is here because
/// Rails put it on a model, not because ya-lsp could type it; that is why the async family and the
/// bulk writers are here.
///
/// **The type can still be refused**, which keeps the width safe. `pick`, `calculate`, `minimum`,
/// `maximum` and every `async_*` return `untyped`: the name resolves, the chain stops, and [`Types::harvest`](crate::analysis::types::Types::harvest) drops the claim instead of
/// carrying a wrong one. A relation class with a table overrides `pick` with its columns
/// ([`pick`]).
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
/// # The calls one signature cannot state
///
/// - **A scalar or an array in one argument.** `find`, `destroy`, `create`, `create!` and `build`
///   return a record for one thing and an `Array` for an array of them, with **one** positional
///   argument either way. Each has an arm per argument class, and the argument's class picks
///   (`types::pick_by_argument`); an argument nothing types picks none, so
///   `Story.find(params[:id])` answers nothing: a request can send an array. A keyword hash is one
///   `Hash` to `create` ([`Arity::Keyed`](crate::analysis::cursor::Arity)). `update` and
///   `update!` return `untyped`: their first parameter *defaults to `:all`*. `instantiate` always
///   builds one record.
/// - **A relation's `count`, `average` and `sum` are a `Hash` after `group`**, and nothing here
///   knows whether a relation was grouped, so a relation's say `(T | Hash[untyped, untyped])`. A
///   model's own ([`Side::Model`]) are over every row.
/// - **`pluck` and `ids` are `Array[untyped]`**, not `Array[Element]`: `Story.pluck(:title)` is an
///   array of *columns*.
/// - **`where` with nothing written** returns a [`WHERE_CHAIN`] (home of `not`, `missing` and
///   `associated`), which answers nothing a relation does, while keywords (`Arity::Keyed`) or a
///   positional reach the relation. Where the bundle declares the class, the bare arm names it,
///   holding the relation it was made from ([`where_chain`]); elsewhere it is `untyped`, and a bare
///   `where` answers nothing.
fn query_interface(framework: &BTreeSet<String>) -> Vec<Query> {
    let chain = framework.contains(WHERE_CHAIN);
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
        if name == "where" {
            continue;
        }
        queries.push(both(name, "(*untyped)".to_owned(), relation.clone()));
    }
    // `where` with nothing written is a `WhereChain`, which answers `not`, `missing` and
    // `associated` and nothing a relation does; keywords or a positional reach the relation.
    queries.push(Query {
        name: "where",
        parameters: "()".to_owned(),
        returns: if chain {
            format!("{WHERE_CHAIN}[{relation}]")
        } else {
            "untyped".to_owned()
        },
        // Rails writes `def where(*args)`, so a keyword hash is its first positional, and the
        // positional arm reaches it (`Arity::Keyed`). An arm of its own taking `**untyped` would
        // also take a call with nothing written, and join the relation to the chain.
        overloads: vec![("(untyped, *untyped)".to_owned(), relation.clone())],
        side: Side::Both,
    });
    // The two the model defines itself, `Scoping::Named#all` and `Scoping::Default#unscoped`.
    // A relation answers both as well: Rails 8 writes `QueryMethods#all` and, in `Delegation`,
    // `delegate :unscoped, to: :model`; 7.2 reaches both through `Delegation`'s `method_missing`.
    // Either way the answer is the model's relation. `all_queries:` is the model's keyword, which
    // Rails 8's relation `all` does not take: that call raises, so the answer holds wherever the
    // call returns. `unscoped` with a block runs it inside the unscoped relation and hands back
    // what the block made, the arm nobody can read. The arm only states that a block is taken: a
    // call with one reaches no blockless arm, so it answers nothing either way.
    queries.push(both(
        "all",
        "(?all_queries: untyped)".to_owned(),
        relation.clone(),
    ));
    queries.push(Query {
        name: "unscoped",
        parameters: "()".to_owned(),
        returns: relation.clone(),
        overloads: vec![("[T] () { () -> T }".to_owned(), "T".to_owned())],
        side: Side::Both,
    });
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
    // A scalar finds a record and an array finds an array of them, with one argument either way.
    // The argument's class picks the arm; one nothing types picks none.
    let by_id = |name| Query {
        name,
        parameters: "(Integer)".to_owned(),
        returns: element.to_owned(),
        overloads: vec![
            ("(String)".to_owned(), element.to_owned()),
            ("(Array[untyped])".to_owned(), records.clone()),
        ],
        side: Side::Both,
    };
    queries.push(by_id("find"));
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
        ("delete_all", "()"),
        ("delete_by", "(*untyped)"),
        ("update_all", "(untyped)"),
        ("touch_all", "(*untyped)"),
    ] {
        queries.push(both(name, parameters.to_owned(), "Integer".to_owned()));
    }
    // A model's `delete(id)` is `delete_by`'s count. **A relation class is also an association's
    // `CollectionProxy`**, whose `delete(*records)` and `destroy(*records)` hand back the records
    // they removed, or `nil` where there were none or a `before_remove` callback aborted
    // (`CollectionAssociation#delete_or_destroy`), so a relation's say both.
    queries.push(plain(
        "delete",
        "(untyped)".to_owned(),
        "Integer".to_owned(),
        Side::Model,
    ));
    queries.push(plain(
        "delete",
        "(*untyped)".to_owned(),
        format!("Integer | {records} | nil"),
        Side::Relation,
    ));
    // `destroy(id)` is `find(id).destroy`: the record, or `false` where a callback halted, or
    // `nil` where one raised `ActiveRecord::Rollback`. An array of ids is `find(ids).each(&:destroy)`.
    let destroyed = format!("{element} | false | nil");
    queries.push(Query {
        name: "destroy",
        parameters: "(Integer)".to_owned(),
        returns: destroyed.clone(),
        overloads: vec![
            ("(String)".to_owned(), destroyed.clone()),
            ("(Array[untyped])".to_owned(), records.clone()),
        ],
        side: Side::Model,
    });
    queries.push(plain(
        "destroy",
        "(*untyped)".to_owned(),
        format!("{element} | {records} | false | nil"),
        Side::Relation,
    ));
    // The two that instantiate what they remove and hand the records back. An association's
    // `destroy_all` is `destroy(load_target)`, `nil` for an empty one.
    queries.push(plain(
        "destroy_all",
        "()".to_owned(),
        records.clone(),
        Side::Model,
    ));
    queries.push(plain(
        "destroy_all",
        "()".to_owned(),
        format!("{records}?"),
        Side::Relation,
    ));
    queries.push(both("destroy_by", "(*untyped)".to_owned(), records.clone()));

    // `Batches`. With a block each runs it batch by batch and ends in `nil` (both
    // `batch_on_loaded_relation` and `batch_on_unloaded_relation` do); without one each returns an
    // enumerator of what the block would have been handed.
    for (name, handed, enumerates) in [
        (
            "find_each",
            element.to_owned(),
            format!("Enumerator[{element}, untyped]"),
        ),
        (
            "find_in_batches",
            records.clone(),
            format!("Enumerator[{records}, untyped]"),
        ),
        (
            "in_batches",
            relation.clone(),
            "ActiveRecord::Batches::BatchEnumerator".to_owned(),
        ),
    ] {
        queries.push(Query {
            name,
            parameters: format!("(*untyped) {{ ({handed}) -> void }}"),
            returns: "NilClass".to_owned(),
            overloads: vec![("(*untyped)".to_owned(), enumerates)],
            side: Side::Both,
        });
    }

    // `Calculations`.
    // After `group` a relation's calculations are a `Hash` by group, and nothing here knows
    // whether a relation was grouped, so a relation's say both. The model's own are ungrouped.
    //
    // No block arm for `sum`: `Enumerable#sum` with a block returns whatever the block summed,
    // while a relation's own `sum` is a number. Stating only the blockless arm makes
    // `Story.sum { ... }` answer nothing instead of something wrong.
    for (name, parameters, returns) in [
        ("count", taking_element(), "Integer"),
        ("average", "(untyped)".to_owned(), "Numeric?"),
        ("sum", "(*untyped)".to_owned(), "Numeric"),
    ] {
        queries.push(plain(
            name,
            parameters.clone(),
            returns.to_owned(),
            Side::Model,
        ));
        queries.push(plain(
            name,
            parameters,
            format!("({returns} | Hash[untyped, untyped])"),
            Side::Relation,
        ));
    }
    queries.push(both(
        "calculate",
        "(untyped, untyped)".to_owned(),
        "untyped".to_owned(),
    ));
    // A column's own type, which a relation class's arms say per column ([`pick`]); the model's is
    // `all`'s, so a model reaches the same arms and its own `def self.pluck` still answers first
    //. `group` hands back a grouped relation ([`relation`]), whose calculations are a
    // `Hash` by group.
    for (name, parameters, returns) in [
        ("pick", "(*untyped)", "untyped"),
        ("pluck", "(*untyped)", "Array[untyped]"),
        ("ids", "()", "Array[untyped]"),
        ("group", "(*untyped)", COLLECTION),
    ] {
        queries.push(plain(
            name,
            parameters.to_owned(),
            returns.to_owned(),
            Side::Relation,
        ));
        queries.push(plain(
            name,
            parameters.to_owned(),
            format!("{FORWARDED}[\"all\", \"{name}\"]"),
            Side::Model,
        ));
    }
    // A column's own type, which a relation class's arms say per column ([`pick`]). The model's is
    // `all`'s (`delegate(*QUERYING_METHODS, to: :all)`), so a model reaches the same arms and
    // a model's own `def self.maximum` still answers first.
    for name in ["minimum", "maximum"] {
        queries.push(plain(
            name,
            "(untyped)".to_owned(),
            "untyped".to_owned(),
            Side::Relation,
        ));
        queries.push(plain(
            name,
            "(untyped)".to_owned(),
            format!("{FORWARDED}[\"all\", \"{name}\"]"),
            Side::Model,
        ));
    }

    // A relation's own, which a model does not delegate: its SQL, its Arel, and loading it, which
    // hands the same relation back.
    for (name, parameters, returns) in [
        ("to_sql", "()", "String"),
        ("arel", "(?untyped)", "Arel::SelectManager"),
        ("load", "() ?{ (untyped) -> void }", "self"),
        ("load_async", "()", "self"),
        ("joins!", "(*untyped)", "self"),
    ] {
        queries.push(plain(
            name,
            parameters.to_owned(),
            returns.to_owned(),
            Side::Relation,
        ));
    }
    // `preload(association).collect(&association)`, so an array of whatever the association is.
    queries.push(both(
        "extract_associated",
        "(untyped)".to_owned(),
        "Array[untyped]".to_owned(),
    ));

    // Each is a `Promise` of the value, where the bundle declares the class (7.1 and later).
    let promise = if framework.contains(PROMISE) {
        PROMISE
    } else {
        "untyped"
    };
    for name in ASYNC {
        queries.push(both(name, "(*untyped)".to_owned(), promise.to_owned()));
    }
    for name in WRITES {
        queries.push(both(
            name,
            "(*untyped)".to_owned(),
            "ActiveRecord::Result".to_owned(),
        ));
    }

    // `Relation`'s own, which raise on the model. This is why [`Side`] exists instead of a flag
    // that could only subtract.
    queries.push(plain(
        "to_a",
        "()".to_owned(),
        records.clone(),
        Side::Relation,
    ));
    // `delegate …, :each, to: :records`: the loaded `Array`'s `each`, which hands the array back,
    // or an enumerator over it without a block.
    queries.push(Query {
        name: "each",
        parameters: format!("() {{ ({element}) -> void }}"),
        returns: records.clone(),
        overloads: vec![("()".to_owned(), format!("Enumerator[{element}, {records}]"))],
        side: Side::Relation,
    });
    // What `ActiveRecord::Delegation` hands the loaded records (`delegate …, to: :records`): each
    // is the two calls `delegate` makes ([`FORWARDED`]), through `to_a`, which is the records, so
    // `Story.where(…).reverse` is `Array[Story]`#reverse's answer.
    for name in RECORDS {
        queries.push(plain(
            name,
            "(*untyped) ?{ (*untyped) -> untyped }".to_owned(),
            format!("{FORWARDED}[\"to_a\", \"{name}\"]"),
            Side::Relation,
        ));
    }
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
    // One record from attributes, an array of them from an array: `create(attributes = nil)` maps
    // an `Array` over itself.
    // `relation.rb` has `alias build new`, the *same method*, so the two say the same. `new` is the
    // relation's alone because a model gets `new` from `Class`, which a class-side declaration
    // would shadow.
    for (name, side) in [
        ("create", Side::Both),
        ("create!", Side::Both),
        ("build", Side::Both),
        ("new", Side::Relation),
    ] {
        let block = format!("?{{ ({element}) -> untyped }}");
        queries.push(Query {
            name,
            parameters: format!("() {block}"),
            returns: element.to_owned(),
            overloads: vec![
                (
                    format!("(Hash[untyped, untyped]) {block}"),
                    element.to_owned(),
                ),
                (format!("(Array[untyped]) {block}"), records.clone()),
            ],
            side,
        });
    }
    // `update(id = :all, attributes)`: attributes alone update every record and hand the loaded
    // `Array` back (`each`), an array of ids the records found and updated, and one id that record.
    // A relation's hands the ids to its model's.
    for name in ["update", "update!"] {
        queries.push(Query {
            name,
            parameters: "(untyped)".to_owned(),
            returns: records.clone(),
            overloads: vec![
                ("(Array[untyped], untyped)".to_owned(), records.clone()),
                ("(Integer, untyped)".to_owned(), element.to_owned()),
                ("(String, untyped)".to_owned(), element.to_owned()),
            ],
            side: Side::Both,
        });
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
    // The memo it was handed, whatever the block did to it.
    queries.push(on_relation(
        "each_with_object",
        format!("[U] (U) {{ ({element}, U) -> untyped }}"),
        "U".to_owned(),
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
/// call without a block reaches the same arm. The block **runs against** the record too, and so do
/// the `if:` and `unless:` lambdas ([`super::blocks::model_callback`]), so a bare `title` inside
/// either is the record's.
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
            parameters: super::blocks::model_callback(ELEMENT),
            because: format!(
                "ActiveRecord's callback, installed on every model by {installed_by}. \
                 ya-lsp writes this; no file declares it."
            ),
            at: None,
            from: Source::Interface,
            overloads: Vec::new(),
            private: false,
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
    let owner = Owner::Instance(collection_of(element));
    facts.note(
        owner.clone(),
        format!("A collection of `{element}`. ya-lsp writes this class; no file declares it."),
    );
    // That is all. Every query-interface member is on [`RELATION_BASE`], written once, because the
    // receiver-relative return types keep the element out of signatures. A relation class gets only
    // the scopes [`Chained`] writes, and its columns' [`pick`]. A scope may open the body first: a document writing a
    // scope onto a relation it was not asked to *emit* opens the class with no superclass, and the
    // two merge like a reopened Ruby class.
    //
    // This is never reached with a superclass the pass did not write: if a project declares
    // [`RELATION_BASE`] itself, `knowledge::rails` withdraws every relation instead of letting one
    // inherit whatever the user meant.
    facts.inherits(owner, RELATION_BASE.to_owned());
}

/// The relation `group` hands back for one element: `X::Grouped`, a subclass of
/// `X::Relation` whose calculations are a `Hash` by group. The chain methods it inherits keep it
/// grouped (`types` answers [`COLLECTION`] with a grouped receiver itself), and `X::Relation#group`
/// makes one. Where the project declares the name itself, `group` stays the plain relation.
///
/// `durations`: the model has an `interval` column, whose `sum` and `average` are a `Duration`
/// ([`pick`]'s decline), so the grouped ones are a `Hash` of anything.
pub(super) fn grouped(facts: &mut Facts, element: &str, durations: bool) {
    let relation = collection_of(element);
    let grouped = grouped_of(element);
    let owner = Owner::Instance(grouped.clone());
    facts.note(
        owner.clone(),
        format!("A `{relation}` after `group`. ya-lsp writes this class; no file declares it."),
    );
    facts.inherits(owner.clone(), relation.clone());
    let row = |owner: &Owner, name: &str, parameters: &str, returns: String| Declared {
        owner: owner.clone(),
        name: name.to_owned(),
        returns,
        parameters: parameters.to_owned(),
        because: String::new(),
        at: None,
        from: Source::Query,
        overloads: Vec::new(),
        private: false,
    };
    facts.declare(row(
        &Owner::Instance(relation),
        "group",
        "(*untyped)",
        grouped.clone(),
    ));
    let (summed, averaged) = if durations {
        ("Hash[untyped, untyped]", "Hash[untyped, untyped]")
    } else {
        ("Hash[untyped, Numeric]", "Hash[untyped, Numeric?]")
    };
    for (name, parameters, returns) in [
        ("count", "(*untyped)", "Hash[untyped, Integer]"),
        ("sum", "(*untyped)", summed),
        ("average", "(untyped)", averaged),
        ("minimum", "(untyped)", "Hash[untyped, untyped]"),
        ("maximum", "(untyped)", "Hash[untyped, untyped]"),
    ] {
        facts.declare(row(&owner, name, parameters, returns.to_owned()));
    }
}

/// `pick` on one relation class, with an arm per column it can answer for.
///
/// **The one query-interface name a relation class holds itself**, because its answer is a
/// column's, not the element's: `Comment.where(...).pick(:depth)` is `Integer?`. One signature on
/// [`RELATION_BASE`] cannot say which column a symbol names; an arm per column can, and
/// `types::pick_by_literal` reads the symbol the call wrote. `columns` is the schema's half
/// ([`Picked`](super::Picked)): every type is already the `?` a relation with no row adds.
///
/// - **The last arm takes anything and says nothing**: a string, an `Arel.sql(...)`, a joined
///   table's column, or several columns (an `Array`). RBS tries arms in order, so it answers only
///   what no column arm did.
/// - **One line**, however many columns, and **no place**: Rails' `pick` is the jump, found by the
///   same lookup as the base's copy ([`Source::Query`]).
/// - **Only the relation side.** A model's own `def self.pick` would make the class side's arms
///   wrong, and a relation's `pick` is Rails' `Calculations#pick` whatever the model defines.
///
/// **`minimum` and `maximum` get the same arms**: Rails casts either through the
/// column's type (`type_cast_calculated_value`), `nil` over no row. After `group` they are a `Hash`
/// by group, and nothing here knows whether a relation was grouped, so each arm says both.
pub(super) fn pick(
    facts: &mut Facts,
    element: &str,
    (first, rest): (&Column, &[Column]),
    key: Option<&str>,
) {
    for (name, shape, rest_arm, otherwise) in [
        ("pick", Shape::Picked, "(*untyped)", "untyped"),
        ("minimum", Shape::Grouped, "(untyped)", "untyped"),
        ("maximum", Shape::Grouped, "(untyped)", "untyped"),
        ("pluck", Shape::Plucked, "(*untyped)", "Array[untyped]"),
    ] {
        let arm = |(column, picked, plucked): &Column| {
            (
                format!("(:{column})"),
                match shape {
                    Shape::Picked => picked.clone(),
                    Shape::Grouped => format!("{picked} | Hash[untyped, untyped]"),
                    Shape::Plucked => format!("Array[{plucked}]"),
                },
            )
        };
        let (parameters, returns) = arm(first);
        let mut overloads: Vec<(String, String)> = rest.iter().map(arm).collect();
        overloads.push((rest_arm.to_owned(), otherwise.to_owned()));
        facts.declare(Declared {
            owner: Owner::Instance(collection_of(element)),
            name: name.to_owned(),
            returns,
            parameters,
            because: String::new(),
            at: None,
            from: Source::Query,
            overloads,
            private: false,
        });
    }
    // `ids` is `pluck(primary_key)`, where the key is one column nothing re-keys.
    if let Some(key) = key {
        facts.declare(Declared {
            owner: Owner::Instance(collection_of(element)),
            name: "ids".to_owned(),
            returns: format!("Array[{key}]"),
            parameters: "()".to_owned(),
            because: String::new(),
            at: None,
            from: Source::Query,
            overloads: Vec::new(),
            private: false,
        });
    }
    // `sum` and `average` cast through the column's type too, so an `interval` column's are a
    // `Duration`, which the interface's `Numeric` is not: both decline on this model, on either
    // side. An `untyped` declaration has no vote, so a model's own `def self.sum` still answers.
    // Ranked as the column it rests on, so it outranks the class side's row where the model is its
    // own base.
    if [first]
        .into_iter()
        .chain(rest)
        .any(|(_, returns, _)| returns.contains(DURATION))
    {
        for (owner, (name, parameters)) in [
            Owner::Instance(collection_of(element)),
            Owner::Singleton(element.to_owned()),
        ]
        .into_iter()
        .flat_map(|owner| {
            [("sum", "(*untyped)"), ("average", "(untyped)")].map(|named| (owner.clone(), named))
        }) {
            facts.declare(Declared {
                owner,
                name: name.to_owned(),
                returns: "untyped".to_owned(),
                parameters: parameters.to_owned(),
                because: String::new(),
                at: None,
                from: Source::Column,
                overloads: Vec::new(),
                private: false,
            });
        }
    }
}

/// One column [`pick`] answers for: its name, `pick`'s type and `pluck`'s element type
/// ([`Picked`](super::Picked)).
type Column = (String, String, String);

/// What one of [`pick`]'s names hands back for a column.
#[derive(Clone, Copy)]
enum Shape {
    /// The column's value, or `nil` for no row.
    Picked,
    /// The same, or a `Hash` by group.
    Grouped,
    /// Every row's value: an `Array` of the column's own type.
    Plucked,
}

/// What an `interval` column reads as ([`super::COLUMN_TYPES`]).
pub(super) const DURATION: &str = "ActiveSupport::Duration";

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
///
/// `framework` is the gem classes the bundle declares: [`WHERE_CHAIN`], which the same document
/// then opens ([`where_chain`]), and [`PROMISE`].
pub fn relation_base(facts: &mut Facts, framework: &BTreeSet<String>) {
    let owner = Owner::Instance(RELATION_BASE.to_owned());
    facts.note(
        owner.clone(),
        "ActiveRecord's query interface, for every relation in the project. ya-lsp writes this \
         class; no file declares it."
            .to_owned(),
    );
    facts.mixin(owner.clone(), "Enumerable".to_owned());
    for query in query_interface(framework) {
        if matches!(query.side, Side::Class | Side::Model) {
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
            private: false,
        });
    }
    if framework.contains(WHERE_CHAIN) {
        where_chain(facts);
    }
}

/// `where` with nothing written: ActiveRecord's `QueryMethods::WhereChain`.
pub const WHERE_CHAIN: &str = "ActiveRecord::QueryMethods::WhereChain";

/// What an `async_*` query hands back (ActiveRecord 7.1 and later).
pub const PROMISE: &str = "ActiveRecord::Promise";

/// [`WHERE_CHAIN`], opened with the relation it was made from as its type parameter.
///
/// **Rails keeps that relation in `@scope`**, and each of the three members hands it back with its
/// condition added, which no reader follows. So the relation travels as a type argument: a bare
/// `where` returns `WhereChain[Story::Relation]` ([`query_interface`]), and each member is `-> R`.
/// `Story.where.not(…)` is then `Story::Relation`, and the chain goes on from there.
///
/// - **Only the return is said.** The members are Rails' own `def`s, which rubydex already holds
///   with their places, so each row is place-less [`Source::Interface`], like the framework's
///   singletons.
/// - **The type parameter is ya-lsp's**, and the class note says so: Rails' class has none.
/// - **`missing` is Rails 6.1's and `associated` 7.0's.** An older bundle's class lacks them, and
///   this still names them.
fn where_chain(facts: &mut Facts) {
    let owner = Owner::Instance(WHERE_CHAIN.to_owned());
    facts.generic(owner.clone(), "[R]".to_owned());
    facts.note(
        owner.clone(),
        "What ActiveRecord's `where` returns with nothing written. `R` is the relation it was \
         made from, which ya-lsp writes; Rails' class has no type parameter."
            .to_owned(),
    );
    for name in ["not", "missing", "associated"] {
        facts.declare(Declared {
            owner: owner.clone(),
            name: name.to_owned(),
            returns: "R".to_owned(),
            parameters: "(*untyped)".to_owned(),
            because: String::new(),
            at: None,
            from: Source::Interface,
            overloads: Vec::new(),
            private: false,
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
pub(super) fn class_side(facts: &mut Facts, base: &str, framework: &BTreeSet<String>) {
    for query in query_interface(framework) {
        let because = match query.side {
            Side::Relation => continue,
            // Deliberately short. This sentence sits above **every** class-side declaration, so its
            // length dominated the generated RBS. It may not disappear: `class ApplicationRecord`
            // is the user's own class, and a generated `def self.pluck` with nothing above it reads
            // as something their file declared. A relation class needs none, because the *class* is
            // what ya-lsp invented, an argument this side cannot make.
            Side::Both | Side::Model => {
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
            private: false,
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

    /// One copy per project, checked on the table, not on a document.
    ///
    /// **No interface signature names a concrete application class**, which is the whole mechanism:
    /// where it would name `Story` it names [`ELEMENT`], and where it would name `Story::Relation`
    /// it names [`COLLECTION`]. Checked on every row, so a row added later that spells an element
    /// (and would need a copy per model again) fails here.
    #[test]
    fn no_signature_in_the_interface_names_what_the_collection_holds() {
        let queries = query_interface(&BTreeSet::new());
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
        assert_eq!(naming_the_element, 96, "of {} rows", queries.len());
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
        assert_eq!(named("where").overloads[0].1, COLLECTION);

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
        // `Story::Relation` (see [`Chained`]); then the asked-for relation's `group` and its
        // grouped class's five calculations, which have no place.
        assert_eq!(declarations.methods, 8 + 3 * 4 + 3 * 3 + 1 + 1 + 1 + 5);
        assert_eq!(declarations.spans.len(), 8 + 3 * 4 + 3 * 3 + 1 + 1);
        // `Story`, the `Story::Relation` the scope is chained onto, the `Comment::Relation` this
        // caller asked for and its `Comment::Grouped`. The third shows that a relation class gets
        // members here whether or not this document writes its superclass.
        assert_eq!(declarations.classes, 4);
    }

    /// The base class the whole project's relations inherit.
    #[test]
    fn the_query_interface_is_written_once_and_names_no_model() {
        let mut facts = Facts::default();
        relation_base(&mut facts, &BTreeSet::new());
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
                "def each: () {{ ({ELEMENT}) -> void }} -> Array[{ELEMENT}] | () -> \
                 Enumerator[{ELEMENT}, Array[{ELEMENT}]]\n"
            )),
            "{rbs}"
        );
        // A bulk writer hands back the connection's result, whatever the adapter.
        assert!(
            rbs.contains("def upsert_all: (*untyped) -> ActiveRecord::Result\n"),
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
        relation_base(&mut interface, &BTreeSet::new());
        let relation = interface.render(&declaring(&[])).rbs;

        let (mut on_both, mut relation_only, mut class_only, mut model_only) = (0, 0, 0, 0);
        // A union return is written in brackets, as RBS reads a method type.
        let enclosed = |returns: &str| {
            if returns.contains(" | ") && !returns.starts_with('(') {
                format!("({returns})")
            } else {
                returns.to_owned()
            }
        };
        for query in query_interface(&BTreeSet::new()) {
            let mut signature = format!(
                "{}: {} -> {}",
                query.name,
                query.parameters,
                enclosed(&query.returns)
            );
            for (parameters, returns) in &query.overloads {
                signature.push_str(&format!(" | {parameters} -> {}", enclosed(returns)));
            }
            let on_relation = relation.contains(&format!("  def {signature}\n"));
            let on_class = class_side.contains(&format!("  def self.{signature}\n"));
            assert_eq!(
                (on_relation, on_class),
                match query.side {
                    Side::Both => (true, true),
                    Side::Relation => (true, false),
                    Side::Class | Side::Model => (false, true),
                },
                "{signature} is on the wrong side: relation={on_relation} class={on_class}"
            );
            match query.side {
                Side::Both => on_both += 1,
                Side::Relation => relation_only += 1,
                Side::Class => class_only += 1,
                Side::Model => model_only += 1,
            }
        }
        // A tripwire on the bound: `ActiveRecord::Querying::QUERYING_METHODS` counted, plus
        // `Querying#with`, plus the five `Persistence::ClassMethods` names that `relation.rb` also
        // defines, plus `Scoping`'s `all` and `unscoped`. A name added without a line of Rails
        // behind it moves this number and must say which file it read.
        // `count`, `average` and `sum` are one name each on both sides, written once per side:
        // a relation's may be grouped and answer a `Hash`, the model's may not.
        assert_eq!(
            on_both, 109,
            "QUERYING_METHODS, `with`, `relation.rb`'s five and `Scoping`'s two, less the five \
             calculations and the three removals a relation answers differently, and `pick`, \
             `pluck`, `ids` and `group`, which the model forwards to `all`"
        );
        assert_eq!(
            relation_only, 87,
            "`Relation`'s own — the five that raise on the model, `new`, which `relation.rb` aliases `build` to, \
             `reload`, `Enumerable`'s 39, and `to_sql`, `arel`, `load`, `load_async` and `joins!` — the \
             five calculations and three removals as a relation answers them, `Delegation`'s 24 \
             to the records, and `pick`, `pluck`, `ids` and `group`"
        );
        assert_eq!(
            class_only, 1,
            "`instantiate`, which `Relation` does not define"
        );
        assert_eq!(
            model_only, 12,
            "the five calculations and `delete`, `destroy`, `destroy_all` as the model answers them, \
             and `pick`, `pluck`, `ids` and `group` forwarded to `all`"
        );
        // Every class-side declaration carries its own provenance, because
        // `class ApplicationRecord` is the user's own class and a note on *it* would read as a
        // claim about their file. The relation base carries one note for the whole class, an
        // argument this side cannot make.
        assert_eq!(
            class_side
                .matches("ActiveRecord's query interface, on every model that inherits this.")
                .count(),
            121,
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
        assert!(!mapped.contains("Guessed from name alone"), "{mapped}");

        // And a return that is the element itself.
        let source = "Story.new.comments.detect { |one| one }.story\n";
        let (mut harness, _dump, uri) = models_project(source);
        let found = card(&mut harness, &uri, source, "story");
        assert!(found.contains("Comment#story"), "{found}");
        assert!(!found.contains("Guessed from name alone"), "{found}");
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
        assert!(!started.contains("Guessed from name alone"), "{started}");

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
        assert!(visible.contains("Story::Relation#visible"), "{visible}");
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
        assert!(first.contains("ActiveRecord::Relation#first"), "{first}");
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
        assert!(first.contains("ActiveRecord::Relation#first"), "{first}");
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
        assert!(!found.contains("Guessed from name alone"), "{found}");

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
            missed.contains("Guessed from name alone"),
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
                found.contains(&format!("Widget.{name}")),
                "{name} is the model's own class method: {found}"
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
            with_self.contains("ActiveRecord::Relation#first"),
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
            chained.contains("ActiveRecord::Relation#first"),
            "a receiverless call that wrote an argument still resolves: {chained}"
        );
        assert!(!chained.contains("Guessed from name alone"), "{chained}");
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
        assert!(counted.contains("ActiveRecord::Relation#size"), "{counted}");
        assert!(
            !counted.contains("Guessed from name alone"),
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
            !sorted.contains("Guessed from name alone"),
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
        assert!(
            plucked.contains("ActiveRecord::Relation#pluck"),
            "{plucked}"
        );
        assert!(!plucked.contains("Guessed from name alone"), "{plucked}");

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
            class_at(&mut harness, &uri, "Story.select(:id).to_a.~"),
            "Array",
            "with column names it is still a relation, so the chain runs on through it"
        );
    }

    #[test]
    fn a_call_that_answers_a_record_or_an_array_is_decided_by_its_argument() {
        // `find` and `create` answer a record for one thing and an `Array` for an array of them,
        // with one argument either way. The argument's class picks the arm, and an argument
        // nothing types picks none: `Story.find(params[:id])` is an `Array` when the request
        // sends one.
        let (mut harness, _story, uri) = models_project("Story.first\n");
        // A record offers the model's own members; the name-based list offers everything,
        // `upcase` included.
        let record = |offered: &[String]| {
            offered.iter().any(|name| name == "user")
                && !offered.iter().any(|name| name == "upcase")
        };
        assert!(record(&harness.declarations_at(&uri, "Story.find(1).~")));
        assert!(record(
            &harness.declarations_at(&uri, "Story.find(\"1\").~")
        ));
        assert_eq!(
            class_at(&mut harness, &uri, "Story.find([1, 2]).~"),
            "Array"
        );
        // An argument nothing types reaches both arms: the record's members beside `Array`'s.
        let untyped = harness.declarations_at(&uri, "def show(id)\n  Story.find(id).~\nend\n");
        assert!(
            record(&untyped) && untyped.iter().any(|name| name == "join"),
            "{untyped:?}"
        );
        // A keyword hash is one `Hash` to `create`, never the `Array` arm.
        assert!(record(
            &harness.declarations_at(&uri, "Story.create(title: \"x\").~")
        ));
        assert!(record(&harness.declarations_at(&uri, "Story.create.~")));
        assert_eq!(
            class_at(&mut harness, &uri, "Story.create([{}]).~"),
            "Array"
        );
        // `destroy(1)` is `find(1).destroy`: the record, or `false` where a callback halted it, a
        // union completion lists both for; an array of ids is the records.
        assert!(record(&harness.declarations_at(&uri, "Story.destroy(1).~")));
        assert_eq!(
            class_at(&mut harness, &uri, "Story.destroy([1]).~"),
            "Array"
        );
    }

    #[test]
    fn a_relation_s_count_may_be_a_hash_and_the_model_s_may_not() {
        // After `group`, a relation's calculations are a `Hash` by group, and nothing here knows
        // whether a relation was grouped. A model's own are counted over every row.
        let (mut harness, _story, uri) = models_project("Story.first\n");
        assert_eq!(class_at(&mut harness, &uri, "Story.count.~"), "Integer");
        // Grouped, a `Hash`; not known to be grouped, a union, whose list is `Integer`'s members
        // beside `Hash`'s.
        let grouped = harness.declarations_at(&uri, "Story.group(:x).count.~");
        assert!(
            grouped.iter().any(|row| row == "keys") && !grouped.iter().any(|row| row == "succ"),
            "{grouped:?}"
        );
        let either = harness.declarations_at(&uri, "Story.all.count.~");
        assert!(
            ["succ", "keys"]
                .iter()
                .all(|name| either.iter().any(|row| row == name)),
            "{either:?}"
        );
    }

    #[test]
    fn a_bare_where_answers_nothing_and_a_written_one_is_a_relation() {
        // `where` with nothing written returns a `QueryMethods::WhereChain` (home of `not`,
        // `missing` and `associated`), which answers nothing a relation does. A call writing
        // keywords or a positional reaches the positional arm, as Rails' `where(*args)` takes
        // either; the bare call reaches only its own arm, which a bundle without the class leaves
        // unreadable, as this one does.
        let source = "\
Story.where.not(id: 1)
Story.where(title: \"x\").first.user
Story.where(\"id = 1\").first.user
";
        let (mut harness, _story, uri) = models_project(source);
        assert!(
            !harness.has("Story::Relation#not()") && !harness.has("ActiveRecordRelation#not()"),
            "`not` is `WhereChain`'s and putting it on a relation would make `Story.all.not` \
             resolve, which raises"
        );
        let keyed = card(&mut harness, &uri, source, "user");
        assert!(keyed.contains("Story#user"), "{keyed}");
        // A relation offers its own members and nothing the name-based list would add; a bare
        // `where` offers only that list. Asked last: completion rewrites the document.
        let precise = |offered: &[String]| {
            offered.iter().any(|name| name == "pluck")
                && !offered.iter().any(|name| name == "upcase")
        };
        let positional = harness.declarations_at(&uri, "Story.where(\"id = 1\").~");
        assert!(precise(&positional), "{positional:?}");
        let keywords = harness.declarations_at(&uri, "Story.where(title: \"x\").~");
        assert!(precise(&keywords), "{keywords:?}");
        let bare = harness.declarations_at(&uri, "Story.where.~");
        assert!(!precise(&bare), "{bare:?}");
    }

    /// `all` and `unscoped` are where chains start, and `Scoping` writes them on the model, outside
    /// `QUERYING_METHODS`. Without their rows `Story.all` found Rails' own `def`, which states no
    /// type, so every call after it fell to the name rung.
    #[test]
    fn all_and_unscoped_are_the_relation_on_either_side() {
        let source = "Story.all.first.user\n";
        let (mut harness, _story, uri) = models_project(source);
        let started = card(&mut harness, &uri, source, "user");
        assert!(started.contains("Story#user"), "{started}");
        assert!(!started.contains("Guessed from name alone"), "{started}");

        let relation = |offered: &[String]| {
            offered.iter().any(|name| name == "pluck")
                && !offered.iter().any(|name| name == "upcase")
        };
        for marked in [
            "Story.unscoped.~",
            // The model's keyword.
            "Story.all(all_queries: true).~",
            // And on a relation, where Rails 8 writes `QueryMethods#all` and delegates `unscoped`
            // to the model.
            "Story.where(id: 1).all.~",
            "Story.first.comments.unscoped.~",
        ] {
            let offered = harness.declarations_at(&uri, marked);
            assert!(relation(&offered), "{marked}: {offered:?}");
        }
        // Still the receiver's own model, through a relation that is not `Story`'s.
        assert!(
            harness
                .declarations_at(&uri, "Story.first.comments.all.first.~")
                .iter()
                .any(|name| name == "story"),
            "a comment has a story"
        );
        // With a block, `unscoped` hands back what the block made.
        let blocked = harness.declarations_at(&uri, "Story.unscoped { 1 }.~");
        assert!(!relation(&blocked), "{blocked:?}");
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
        assert!(!card.contains("Guessed from name alone"), "{card}");
        assert!(!card.contains("Guessed from name alone"), "{card}");

        let definition = harness.definition_at(&uri, source, "username");
        assert_eq!(definition.as_array().map(Vec::len), Some(1), "{definition}");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(schema.as_str()),
            "{definition}"
        );
    }

    /// The two classes the interface names only where the bundle declares them.
    #[test]
    fn a_gem_class_the_interface_names_is_written_only_where_the_bundle_declares_it() {
        let render = |framework: &BTreeSet<String>| {
            let mut facts = Facts::default();
            relation_base(&mut facts, framework);
            facts.render(&declaring(&[])).rbs
        };
        let bare = render(&BTreeSet::new());
        assert!(
            bare.contains("def async_count: (*untyped) -> untyped\n"),
            "{bare}"
        );
        let declared = render(&[PROMISE.to_owned(), WHERE_CHAIN.to_owned()].into());
        assert!(
            declared.contains("def async_count: (*untyped) -> ActiveRecord::Promise\n"),
            "{declared}"
        );
        assert!(
            declared.contains("class ActiveRecord::QueryMethods::WhereChain"),
            "{declared}"
        );
    }

    /// `minimum` and `maximum` cast through the column's type, as `pick` does, and after `group`
    /// are a `Hash`, so a relation's arm says both; a model's is its `all`'s. An `interval` column's
    /// `sum` and `average` are a `Duration`, which the interface's `Numeric` is not, so that model
    /// declines both, on either side, and its grouped ones are a `Hash` of anything
    ///.
    #[test]
    fn a_calculation_is_the_type_of_the_column_it_names() {
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
        harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\nend\n",
        );
        let visit = harness.write(
            "app/models/visit.rb",
            "class Visit < ApplicationRecord\nend\n",
        );
        harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[7.1].define(version: 1) do\n  \
             create_table \"comments\", force: :cascade do |t|\n    \
             t.integer \"depth\", null: false\n  end\n  \
             create_table \"visits\", force: :cascade do |t|\n    \
             t.interval \"spent\"\n  end\nend\n",
        );
        let source = "\
low = Comment.where(id: 1).minimum(:depth)
high = Comment.maximum(:depth)
total = Comment.sum(:depth)
spent = Visit.sum(:spent)
kept = Visit.where(id: 1).average(:spent)
grouped = Visit.group(:id).sum(:spent)
";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "low: Integer? | Hash = Comment.where(id: 1).minimum(:depth)\n\
             high: Integer? | Hash = Comment.maximum(:depth)\n\
             grouped: Hash = Visit.group(:id).sum(:spent)"
        );
        let generated = harness.generated_for(&visit).unwrap_or_default();
        for line in [
            "def self.sum: (*untyped) -> untyped",
            "def self.average: (untyped) -> untyped",
            "def sum: (*untyped) -> untyped",
            "def average: (untyped) -> untyped",
            "def maximum: (:id) -> (Integer? | Hash[untyped, untyped]) | (:spent) -> \
             (ActiveSupport::Duration? | Hash[untyped, untyped]) | (untyped) -> untyped",
            "def sum: (*untyped) -> Hash[untyped, untyped]",
            "def average: (untyped) -> Hash[untyped, untyped]",
        ] {
            assert!(generated.contains(line), "{line}: {generated}");
        }
    }

    /// What a relation answers where Rails hands the call to its loaded records or to an
    /// association's proxy: `each` is the `Array`, an `Array` of attributes builds an
    /// `Array`, `update` with attributes alone updates every record, a block `unscoped` runs is
    /// its value, and `each_with_object` is its memo.
    #[test]
    fn a_relation_answers_what_rails_hands_its_call_to() {
        let source = "\
walked = Story.where(id: 1).each { |story| story }
built = Story.all.new([{}, {}])
one = Story.all.new(title: \"x\")
updated = Story.where(id: 1).update(title: \"x\")
found = Story.update(1, title: \"x\")
scoped = Story.unscoped { 1 }
memo = Story.all.each_with_object([]) { |story, all| all }
";
        let (mut harness, _story, uri) = models_project(source);
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "walked: Array[Story] = Story.where(id: 1).each { |story| story }\n\
             walked = Story.where(id: 1).each { |story: Story| story }\n\
             built: Array[Story] = Story.all.new([{}, {}])\n\
             one: Story = Story.all.new(title: \"x\")\n\
             updated: Array[Story] = Story.where(id: 1).update(title: \"x\")\n\
             found: Array | Story = Story.update(1, title: \"x\")\n\
             scoped: Integer = Story.unscoped { 1 }\n\
             memo: Array = Story.all.each_with_object([]) { |story, all| all }\n\
             memo = Story.all.each_with_object([]) { |story: Story, all| all }"
        );
    }

    /// `pluck` by column, `ids` by the primary key where nothing moves it, and a
    /// grouped relation whose calculations are a `Hash` by group, through any chain.
    ///
    /// - `pluck` keeps a column's `?` only where it is nullable: stored rows hold `null: false`
    ///   values. `body` is nullable, so its element is no bare class, and several columns reach
    ///   the catch-all.
    /// - `ids` is untyped for `Keystore`, which writes `self.primary_key =`, for `Archived`, which
    ///   inherits that key, for `Pinned`, which includes a concern writing it, for `Legacy`,
    ///   which defines `self.primary_key`, and for `Tally`, whose body sets some receiver's key.
    /// - `Tally::Grouped` is the application's own class, so `Tally.group` stays the relation.
    #[test]
    fn a_relation_plucks_its_columns_and_groups_its_calculations() {
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
        harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\n  has_many :comments\nend\n",
        );
        harness.write(
            "app/models/keystore.rb",
            "class Keystore < ApplicationRecord\n  self.primary_key = \"key\"\nend\n",
        );
        harness.write("app/models/archived.rb", "class Archived < Keystore\nend\n");
        harness.write(
            "app/models/concerns/keyed.rb",
            "module Keyed\n  extend ActiveSupport::Concern\n\n  included do\n    \
             self.primary_key = \"uid\"\n  end\nend\n",
        );
        harness.write(
            "app/models/pinned.rb",
            "class Pinned < ApplicationRecord\n  include Keyed\nend\n",
        );
        harness.write(
            "app/models/legacy.rb",
            "class Legacy < ApplicationRecord\n  def self.primary_key\n    \"code\"\n  end\nend\n",
        );
        harness.write(
            "app/models/tally.rb",
            "class Tally < ApplicationRecord\n  Legacy.primary_key = \"code\"\nend\n",
        );
        harness.write("app/models/tally/grouped.rb", "class Tally::Grouped\nend\n");
        harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[7.1].define(version: 1) do\n  \
             create_table \"comments\", force: :cascade do |t|\n    \
             t.integer \"depth\", null: false\n    t.string \"body\"\n  end\n  \
             create_table \"keystores\", force: :cascade do |t|\n    t.string \"key\"\n  end\n  \
             create_table \"archiveds\", force: :cascade do |t|\n    t.string \"key\"\n  end\n  \
             create_table \"pinneds\", force: :cascade do |t|\n    t.string \"uid\"\n  end\n  \
             create_table \"legacies\", force: :cascade do |t|\n    t.string \"code\"\n  end\n  \
             create_table \"tallies\", force: :cascade do |t|\n    t.integer \"n\"\n  end\nend\n",
        );
        let source = "\
depth = Comment.where(id: 1).pluck(:depth)
body = Comment.all.pluck(:body)
class_side = Comment.pluck(:depth)
both = Comment.all.pluck(:depth, :body)
ids = Comment.ids
relation_ids = Comment.where(id: 1).ids
kept = Keystore.ids
inherited = Archived.ids
included = Pinned.ids
defined = Legacy.ids
elsewhere = Tally.ids
counted = Comment.group(:depth).count
chained = Comment.group(:depth).where(id: 1).order(:id).count
summed = Comment.all.group(:depth).sum(:depth)
ungrouped = Comment.where(id: 1).count
owned = Tally.group(:n).count
";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "\
depth: Array[Integer] = Comment.where(id: 1).pluck(:depth)
body: Array = Comment.all.pluck(:body)
class_side: Array[Integer] = Comment.pluck(:depth)
both: Array = Comment.all.pluck(:depth, :body)
ids: Array[Integer] = Comment.ids
relation_ids: Array[Integer] = Comment.where(id: 1).ids
kept: Array = Keystore.ids
inherited: Array = Archived.ids
included: Array = Pinned.ids
defined: Array = Legacy.ids
elsewhere: Array = Tally.ids
counted: Hash[untyped, Integer] = Comment.group(:depth).count
chained: Hash[untyped, Integer] = Comment.group(:depth).where(id: 1).order(:id).count
summed: Hash = Comment.all.group(:depth).sum(:depth)
ungrouped: Integer | Hash = Comment.where(id: 1).count
owned: Integer | Hash = Tally.group(:n).count"
        );
    }

    /// `pick(:column)` on a relation is that column's type or `nil`, and only for a column read
    /// through its own type.
    ///
    /// - `depth` is `null: false` and still `Integer?`: a relation with no row picks `nil`.
    /// - `state` is an `enum`'s label, `meta` a `serialize`'s value, `price` an `attribute`'s type
    ///   object, `tags` a concern's `serialize` and `prefs` a `store`'s hash: none is the column's
    ///   type, so none gets an arm.
    /// - A string, a variable, two columns and a name no column has reach the catch-all, which
    ///   answers nothing. The class side is `all`'s, so a model's own `def self.pick`
    ///   still answers first.
    #[test]
    fn a_relation_picks_the_type_of_the_column_its_symbol_names() {
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
        let comment = harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\n  enum :state, { open: 0 }\n  \
             serialize :meta, coder: YAML\n  attribute :price, Money::Type.new\n  \
             store :prefs, accessors: [:color]\nend\n",
        );
        harness.write(
            "app/models/concerns/tagged.rb",
            "module Tagged\n  extend ActiveSupport::Concern\n\n  included do\n    \
             serialize :tags\n  end\nend\n",
        );
        harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[7.1].define(version: 1) do\n  \
             create_table \"comments\", force: :cascade do |t|\n    \
             t.integer \"depth\", null: false\n    t.string \"body\"\n    \
             t.string \"codes\", array: true\n    t.integer \"state\"\n    t.text \"meta\"\n    \
             t.decimal \"price\"\n    t.text \"tags\"\n    t.text \"prefs\"\n    \
             t.jsonb \"data\"\n  end\nend\n",
        );
        let source = "\
depth = Comment.where(id: 1).pick(:depth)
body = Comment.all.order(:id).pick(:body)
codes = Comment.all.pick(:codes)
state = Comment.all.pick(:state)
meta = Comment.all.pick(:meta)
price = Comment.all.pick(:price)
tags = Comment.all.pick(:tags)
prefs = Comment.all.pick(:prefs)
data = Comment.all.pick(:data)
named = Comment.all.pick(\"depth\")
column = :depth
held = Comment.all.pick(column)
both = Comment.all.pick(:depth, :body)
none = Comment.all.pick(:nothing)
class_side = Comment.pick(:depth)
";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "\
depth: Integer? = Comment.where(id: 1).pick(:depth)
body: String? = Comment.all.order(:id).pick(:body)
codes: Array[String]? = Comment.all.pick(:codes)
class_side: Integer? = Comment.pick(:depth)"
        );
        let generated = harness.generated_for(&comment).unwrap_or_default();
        assert!(
            generated.contains(
                "def pick: (:id) -> Integer? | (:depth) -> Integer? | (:body) -> String? | \
                 (:codes) -> Array[String]? | (*untyped) -> untyped"
            ),
            "{generated}"
        );
    }
}
