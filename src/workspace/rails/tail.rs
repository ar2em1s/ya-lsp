//! The long tail: every remaining Rails macro that names a member, as one table.
//!
//! Individually none of these earns a reader, and they are all the same shape — a name, a
//! signature, a span, and a rule for when to decline. What makes them one table rather than a
//! reader each is that every one installs a fixed set of members whose names are *affixes* on the
//! names the call was given, and whose types are either written down here or come from one class
//! the call names.
//!
//! # What the table is made of
//!
//! [`Installs`] names the family, [`super::LONG_TAIL`] says which macro is in which, and [`row`]
//! is every family's members side by side, so "what does this crate claim Rails installs" is
//! answered by reading one function. A [`Row`] has three parts:
//!
//! - **[`Names`]** — where the names come from. `class_attribute :a, :b` gives two; a
//!   `has_secure_token` with no arguments gives the one Rails defaults to; a `store_accessor`
//!   gives all but its first.
//! - **[`Shape`]** — one member per name, spelled as an [`Affix`] around it, with the side it
//!   hangs on, its parameters, and either a type written here or a reference to the one class
//!   the call names.
//! - **[`Typing`]** — where [`Returns::Named`] comes from, and the gate it has to pass.
//!
//! # The four that read and declare nothing
//!
//! Being *read* and declining is the only way a name stays reviewable: a macro absent from the
//! table looks exactly like one nobody thought about. They are two categories, which
//! [`Installs::Nothing`] and [`Installs::Elsewhere`] keep apart.
//!
//! There is no method to declare. `normalizes` appends to `normalized_attributes` and defines
//! nothing per call; `encrypts` re-encrypts an attribute that already exists without changing
//! what it returns; `generates_token_for` installs `generate_token_for` and `find_by_token_for`,
//! both defined once in `ActiveRecord::TokenFor`.
//!
//! There is nowhere to put it. `helper_method` writes a real method per call into
//! `_helpers_for_modification` — a module the **view context** includes, not the controller — and
//! that host is the one thing in Rails this crate has no type for: a template's implicit
//! receiver. Declaring the member on the controller instead would restate a `def` rubydex
//! already has and put a second *place* under a name that had one. A bare `current_user` in a
//! template already hovers and jumps on the name rung, so the gap is the receiver rather than
//! the declaration, and `analysis::views` closes it. This table's job there is to be the
//! **allow-list**: `helper_method` is exactly which of a controller's methods Rails exposes, and
//! a feature built without it would offer every action and every private method to a template.
//!
//! # `delegated_type`'s static half ships and its fan-out does not
//!
//! Whatever its `types:` holds, it installs `entryable_types`, `entryable_name` and
//! `build_entryable` — not `entryables`, and `entryable_type` is the polymorphic *column* rather
//! than a method the macro writes. Those four ship. The per-element fan-out (a scope, a
//! predicate, a reader and an id reader for each entry in `types:`) is declined: it needs each
//! element to be a class the application defines and needs a relation of the owning class, which
//! is a reader and not a table row.

use std::collections::BTreeSet;

use ruby_prism::{CallNode, Node};

use super::LONG_TAIL;
use super::inflect::camelize;
use super::syntax::{constant_spelling, header, keyword, symbol_or_string};
use crate::generated::{Declared, Facts, Owner, Source};

/// Which family a macro is in. One word per row of [`super::LONG_TAIL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Installs {
    /// `class_attribute :setting` — reader, writer and predicate, on both sides.
    ClassAttribute,
    /// `mattr_reader`, and the `cattr_` and `thread_` spellings of it.
    ModuleReader,
    /// `mattr_writer`, and the same three spellings.
    ModuleWriter,
    /// `mattr_accessor`, which is both.
    ModuleAccessor,
    /// `accepts_nested_attributes_for :author` — `author_attributes=`.
    NestedAttributes,
    /// `store_accessor :settings, :a, :b` — reader, writer and `_changed?` per key.
    StoreAccessor,
    /// `store :settings, accessors: [ :a ]` — the same, from a keyword.
    Store,
    /// `alias_attribute :new, :old` — reader, writer and predicate, typed in phase two.
    AliasAttribute,
    /// `has_secure_token :token` — `regenerate_token`.
    SecureToken,
    /// `has_secure_password` — the eight names one call installs.
    SecurePassword,
    /// `composed_of :balance, class_name: "Money"` — a class the application defines.
    ComposedOf,
    /// `has_one_attached :avatar` — a class Active Storage defines.
    OneAttached,
    /// `has_many_attached :images` — likewise.
    ManyAttached,
    /// `has_rich_text :body` — a class Action Text defines.
    RichText,
    /// `delegated_type :entryable, types: [ ... ]` — the four static names of it.
    DelegatedType,
    /// `serialize :codes, coder: YAML, type: Array` — the column, re-typed.
    Serialize,
    /// Read, and there is **no method to declare**: the macro defines nothing per call.
    Nothing,
    /// Read, and the method it installs goes somewhere this crate does not model.
    ///
    /// One member: `helper_method`. It is not [`Installs::Nothing`] and the difference is the
    /// whole of why it is declined — `abstract_controller/helpers.rb` really does write
    /// `def current_user(...)` per call, onto the controller's `_helpers` module, which the
    /// **view context** includes. A template's implicit receiver has no type here, so there is
    /// nothing for that member to hang on that anything would reach; and what a template's bare
    /// call answers *today* — a jump and a hover, on the name rung — is already right, and is
    /// not improved by declaring a second copy of a `def` rubydex already has.
    Elsewhere,
}

impl Installs {
    /// Whether this family reads a call and says nothing about it.
    ///
    /// Two variants and two different reasons, which is the point of their being two: there is
    /// **no method** ([`Installs::Nothing`]) or there is **nowhere to put it**
    /// ([`Installs::Elsewhere`]). Both decline here and only one of them is a dead end: what
    /// `helper_method` names is reachable through `analysis::views`.
    pub(super) fn declines(self) -> bool {
        matches!(self, Self::Nothing | Self::Elsewhere)
    }
}

/// Where the names a call installs members for come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Names {
    /// Every positional literal: `class_attribute :a, :b`.
    All,
    /// The first positional, or the name Rails defaults to when the call gave none.
    First(&'static str),
    /// Every positional after the first, which names the column they are stored in.
    Keys,
    /// The `accessors:` keyword's array.
    Accessors,
    /// The first positional; the second is the attribute it aliases.
    Alias,
}

/// Where a [`Returns::Named`] comes from, and what it has to pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Typing {
    /// Every type is written in the table. Nothing to look up and nothing to decline.
    Fixed,
    /// One class, from `class_name:` or from the name camelized — [`super::ASSOCIATIONS`]' own
    /// rule, and gated the same way: a class the application does not define declares nothing.
    OwnClass,
    /// One class, fixed by the macro and declared by a **gem**.
    ///
    /// `ActiveStorage::Attached::One` is not this application's class and never will be, so the
    /// gate cannot be [`Model::signatures`](super::Model::signatures)' `known`. It is whether
    /// the graph has the constant at all, which is the caller's to answer — a bundle without
    /// Active Storage in it declares nothing here rather than naming a class no jump can reach.
    Gem(&'static str),
    /// The second positional names an attribute of this same class, and phase two asks what
    /// that one returns.
    Aliased,
    /// The constant the call's `type:` names, written down with **no gate at all** — the one
    /// place in this directory a class name is declared without asking whether anything defines
    /// it, and `delegate`'s inversion is why. A `serialize` declares no member: the member is the
    /// **column**, which certainly exists, and all this changes is the type it answers with. So
    /// a `type:` naming something unreachable degrades to no answer, which is exactly where the
    /// attribute already was, while declining would leave the column's own type standing — and
    /// that one is *known* to be wrong, because a serialized column is `text` in the database
    /// and never a `String` in Ruby.
    Written,
}

/// How a member is spelled, given a name the call wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Affix {
    /// Written around it: `("regenerate_", "")` and `("", "_attributes=")`.
    Around(&'static str, &'static str),
    /// A name of its own, installed **only** when the call took the macro's default.
    ///
    /// `has_secure_password` aliases `authenticate` to `authenticate_password` and does it
    /// `if attribute == :password`, so a call that named something else installs
    /// `authenticate_recovery` and no bare `authenticate`. One clause, and without it this
    /// declares the commonest method in the family or invents one, with no third option.
    Default(&'static str),
}

/// What a member returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Returns {
    /// Written here: `untyped`, `bool`, `String?`.
    Fixed(&'static str),
    /// The one class the call names, subject to [`Typing`].
    Named,
}

/// One member a macro installs.
#[derive(Debug, Clone, Copy)]
pub(super) struct Shape {
    name: Affix,
    /// Whether it is a `def self.`.
    singleton: bool,
    parameters: &'static str,
    returns: Returns,
    /// The keywords that turn this member off, each read as "written, and written as `false`".
    ///
    /// `class_attribute :setting, instance_writer: false` really does not install
    /// `setting=` on the instance, and declaring one would be a member that raises. The lists
    /// differ per side on purpose: `instance_predicate: false` removes the **class**-side
    /// predicate too, which is `attribute.rb`'s own `if instance_predicate` around both.
    off: &'static [&'static str],
}

/// One row of the table: where the names come from, what each installs, and how it is typed.
pub(super) struct Row {
    pub(super) names: Names,
    pub(super) typing: Typing,
    pub(super) shapes: &'static [Shape],
}

/// No keyword turns this member off.
const ALWAYS: &[&str] = &[];
/// The instance reader, which two keywords can remove.
const READER: &[&str] = &["instance_reader", "instance_accessor"];
/// The instance writer, likewise.
const WRITER: &[&str] = &["instance_writer", "instance_accessor"];
/// A predicate on either side: one keyword removes both.
const PREDICATE: &[&str] = &["instance_predicate"];
/// The instance predicate, which needs the reader as well as the predicate.
const INSTANCE_PREDICATE: &[&str] = &["instance_predicate", "instance_reader", "instance_accessor"];

/// The three members every family in this table is built out of, on the instance side.
///
/// Spelled as base values and varied with `..`, rather than as three `const fn`s: a helper whose
/// only caller is a `const` item is compiled but never *run*, so a constructor here would be
/// three functions this file's own coverage bar could never reach. Struct update syntax says the
/// same thing and is a value rather than a call.
const READ: Shape = Shape {
    name: Affix::Around("", ""),
    singleton: false,
    parameters: "()",
    returns: Returns::Fixed("untyped"),
    off: ALWAYS,
};

/// A writer, which takes whatever it is given and hands it back.
const WRITE: Shape = Shape {
    name: Affix::Around("", "="),
    parameters: "(untyped)",
    ..READ
};

/// A predicate, which is `!!` of something and so is a `bool` whatever that something is.
const ASK: Shape = Shape {
    name: Affix::Around("", "?"),
    returns: Returns::Fixed("bool"),
    ..READ
};

/// `class_attribute :setting` — six members, and four keywords that remove them.
///
/// `class_eval "class << self; def #{name}; ...; def #{name}=(value); ...", then the instance
/// halves, then `def #{name}?; !!self.#{name}; end` on both sides.
const CLASS_ATTRIBUTE: &[Shape] = &[
    Shape {
        singleton: true,
        ..READ
    },
    Shape {
        singleton: true,
        ..WRITE
    },
    Shape {
        singleton: true,
        off: PREDICATE,
        ..ASK
    },
    Shape {
        off: READER,
        ..READ
    },
    Shape {
        off: WRITER,
        ..WRITE
    },
    Shape {
        off: INSTANCE_PREDICATE,
        ..ASK
    },
];

/// `mattr_reader :x` — `def self.#{sym}; @@#{sym}; end`, and the instance half unless it is off.
const MODULE_READER: &[Shape] = &[
    Shape {
        singleton: true,
        ..READ
    },
    Shape {
        off: READER,
        ..READ
    },
];

/// `mattr_writer :x` — the same pair, assigning.
const MODULE_WRITER: &[Shape] = &[
    Shape {
        singleton: true,
        ..WRITE
    },
    Shape {
        off: WRITER,
        ..WRITE
    },
];

/// `mattr_accessor :x` — both, which is what Rails' own `mattr_accessor` calls.
const MODULE_ACCESSOR: &[Shape] = &[
    Shape {
        singleton: true,
        ..READ
    },
    Shape {
        off: READER,
        ..READ
    },
    Shape {
        singleton: true,
        ..WRITE
    },
    Shape {
        off: WRITER,
        ..WRITE
    },
];

/// `accepts_nested_attributes_for :author` — `def #{name}_attributes=(attributes)`, and nothing
/// else. Rails also raises unless the association exists, which this deliberately does not
/// check: a `has_many` written in a concern is an association this reader cannot see, and
/// declining there would cost the member for the one shape a concern body exists to support.
const NESTED_ATTRIBUTES: &[Shape] = &[Shape {
    name: Affix::Around("", "_attributes="),
    singleton: false,
    parameters: "(untyped)",
    returns: Returns::Fixed("untyped"),
    off: ALWAYS,
}];

/// One store key: what it reads back, what it takes, and whether it changed.
///
/// A reader, a writer and `#{key}_changed?` per key, in a `GeneratedStoreMethods` module the
/// macro includes. The other three it writes — `_change`, `_was`, `_before_last_save` — are the
/// same shape and are declined for the reason `synthesized.md` gives: what a store holds is
/// `untyped`, so each of them is a name and no more.
const STORE: &[Shape] = &[
    READ,
    WRITE,
    Shape {
        name: Affix::Around("", "_changed?"),
        ..ASK
    },
];

/// `alias_attribute :new, :old` — the three that are the attribute itself.
///
/// The macro aliases the whole pattern set, the dirty-tracking members included. The type is the
/// aliased attribute's, which is a fact some other document states — see [`Typing::Aliased`].
const ALIAS_ATTRIBUTE: &[Shape] = &[
    Shape {
        returns: Returns::Named,
        ..READ
    },
    WRITE,
    ASK,
];

/// `has_secure_token :token` — `define_method("regenerate_#{attribute}") { update! ... }`, and
/// `update!` answers `true` or raises. The attribute itself is a column and is the schema's.
const SECURE_TOKEN: &[Shape] = &[Shape {
    name: Affix::Around("regenerate_", ""),
    singleton: false,
    parameters: "()",
    returns: Returns::Fixed("bool"),
    off: ALWAYS,
}];

/// `composed_of` and the two attachment macros — a reader of one class, and a writer.
///
/// `reader_method` and `writer_method` in `aggregations.rb`, and the two `class_eval`'d methods
/// at the top of `has_one_attached`. Three families share the pair because they *are* the same
/// pair; what differs is where [`Typing`] gets the class from.
const READER_AND_WRITER: &[Shape] = &[
    Shape {
        returns: Returns::Named,
        ..READ
    },
    WRITE,
];

/// `has_rich_text :body` — `def #{name}`, `def #{name}?` and a writer.
const RICH_TEXT: &[Shape] = &[
    Shape {
        returns: Returns::Named,
        ..READ
    },
    WRITE,
    ASK,
];

/// `serialize :codes, type: Array` — the column, answering the class the coder round-trips
/// through. Nothing is defined; `decorate_attributes` replaces the type of a member that
/// already exists.
const SERIALIZE: &[Shape] = &[Shape {
    returns: Returns::Named,
    ..READ
}];

/// The eight `has_secure_password` installs, out of `secure_password.rb`.
///
/// `attr_reader attribute` and `attr_accessor :"#{attribute}_confirmation"`,
/// `:"#{attribute}_challenge"`; then `define_method` for the writer, `authenticate_#{attribute}`
/// and `#{attribute}_salt`. Every one of them is `nil` until something assigns it, which is what
/// the `?`s are. `authenticate_` answers the record or `false`, so the honest common type is
/// `untyped` rather than a `bool` that would be wrong for the branch everybody uses.
const SECURE_PASSWORD: &[Shape] = &[
    Shape {
        returns: Returns::Fixed("String?"),
        ..READ
    },
    WRITE,
    Shape {
        name: Affix::Around("", "_confirmation"),
        singleton: false,
        parameters: "()",
        returns: Returns::Fixed("String?"),
        off: ALWAYS,
    },
    Shape {
        name: Affix::Around("", "_confirmation="),
        singleton: false,
        parameters: "(untyped)",
        returns: Returns::Fixed("untyped"),
        off: ALWAYS,
    },
    Shape {
        name: Affix::Around("", "_challenge"),
        singleton: false,
        parameters: "()",
        returns: Returns::Fixed("String?"),
        off: ALWAYS,
    },
    Shape {
        name: Affix::Around("", "_challenge="),
        singleton: false,
        parameters: "(untyped)",
        returns: Returns::Fixed("untyped"),
        off: ALWAYS,
    },
    Shape {
        name: Affix::Around("", "_salt"),
        singleton: false,
        parameters: "()",
        returns: Returns::Fixed("String?"),
        off: ALWAYS,
    },
    Shape {
        name: Affix::Around("authenticate_", ""),
        singleton: false,
        parameters: "(String)",
        returns: Returns::Fixed("untyped"),
        off: ALWAYS,
    },
    Shape {
        name: Affix::Default("authenticate"),
        singleton: false,
        parameters: "(String)",
        returns: Returns::Fixed("untyped"),
        off: ALWAYS,
    },
];

/// The four names a `delegated_type` installs whatever its `types:` holds.
///
/// The role itself is a polymorphic `belongs_to`, which names no class at all — the association
/// reader declines exactly that shape, and `untyped` here is a member that exists rather than
/// one it invents.
const DELEGATED_TYPE: &[Shape] = &[
    READ,
    Shape {
        name: Affix::Around("", "_class"),
        singleton: false,
        parameters: "()",
        returns: Returns::Fixed("Class"),
        off: ALWAYS,
    },
    Shape {
        name: Affix::Around("", "_name"),
        singleton: false,
        parameters: "()",
        returns: Returns::Fixed("String"),
        off: ALWAYS,
    },
    Shape {
        name: Affix::Around("build_", ""),
        singleton: false,
        parameters: "(*untyped)",
        returns: Returns::Fixed("untyped"),
        off: ALWAYS,
    },
    Shape {
        name: Affix::Around("", "_types"),
        singleton: true,
        parameters: "()",
        returns: Returns::Fixed("Array[String]"),
        off: ALWAYS,
    },
];

/// Nothing at all — the four families that are read and decline.
const DECLARES_NOTHING: &[Shape] = &[];

/// Every family's row: where its names come from, how it is typed, and what it installs.
///
/// The one function in this crate that says what Rails installs for a macro nobody reads out of
/// Rails, so each of the lists above was read out of the framework's own source at `7ba5fa3`
/// rather than remembered — the file each came from is named in `synthesized.md`.
pub(super) fn row(installs: Installs) -> Row {
    let (names, typing, shapes) = match installs {
        Installs::ClassAttribute => (Names::All, Typing::Fixed, CLASS_ATTRIBUTE),
        Installs::ModuleReader => (Names::All, Typing::Fixed, MODULE_READER),
        Installs::ModuleWriter => (Names::All, Typing::Fixed, MODULE_WRITER),
        Installs::ModuleAccessor => (Names::All, Typing::Fixed, MODULE_ACCESSOR),
        Installs::NestedAttributes => (Names::All, Typing::Fixed, NESTED_ATTRIBUTES),
        Installs::StoreAccessor => (Names::Keys, Typing::Fixed, STORE),
        Installs::Store => (Names::Accessors, Typing::Fixed, STORE),
        Installs::AliasAttribute => (Names::Alias, Typing::Aliased, ALIAS_ATTRIBUTE),
        Installs::SecureToken => (Names::First("token"), Typing::Fixed, SECURE_TOKEN),
        Installs::SecurePassword => (Names::First("password"), Typing::Fixed, SECURE_PASSWORD),
        Installs::ComposedOf => (Names::First(""), Typing::OwnClass, READER_AND_WRITER),
        Installs::OneAttached => (
            Names::First(""),
            Typing::Gem("ActiveStorage::Attached::One"),
            READER_AND_WRITER,
        ),
        Installs::ManyAttached => (
            Names::First(""),
            Typing::Gem("ActiveStorage::Attached::Many"),
            READER_AND_WRITER,
        ),
        Installs::RichText => (
            Names::First(""),
            Typing::Gem("ActionText::RichText"),
            RICH_TEXT,
        ),
        Installs::DelegatedType => (Names::First(""), Typing::Fixed, DELEGATED_TYPE),
        Installs::Serialize => (Names::First(""), Typing::Written, SERIALIZE),
        Installs::Nothing | Installs::Elsewhere => (Names::All, Typing::Fixed, DECLARES_NOTHING),
    };
    Row {
        names,
        typing,
        shapes,
    }
}

/// One name a call gave, and the member name it produces.
#[derive(Debug)]
struct Named {
    /// What the members are built around — the key with its `prefix:` and `suffix:` applied.
    member: String,
    /// What the call wrote, for the provenance line.
    wrote: String,
    /// The span of what the call wrote.
    at: (u32, u32),
}

impl From<(String, (u32, u32))> for Named {
    fn from((wrote, at): (String, (u32, u32))) -> Self {
        Self {
            member: wrote.clone(),
            wrote,
            at,
        }
    }
}

/// The body a macro was written in: who owns what it declares, and what it already answers.
///
/// Three fields rather than three arguments, because every one of them is a property of the
/// *body* and none of them is a property of the call — and [`Tail::declare`] and
/// [`Tail::declare_derived`] both need all three.
pub(super) struct Host<'body> {
    /// The class or module the macro is written in, spelled with its nesting.
    pub(super) class: &'body str,
    /// Whether that body is a `module`, which decides both halves' [`Owner`].
    pub(super) module: bool,
    /// Every `def` the body writes itself, by `(is a def self., name)`. See
    /// `ModelClass::defined` for why this reader asks and no other one does.
    pub(super) defined: &'body BTreeSet<(bool, String)>,
}

/// One long-tail macro call, read.
#[derive(Debug)]
pub(super) struct Tail {
    /// The macro as written, for the provenance line. Twenty-nine names reach seventeen families,
    /// so
    /// the spelling cannot be recovered from [`Installs`]: `cattr_accessor` is a
    /// [`Installs::ModuleAccessor`] and is not a `mattr_accessor`.
    spelled: String,
    installs: Installs,
    /// Every name this call gave a member for: the name the members are built around, the text
    /// the call actually wrote, and its span.
    ///
    /// The two differ for exactly one family. `store_accessor :settings, :color, prefix: true`
    /// installs `settings_color`, and a provenance line reading ``store_accessor :settings_color``
    /// would quote a call nobody made — so the member is named from the first and the sentence
    /// under it from the second.
    names: Vec<Named>,
    /// The class the call names, for [`Typing::OwnClass`] and [`Typing::Gem`].
    class: Option<String>,
    /// The attribute an `alias_attribute` aliases.
    aliased: Option<String>,
    /// The `instance_*` keywords this call wrote as `false`.
    off: Vec<String>,
    /// Whether the name came from the macro's default rather than from the call.
    defaulted: bool,
    at: (u32, u32),
}

/// Read one call of a macro in [`LONG_TAIL`], or decline it.
///
/// `None` for a name that is not in the table, for a family that declares nothing, and — the
/// case that does the work — for a call this cannot take a **name** out of. A splat, a constant
/// and an interpolated string are all Ruby that only runs, and a member named from one of them
/// would be a member no `respond_to?` answers.
pub(super) fn read(source: &str, node: &CallNode<'_>, called: &str) -> Option<Tail> {
    let (_, installs) = LONG_TAIL.iter().find(|(name, _)| *name == called)?;
    if installs.declines() {
        return None;
    }
    let table = row(*installs);
    let message = node.message_loc()?;
    let at = header(node).unwrap_or((message.start_offset() as u32, message.end_offset() as u32));
    let (mut names, defaulted) = named(source, node, table.names, at)?;
    if let Names::Keys | Names::Accessors = table.names {
        let around = affixes(source, node)?;
        for named in &mut names {
            named.member = format!("{}{}{}", around.0, named.wrote, around.1);
        }
    }
    let aliased = matches!(table.typing, Typing::Aliased)
        .then(|| second(source, node))
        .flatten();
    if matches!(table.typing, Typing::Aliased) && aliased.is_none() {
        return None;
    }
    let class = match table.typing {
        Typing::Gem(name) => Some(name.to_owned()),
        Typing::OwnClass => Some(class_named(source, node, &names.first()?.member)?),
        // No `type:` is `Object`, and `Object` is every member there is — so the honest
        // spelling of it is the one RBS has for "this says nothing".
        Typing::Written => Some(
            keyword(node, "type")
                .map(|written| constant_spelling(source, &written))
                .filter(|spelled| !spelled.is_empty())
                .unwrap_or_else(|| "untyped".to_owned()),
        ),
        Typing::Fixed | Typing::Aliased => None,
    };
    Some(Tail {
        spelled: called.to_owned(),
        installs: *installs,
        names,
        class,
        aliased,
        off: turned_off(node, &table),
        defaulted,
        at,
    })
}

/// The names one call gives members for, and whether they came from the macro's default.
///
/// Empty is `None` rather than an empty list: a call this could read no name out of declares
/// nothing, and saying that once here is what keeps every caller below from checking.
fn named(
    source: &str,
    node: &CallNode<'_>,
    from: Names,
    at: (u32, u32),
) -> Option<(Vec<Named>, bool)> {
    let arguments: Vec<Node<'_>> = node
        .arguments()
        .map(|list| {
            list.arguments()
                .iter()
                .filter(|argument| argument.as_keyword_hash_node().is_none())
                .collect()
        })
        .unwrap_or_default();
    let one = |node: &Node<'_>| symbol_or_string(source, node).map(Named::from);
    let literals = |nodes: &[Node<'_>]| -> Vec<Named> {
        let mut found = Vec::new();
        for node in nodes {
            match node.as_array_node() {
                // `store_accessor :settings, [ :a, :b ]` — Rails calls `keys.flatten`, so an
                // array literal and a list of symbols are one spelling with two shapes.
                Some(array) => {
                    found.extend(array.elements().iter().filter_map(|element| one(&element)));
                }
                None => found.extend(one(node)),
            }
        }
        found
    };
    let found = match from {
        Names::All => literals(&arguments),
        // The **first** argument specifically, and not the first literal among them: the second
        // is the attribute this aliases, so `alias_attribute NAME, :title` reading past an
        // unspellable first name would declare `title` as an alias of itself — three members
        // the class already has, on a line that aliases nothing.
        Names::Alias => arguments.first().and_then(one).into_iter().collect(),
        Names::First(default) => match arguments.first() {
            Some(first) => literals(std::slice::from_ref(first)),
            // Rails' own parameter default, and the only way a name is ever invented here: the
            // macro really does install `regenerate_token` for a `has_secure_token` that named
            // nothing, and the span is the macro's own word because no name was written.
            None if !default.is_empty() => {
                return Some((vec![Named::from((default.to_owned(), at))], true));
            }
            None => Vec::new(),
        },
        Names::Keys => literals(arguments.get(1..).unwrap_or_default()),
        Names::Accessors => keyword(node, "accessors")
            .map(|value| literals(std::slice::from_ref(&value)))
            .unwrap_or_default(),
    };
    (!found.is_empty()).then_some((found, false))
}

/// The second positional literal — the attribute an `alias_attribute` aliases.
fn second(source: &str, node: &CallNode<'_>) -> Option<String> {
    let (name, _) = symbol_or_string(source, &node.arguments()?.arguments().iter().nth(1)?)?;
    Some(name)
}

/// The class a [`Typing::OwnClass`] call names: `class_name:`, or the name camelized.
fn class_named(source: &str, node: &CallNode<'_>, name: &str) -> Option<String> {
    match keyword(node, "class_name") {
        Some(written) => Some(symbol_or_string(source, &written)?.0),
        None => camelize(name),
    }
}

/// What a `store_accessor`'s `prefix:` and `suffix:` put around every key.
///
/// `None` declines the **whole call**, and that is the one place in this file where an option it
/// cannot read costs every member rather than one: a prefix changes what each accessor is
/// *called*, so reading the keys and ignoring it declares a set of names none of which exists.
fn affixes(source: &str, node: &CallNode<'_>) -> Option<(String, String)> {
    let store = node
        .arguments()
        .and_then(|list| list.arguments().iter().next())
        .and_then(|first| symbol_or_string(source, &first))
        .map(|(name, _)| name)
        .unwrap_or_default();
    let written = |name: &str| -> Option<String> {
        match keyword(node, name) {
            None => Some(String::new()),
            Some(value) if value.as_false_node().is_some() || value.as_nil_node().is_some() => {
                Some(String::new())
            }
            // `prefix: true` is Rails' shorthand for the store column's own name.
            Some(value) if value.as_true_node().is_some() => Some(store.clone()),
            Some(value) => symbol_or_string(source, &value).map(|(written, _)| written),
        }
    };
    let prefix = written("prefix")?;
    let suffix = written("suffix")?;
    Some((
        if prefix.is_empty() {
            prefix
        } else {
            format!("{prefix}_")
        },
        if suffix.is_empty() {
            suffix
        } else {
            format!("_{suffix}")
        },
    ))
}

/// Which `instance_*` keywords this call wrote as `false`.
///
/// Written *and* written as the literal, which is the difference between reading an option and
/// guessing at one: `instance_writer: options[:writer]` is Ruby that only runs, and a call that
/// writes it keeps the member rather than losing it.
fn turned_off(node: &CallNode<'_>, table: &Row) -> Vec<String> {
    let mut off = Vec::new();
    for shape in table.shapes {
        for name in shape.off {
            if !off.iter().any(|held| held == name)
                && keyword(node, name).is_some_and(|value| value.as_false_node().is_some())
            {
                off.push((*name).to_owned());
            }
        }
    }
    off
}

impl Tail {
    /// Whether this call's type is a fact some other document states — the pass's second phase.
    ///
    /// One family, and it is `alias_attribute`: `alias_attribute :sent_at, :created_at` needs
    /// `created_at`'s type, which is a **column**, written into the schema's generated document
    /// in this same pass with nothing resolved and nothing indexed. It is the question a
    /// `delegate` asks twice, asked once, and the second consumer of [`Facts::returns`].
    pub(super) fn derived(&self) -> bool {
        matches!(row(self.installs).typing, Typing::Aliased)
    }

    /// Every column this call re-types, so the schema can decline to declare it.
    ///
    /// One family, and it is `serialize`. The column stays where it is and the *type* it answers
    /// with is replaced, which is `attributes.rb`'s sentence for `attribute` and
    /// `attribute_methods/serialization.rb`'s behaviour for this one — and, unlike either of
    /// them, it is a re-type that is right even when the new type is `untyped`: a `text` column
    /// carrying YAML answers a `Hash` or an `Array` in Ruby and never the `String` the schema
    /// says. Withdrawing it removes an answer that is known to be wrong.
    /// **`store` withdraws nothing, and that is measured rather than forgotten.** Rails
    /// implements it by calling `serialize` on the store column, so in principle it re-types one
    /// too — but both of the corpus' two `store` columns are `json`/`jsonb`, which is not one of
    /// [`super::COLUMN_TYPES`]' ten and which the schema therefore already declares `untyped`.
    /// Withdrawing there would remove a member and put nothing in its place, where withdrawing a
    /// `text` column removes a `String` that is known to be wrong.
    pub(super) fn retypes(&self) -> impl Iterator<Item = &str> {
        matches!(self.installs, Installs::Serialize)
            .then(|| self.names.iter().map(|named| named.member.as_str()))
            .into_iter()
            .flatten()
    }

    /// Say every member this call installs, or decline the ones it cannot type.
    ///
    /// `known` is every class the application defines — [`Typing::OwnClass`]' gate, and
    /// [`super::ASSOCIATIONS`]' — and `framework` is the subset of the classes a **gem**
    /// declares that this workspace's bundle actually has. A `has_one_attached` in a project
    /// with no Active Storage names `ActiveStorage::Attached::One`, which is a class no jump can
    /// reach and no chain can continue through, so the call declares nothing at all rather than
    /// a member typed as a name that is not in the graph.
    pub(super) fn declare(
        &self,
        facts: &mut Facts,
        file: &str,
        host: &Host<'_>,
        known: &BTreeSet<String>,
        framework: &BTreeSet<String>,
    ) {
        if self.derived() {
            return;
        }
        let table = row(self.installs);
        let named = match (&self.class, table.typing) {
            (Some(name), Typing::OwnClass) if known.contains(name) => name.clone(),
            (Some(name), Typing::Gem(_)) if framework.contains(name) => name.clone(),
            (Some(name), Typing::Written) => name.clone(),
            (_, Typing::Fixed) => String::new(),
            // A class this workspace cannot name is the same answer a misspelled association
            // gets, and it is the whole call rather than one member: the reader is the macro's
            // reason to exist and a writer without it is a member nothing can be assigned to.
            _ => return,
        };
        self.emit(facts, file, host, &table, &named);
    }

    /// The second phase, for the one family that needs it.
    ///
    /// `project` is the union of everything phase one said, and what it is asked is the aliased
    /// attribute's own type on this same class. **What it answers is never a reason to decline**
    /// — an `alias_attribute` derives no *name*, because `attribute_aliases` installs the
    /// pattern set whatever the old name turns out to be, so an alias of something nothing typed
    /// is `untyped` and still a member: the type declines, the name never does.
    pub(super) fn declare_derived(
        &self,
        facts: &mut Facts,
        file: &str,
        owner: &Owner,
        project: &Facts,
        defined: &BTreeSet<(bool, String)>,
    ) {
        // The one guard, and it is the alias' target rather than the family: `read` records a
        // target for exactly the family phase two owns, so a `class_attribute` in the same class
        // body reaches here and leaves without a second test having to say so.
        let Some(aliased) = self.aliased.as_deref() else {
            return;
        };
        let table = row(self.installs);
        let returns = project
            .returns(owner, aliased)
            .unwrap_or("untyped")
            .to_owned();
        let host = Host {
            class: owner.name(),
            module: matches!(owner, Owner::Module(_) | Owner::ModuleSingleton(_)),
            defined,
        };
        self.emit(facts, file, &host, &table, &returns);
    }

    /// One member per [`Shape`] per name, minus the ones this call turned off.
    fn emit(
        &self,
        facts: &mut Facts,
        file: &str,
        host: &Host<'_>,
        table: &Row,
        // `named` is the one class this call names, already through whichever gate `Typing` set.
        // Empty for a `Typing::Fixed` family, which is why no shape in one may carry a
        // `Returns::Named` — `no_fixed_family_names_a_class` is that invariant.
        named: &str,
    ) {
        for Named { member, wrote, at } in &self.names {
            for shape in table.shapes {
                if shape
                    .off
                    .iter()
                    .any(|off| self.off.iter().any(|had| had == off))
                {
                    continue;
                }
                let spelled = match shape.name {
                    Affix::Around(before, after) => format!("{before}{member}{after}"),
                    // Installed only for the macro's own default, which is the one thing the
                    // call did not write down.
                    Affix::Default(literal) if self.defaulted => literal.to_owned(),
                    Affix::Default(_) => continue,
                };
                // The body's own `def` wins, and `ModelClass::defined` says why this reader asks
                // and no other one does.
                if host.defined.contains(&(shape.singleton, spelled.clone())) {
                    continue;
                }
                let returns = match shape.returns {
                    Returns::Fixed(written) => written.to_owned(),
                    Returns::Named => named.to_owned(),
                };
                let owner = match (shape.singleton, host.module) {
                    (false, false) => Owner::Instance(host.class.to_owned()),
                    (false, true) => Owner::Module(host.class.to_owned()),
                    (true, false) => Owner::Singleton(host.class.to_owned()),
                    (true, true) => Owner::ModuleSingleton(host.class.to_owned()),
                };
                facts.declare(Declared {
                    owner,
                    name: spelled,
                    returns,
                    parameters: shape.parameters.to_owned(),
                    because: format!("From `{file}`, `{} :{wrote}`.", self.spelled),
                    at: Some((self.at, *at)),
                    from: Source::Derived,
                    overloads: Vec::new(),
                });
            }
        }
    }
}

/// The class a family names that a gem declares, when it names one.
///
/// Read off the same table everything else is, so [`super::framework_classes`] cannot fall out
/// of step with what [`row`] actually returns.
pub(super) fn gem_class(installs: Installs) -> Option<&'static str> {
    match row(installs).typing {
        Typing::Gem(name) => Some(name),
        Typing::Fixed | Typing::OwnClass | Typing::Aliased | Typing::Written => None,
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use crate::generated::declaring;
    use std::collections::BTreeSet;

    use super::super::{Elsewhere, LONG_TAIL, read_model};
    use super::Installs;
    use crate::generated::{Declared, Facts, Owner, Source};

    fn owned(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// Everything one body declares, with `Money` a class the application defines and Active
    /// Storage and Action Text in the bundle.
    fn rbs(body: &str) -> String {
        read_model(&format!("class Story < ApplicationRecord\n{body}end\n"))
            .signatures(
                "app/models/story.rb",
                &Elsewhere {
                    known: &owned(&["Story", "Money"]),
                    framework: &owned(&super::super::framework_classes()),
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]))
            .rbs
    }

    /// The RBS a concern's `mattr_accessor` declares, which is what [`Owner::ModuleSingleton`]
    /// exists for.
    fn module_rbs(body: &str) -> String {
        read_model(&format!("module Storyish\n{body}end\n"))
            .signatures(
                "app/models/concerns/storyish.rb",
                &Elsewhere {
                    known: &owned(&["Storyish"]),
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]))
            .rbs
    }

    /// What one call of every family declares, pinned as a document.
    ///
    /// Seventeen families and seventeen expected blocks, asserted whole: what a macro installs
    /// is a claim about Rails, and a `contains` would let one
    /// of them grow a member nobody meant. Each was checked against the framework's own source
    /// at `7ba5fa3` — `synthesized.md` names the file for each.
    #[test]
    fn the_rbs_each_family_declares() {
        let expected: [(&str, &str); 17] = [
            (
                "  class_attribute :setting\n",
                "  def self.setting: () -> untyped
  def self.setting=: (untyped) -> untyped
  def self.setting?: () -> bool
  def setting: () -> untyped
  def setting=: (untyped) -> untyped
  def setting?: () -> bool
",
            ),
            (
                "  mattr_reader :pam\n",
                "  def self.pam: () -> untyped\n  def pam: () -> untyped\n",
            ),
            (
                "  cattr_writer :pam\n",
                "  def self.pam=: (untyped) -> untyped\n  def pam=: (untyped) -> untyped\n",
            ),
            (
                "  thread_mattr_accessor :pam\n",
                "  def self.pam: () -> untyped
  def pam: () -> untyped
  def self.pam=: (untyped) -> untyped
  def pam=: (untyped) -> untyped
",
            ),
            (
                "  accepts_nested_attributes_for :author, :pages\n",
                "  def author_attributes=: (untyped) -> untyped\n  \
                 def pages_attributes=: (untyped) -> untyped\n",
            ),
            (
                "  store_accessor :settings, :color\n",
                "  def color: () -> untyped
  def color=: (untyped) -> untyped
  def color_changed?: () -> bool
",
            ),
            (
                "  store :settings, accessors: [ :color ], prefix: true\n",
                "  def settings_color: () -> untyped
  def settings_color=: (untyped) -> untyped
  def settings_color_changed?: () -> bool
",
            ),
            (
                "  serialize :codes, coder: YAML, type: Array\n",
                "  def codes: () -> Array\n",
            ),
            (
                "  serialize :blob, coder: YAML\n",
                "  def blob: () -> untyped\n",
            ),
            (
                "  has_secure_token\n",
                "  def regenerate_token: () -> bool\n",
            ),
            (
                "  has_secure_password :recovery\n",
                "  def recovery: () -> String?
  def recovery=: (untyped) -> untyped
  def recovery_confirmation: () -> String?
  def recovery_confirmation=: (untyped) -> untyped
  def recovery_challenge: () -> String?
  def recovery_challenge=: (untyped) -> untyped
  def recovery_salt: () -> String?
  def authenticate_recovery: (String) -> untyped
",
            ),
            (
                "  composed_of :balance, class_name: \"Money\"\n",
                "  def balance: () -> Money\n  def balance=: (untyped) -> untyped\n",
            ),
            (
                "  has_one_attached :avatar\n",
                "  def avatar: () -> ActiveStorage::Attached::One\n  \
                 def avatar=: (untyped) -> untyped\n",
            ),
            (
                "  has_many_attached :images\n",
                "  def images: () -> ActiveStorage::Attached::Many\n  \
                 def images=: (untyped) -> untyped\n",
            ),
            (
                "  has_rich_text :body\n",
                "  def body: () -> ActionText::RichText
  def body=: (untyped) -> untyped
  def body?: () -> bool
",
            ),
            (
                "  delegated_type :entryable, types: %w[Message Comment]\n",
                "  def entryable: () -> untyped
  def entryable_class: () -> Class
  def entryable_name: () -> String
  def build_entryable: (*untyped) -> untyped
  def self.entryable_types: () -> Array[String]
",
            ),
            // The sixteenth family is `alias_attribute` and it is phase two's; the seventeenth
            // row is the four that read and decline, one of which stands for all of them here
            // and all four of which `every_macro_the_table_names_is_read` asks about.
            ("  normalizes :email, with: ->(e) { e }\n", ""),
        ];
        for (call, declared) in expected {
            assert_eq!(without_provenance(&rbs(call)), declared, "{call}");
        }
    }

    /// The document with its provenance comments stripped, and the `class Story` around it.
    ///
    /// The comment above every declaration is one sentence written by one `format!` and asserted
    /// whole in [`the_provenance_quotes_the_call_that_was_written`]; repeating it above sixty
    /// expected `def`s would pin the same string sixty times and hide what each family installs.
    fn without_provenance(rendered: &str) -> String {
        rendered
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .filter(|line| !line.starts_with("class ") && *line != "end")
            .fold(String::new(), |mut out, line| {
                out.push_str(line);
                out.push('\n');
                out
            })
    }

    /// One sentence per member, quoting the call the user really wrote.
    ///
    /// The `store` line is the reason this is its own test: the member is `settings_color` and
    /// the call wrote `:color`, so a provenance line built from the member name would quote a
    /// call that is not in the file.
    #[test]
    fn the_provenance_quotes_the_call_that_was_written() {
        assert_eq!(
            rbs("  store :settings, accessors: [ :color ], prefix: true\n"),
            "\
class Story
  # From `app/models/story.rb`, `store :color`.
  def settings_color: () -> untyped
  # From `app/models/story.rb`, `store :color`.
  def settings_color=: (untyped) -> untyped
  # From `app/models/story.rb`, `store :color`.
  def settings_color_changed?: () -> bool
end
"
        );
    }

    /// Every name in the table declares something, or is one of the four that deliberately does
    /// not — and which of the two it is, is asserted here rather than left to the reader.
    #[test]
    fn every_macro_the_table_names_is_read() {
        for (called, installs) in LONG_TAIL {
            // One call written so that every family finds what it needs in it: two positional
            // names, an `accessors:` for `store`, a `types:` for `delegated_type` and a
            // `class_name:` for `composed_of`.
            let call = format!(
                "  {called} :thing, :other, accessors: [ :key ], types: [], \
                 class_name: \"Money\"\n"
            );
            let declared = rbs(&call);
            if installs.declines() {
                assert_eq!(declared, "", "{called} declared something");
            } else if installs == Installs::AliasAttribute {
                // Phase two's, and `an_alias_attribute_takes_its_type_from_the_attribute_it_aliases`
                // is where it is read; phase one saying nothing about it is the assertion here.
                assert_eq!(declared, "", "{called} declared in phase one");
            } else {
                assert!(!declared.is_empty(), "{called} declared nothing");
            }
        }
    }

    /// The bare `authenticate` is installed only when the call took the macro's own default.
    ///
    /// `secure_password.rb` aliases it `if attribute == :password`, so this is the one
    /// [`Affix::Default`] in the table and the one place a name is spelled without the call
    /// having written it. `has_secure_password :recovery` is in
    /// [`the_rbs_each_family_declares`] and has no `authenticate` in its expected block.
    #[test]
    fn the_bare_authenticate_is_only_installed_for_the_default_attribute() {
        let declared = without_provenance(&rbs("  has_secure_password\n"));
        assert!(
            declared.contains("  def authenticate: (String) -> untyped\n"),
            "{declared}"
        );
        assert!(
            declared.contains("  def authenticate_password: (String) -> untyped\n"),
            "{declared}"
        );
    }

    /// A family whose types are all written in the table may not carry a [`Returns::Named`].
    ///
    /// [`Tail::emit`] resolves that variant to the one class the call names, which a
    /// [`Typing::Fixed`] row never has — so a shape added to the wrong list would declare a
    /// member returning the empty string, which is RBS nothing can parse and which would cost
    /// the whole generated document.
    #[test]
    fn no_fixed_family_names_a_class() {
        for (called, installs) in LONG_TAIL {
            let table = super::row(installs);
            if matches!(table.typing, super::Typing::Fixed) {
                assert!(
                    table
                        .shapes
                        .iter()
                        .all(|shape| matches!(shape.returns, super::Returns::Fixed(_))),
                    "{called} has a type nothing resolves"
                );
            }
        }
    }

    /// A `def` in the same body wins, and the macro's member is not declared beside it.
    ///
    /// Solidus writes `mattr_accessor :user_class` in `module Spree` and a `def self.user_class`
    /// two lines under it. Rails' macro really does define `def self.user_class`, and the `def`
    /// then replaces it — so declaring both puts a line of Rails' beside the one that answers,
    /// and `Spree.user_class` is called 167 times in that repository. Ten members across five
    /// applications are shadowed this way and this is the largest of them.
    #[test]
    fn a_def_in_the_same_body_shadows_the_member_a_macro_would_install() {
        // Both sides, and each only on its own: `def self.pam` leaves the instance reader alone.
        let declared = without_provenance(&rbs(
            "  mattr_accessor :pam\n  def self.pam\n  end\n\n  def other=(value)\n  end\n\n  \
             mattr_accessor :other\n",
        ));
        assert_eq!(
            declared,
            "  def pam: () -> untyped
  def self.pam=: (untyped) -> untyped
  def pam=: (untyped) -> untyped
  def self.other: () -> untyped
  def other: () -> untyped
  def self.other=: (untyped) -> untyped
"
        );
        // Phase two asks the same question, which it has to: an `alias_attribute` and a `def` of
        // the aliased name is the same collision one document later.
        let model = read_model(
            "class Story\n  alias_attribute :sent_at, :created_at\n  def sent_at?\n  end\nend\n",
        );
        let derived = without_provenance(
            &model
                .derived("app/models/story.rb", &Facts::default())
                .render(&declaring(&[]))
                .rbs,
        );
        assert_eq!(
            derived,
            "  def sent_at: () -> untyped\n  def sent_at=: (untyped) -> untyped\n"
        );
    }

    /// A name this cannot spell is a member it would invent, so the call declares nothing.
    #[test]
    fn a_call_this_cannot_read_a_name_out_of_declares_nothing() {
        for call in [
            "  class_attribute\n",
            "  class_attribute(*names)\n",
            "  class_attribute NAME\n",
            "  accepts_nested_attributes_for \"#{prefix}_author\"\n",
            // `Names::Keys` — the first argument is the column, so a call with only one gives
            // no keys at all.
            "  store_accessor :settings\n",
            "  store :settings, coder: JSON\n",
            "  composed_of\n",
            "  has_one_attached\n",
            "  alias_attribute :new_name\n",
        ] {
            assert_eq!(rbs(call), "", "{call}");
        }
    }

    /// The four `instance_*` keywords, and the one shape that is not one of them.
    #[test]
    fn a_keyword_written_as_false_removes_the_member_it_names() {
        let declared = |call: &str| without_provenance(&rbs(call));
        assert!(!declared("  class_attribute :a, instance_reader: false\n").contains("\n  def a:"));
        assert!(!declared("  class_attribute :a, instance_writer: false\n").contains("  def a=:"));
        assert!(
            !declared("  class_attribute :a, instance_accessor: false\n").contains("\n  def a:")
        );
        // The predicate keyword removes *both* sides, which is `attribute.rb`'s own
        // `if instance_predicate` around the class-side branch as well as the instance one.
        let no_predicate = declared("  class_attribute :a, instance_predicate: false\n");
        assert!(!no_predicate.contains("def a?"), "{no_predicate}");
        assert!(!no_predicate.contains("def self.a?"), "{no_predicate}");
        // Written, but not written as the literal: Ruby that only runs keeps the member.
        assert!(
            declared("  class_attribute :a, instance_writer: options[:w]\n").contains("  def a=:")
        );
    }

    /// A `prefix:` this cannot read costs the whole call, and it is the only option that does.
    ///
    /// Every other keyword in this file removes one member or changes one type. A prefix changes
    /// what each accessor is *called*, so reading the keys and ignoring it declares a set of
    /// names none of which exists.
    #[test]
    fn a_store_prefix_this_cannot_read_declines_every_key() {
        assert_eq!(
            rbs("  store_accessor :settings, :color, prefix: PREFIX\n"),
            ""
        );
        assert_eq!(
            rbs("  store_accessor :settings, :color, suffix: SUFFIX\n"),
            ""
        );
        // `false` and `nil` are Rails' own "no affix", and the keys keep their own names.
        assert!(
            without_provenance(&rbs("  store_accessor :settings, :color, prefix: false\n"))
                .contains("  def color:")
        );
        assert!(
            without_provenance(&rbs("  store_accessor :settings, :color, suffix: nil\n"))
                .contains("  def color:")
        );
        assert!(
            without_provenance(&rbs("  store_accessor :settings, :color, suffix: :x\n"))
                .contains("  def color_x:")
        );
    }

    /// A gem that is not in the bundle declares nothing at all.
    ///
    /// Not a member typed `untyped` and not a member typed as a name nothing defines — the whole
    /// call, because the reader *is* the macro's reason to exist and a writer with no reader is
    /// a member nothing can be assigned to.
    #[test]
    fn an_attachment_declines_when_the_gem_is_not_in_the_bundle() {
        for call in [
            "  has_one_attached :avatar\n",
            "  has_many_attached :images\n",
            "  has_rich_text :body\n",
        ] {
            let declared = read_model(&format!("class Story\n{call}end\n"))
                .signatures(
                    "app/models/story.rb",
                    &Elsewhere {
                        known: &owned(&["Story"]),
                        ..Elsewhere::nothing()
                    },
                )
                .render(&declaring(&[]))
                .rbs;
            assert_eq!(declared, "", "{call}");
        }
    }

    /// A `composed_of` naming a class the application does not define declares nothing, which is
    /// [`ASSOCIATIONS`](super::super::ASSOCIATIONS)' rule reached by the same road — and the
    /// camelized name is the same fallback a `belongs_to` has.
    #[test]
    fn a_composed_of_is_gated_on_the_class_it_names() {
        assert_eq!(rbs("  composed_of :balance\n"), "");
        assert!(
            without_provenance(&rbs("  composed_of :money\n")).contains("  def money: () -> Money")
        );
        assert_eq!(rbs("  composed_of :balance, class_name: \"Nothing\"\n"), "");
    }

    /// A `mattr_accessor` in a concern hangs on the module — **both halves, in one body**.
    ///
    /// The whole of what [`Owner::ModuleSingleton`](crate::generated::Owner) exists for: the
    /// render key is `(is_module, name)`, so a plain `Owner::Singleton` here would open
    /// `class Storyish` beside the `module Storyish` the instance half opened, and RBS holds one
    /// declaration of a constant or the other.
    #[test]
    fn a_concerns_module_attribute_declares_both_sides_in_the_module_body() {
        assert_eq!(
            module_rbs("  mattr_accessor :pam\n"),
            "\
module Storyish
  # From `app/models/concerns/storyish.rb`, `mattr_accessor :pam`.
  def self.pam: () -> untyped
  # From `app/models/concerns/storyish.rb`, `mattr_accessor :pam`.
  def pam: () -> untyped
  # From `app/models/concerns/storyish.rb`, `mattr_accessor :pam`.
  def self.pam=: (untyped) -> untyped
  # From `app/models/concerns/storyish.rb`, `mattr_accessor :pam`.
  def pam=: (untyped) -> untyped
end
"
        );
    }

    /// `alias_attribute` is phase two's, and what it derives is a **type** and never a name.
    ///
    /// The type declines and the name never does: `attribute_aliases` installs the pattern
    /// set whatever the old name turns out to be, so an alias of something nothing typed is
    /// `untyped` and still three members.
    #[test]
    fn an_alias_attribute_takes_its_type_from_the_attribute_it_aliases() {
        // A second macro in the same body, so that phase two is asked about one it does not own
        // and leaves it alone — which is the guard `declare_derived` opens with.
        let model = read_model(
            "class Story\n  class_attribute :setting\n  alias_attribute :sent_at, :created_at\nend\n",
        );
        assert!(model.derives(), "the second phase has something to do");
        assert!(
            !model
                .signatures(
                    "app/models/story.rb",
                    &Elsewhere {
                        known: &owned(&["Story"]),
                        ..Elsewhere::nothing()
                    }
                )
                .render(&declaring(&[]))
                .rbs
                .contains("sent_at"),
            "phase one says nothing about an alias"
        );

        let mut project = Facts::default();
        project.declare(Declared {
            owner: Owner::Instance("Story".to_owned()),
            name: "created_at".to_owned(),
            returns: "Time".to_owned(),
            parameters: "()".to_owned(),
            because: String::new(),
            at: None,
            from: Source::Column,
            overloads: Vec::new(),
        });
        assert_eq!(
            without_provenance(
                &model
                    .derived("app/models/story.rb", &project)
                    .render(&declaring(&[]))
                    .rbs
            ),
            "  def sent_at: () -> Time
  def sent_at=: (untyped) -> untyped
  def sent_at?: () -> bool
"
        );
        // A first argument this cannot spell declares nothing — and it is the *first* argument
        // rather than the first literal among them, because reading past it would take `:title`
        // as the name and declare three members aliasing the attribute they already are.
        assert_eq!(
            read_model("class Story\n  alias_attribute NAME, :title\nend\n")
                .derived("app/models/story.rb", &project)
                .render(&declaring(&[]))
                .rbs,
            ""
        );
        // And with nothing in the project to derive from: the same three names, no type.
        assert_eq!(
            without_provenance(
                &model
                    .derived("app/models/story.rb", &Facts::default())
                    .render(&declaring(&[]))
                    .rbs
            ),
            "  def sent_at: () -> untyped
  def sent_at=: (untyped) -> untyped
  def sent_at?: () -> bool
"
        );
    }

    /// Only a `serialize` tells the schema to withdraw a column, and a `class_attribute` of the
    /// same name does not.
    #[test]
    fn only_a_serialize_re_types_a_column() {
        let model = read_model(
            "class Story\n  serialize :codes, coder: YAML\n  class_attribute :setting\n\
             \n  alias_attribute :sent_at, :created_at\nend\n",
        );
        assert_eq!(
            model.retyped_columns().collect::<Vec<_>>(),
            [("Story", "codes")]
        );
    }

    /// Every name in the table is in a family, and every family is reachable from the table.
    #[test]
    fn the_table_is_the_whole_of_what_is_read() {
        let mut families: Vec<Installs> = LONG_TAIL.iter().map(|(_, what)| *what).collect();
        families.sort_unstable_by_key(|what| format!("{what:?}"));
        families.dedup();
        assert_eq!(families.len(), 18, "{families:?}");
    }
}
