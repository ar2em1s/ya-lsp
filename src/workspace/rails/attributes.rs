//! `attribute`, and the two kinds of evidence that the call is Rails' at all.
//!
//! Each call names one member (and its writer). What is declared depends on the second positional
//! argument:
//! - **a symbol naming one of [`CAST_TYPES`]** (Rails' type registries, which a cast type is looked
//!   up in): the member, typed, on any host;
//! - **anything else, or nothing**: the member as `untyped`, and only on a host the project treats
//!   as a model (a model class or a module), and only where no column already holds the name.
//!
//! # `attribute` in a class body is not evidence that a method exists
//!
//! Three gems spell a macro `attribute`, and **only Rails' defines a method**:
//!
//! | who | what `attribute :name` does | second positional |
//! | --- | --- | --- |
//! | `ActiveModel::Attributes` / ActiveRecord | defines `name` and `name=` | the **cast type** |
//! | `active_model_serializers` | stores an `Attribute` in `_attributes_data` | an options hash |
//! | `jsonapi-serializer` | `alias_method :attribute, :attributes`, stores a `Scalar` | *another name* |
//!
//! Both serializer gems read the value off the record they serialize, so a serializer answers
//! `respond_to?` **false** for every name its own macro wrote (unless its author also wrote a
//! `def`, which rubydex already has). Declaring a member for those calls would declare a method
//! that does not exist, and serializers write most `attribute` calls.
//!
//! Two pieces of evidence separate them:
//! 1. **The cast type, by syntax.** `attribute :thing, :string` cannot be either serializer's call:
//!    `active_model_serializers` would `fetch` on a `Symbol` and raise, and `jsonapi-serializer`
//!    would read `:string` as a second attribute name. That is also why a type symbol ya-lsp cannot
//!    map carries no type: `attribute :name, :tag_line` **is** that gem's list form.
//! 2. **The host, when there is no cast type.** Without a cast type the syntax is the serializers'
//!    too, so the question moves to the class: the same admit list `has_many` uses.
//!
//! # What Rails says about the precedence
//!
//! `activerecord/lib/active_record/attributes.rb` settles it in two sentences:
//!
//! > Defines an attribute with a type on this model. **It will override the type of existing
//! > attributes if needed.** … If this parameter is not passed, **the previously defined type
//! > (if any) will be used**.
//!
//! So a written cast type *re-types its column*, exactly as an `enum` does: the attribute wins and
//! the column defers. The mechanism is the `enum`'s:
//! [`Model::retyped_columns`](super::Model::retyped_columns) tells the schema which columns to
//! withdraw, and no rank moves. The second sentence is why an untyped call over an existing column
//! declares nothing: deferring to the column is what Rails does.

use ruby_prism::{CallNode, Node};

use super::syntax::{header, symbol_or_string};
use super::{EITHER_TIME, TIME_WITH_ZONE};
use crate::generated::{Declared, Facts, Owner, Source};

/// The cast types `attribute` may name, and the class each reads as.
///
/// **Rails' type registries, not a migration's vocabulary.** A cast type is looked up in
/// `ActiveModel::Type`'s registry, which `ActiveRecord::Type`'s repeats and extends with `text` and
/// `json` (`activemodel/lib/active_model/type.rb`, `activerecord/lib/active_record/type.rb`). So
/// `:big_integer` and `:immutable_string` are cast types no schema writes, and `:bigint`, which a
/// schema writes, raises.
///
/// - **`datetime` rests on the host**, so its class is `None` here. On an ActiveRecord model Rails
///   converts it to the time zone, as it does a column ([`super::COLUMN_TYPES`]); on an
///   `ActiveModel::Attributes` class nothing does, and it is a `Time`. [`Attribute::declare`] is
///   told which it is.
/// - **`time` and `json` are left out**, though both are registered. Neither has a class to give
///   (`time` is converted only from Rails 5.1 on, and `json` holds whatever JSON decodes to), and
///   both are plausible second names in `jsonapi-serializer`'s list form, so they get the host gate
///   an unknown symbol gets.
const CAST_TYPES: [(&str, Option<&str>); 11] = [
    ("big_integer", Some("Integer")),
    ("binary", Some("String")),
    ("boolean", Some("bool")),
    ("date", Some("Date")),
    ("datetime", None),
    ("decimal", Some("BigDecimal")),
    ("float", Some("Float")),
    ("immutable_string", Some("String")),
    ("integer", Some("Integer")),
    ("string", Some("String")),
    ("text", Some("String")),
];

/// The cast type a call wrote, where it is one of [`CAST_TYPES`].
#[derive(Debug)]
pub(super) struct Cast {
    /// As written (`integer`), for the provenance line.
    written: String,
    /// The class it names (`Integer`), or `None` where [`CAST_TYPES`] cannot name one alone.
    returns: Option<&'static str>,
}

/// One `attribute` call.
#[derive(Debug)]
pub(super) struct Attribute {
    /// The member's name: `count`.
    name: String,
    /// The cast type, where one was written and this crate has a class for it.
    ///
    /// `None` is a call with no second positional argument, or one this crate has no class for. It
    /// says the **member** and nothing about the type: the same inversion `delegate` makes. The
    /// type is declined, never the name, because `ActiveModel::AttributeMethods` defines the pair
    /// whatever the cast is.
    cast: Option<Cast>,
    /// Whether the call wrote **any** type: a second positional that is not the keyword hash.
    ///
    /// Wider than [`Self::cast`]. `attribute :price, :money` and
    /// `attribute :price, Money::Type.new` name a type this crate has no class for, and Rails
    /// still replaces the column's with it, so the column's answer is wrong from then on.
    typed: bool,
    /// The whole `attribute ...` header, and the member's own name inside it.
    at: (u32, u32),
    name_at: (u32, u32),
}

/// Read one `attribute` call, or decline it.
///
/// `None` means no **name** could be taken out of the call: a member this could not spell. A
/// missing or unmappable cast type is not a decline here; it is `cast: None`, and
/// [`Attribute::declare`] decides what that is worth.
pub(super) fn read(source: &str, node: &CallNode<'_>) -> Option<Attribute> {
    let at = header(node)?;
    let mut arguments = node.arguments()?.arguments().iter();
    let (name, name_at) = symbol_or_string(source, &arguments.next()?)?;
    let written = arguments.next();
    Some(Attribute {
        name,
        typed: written
            .as_ref()
            .is_some_and(|written| written.as_keyword_hash_node().is_none()),
        cast: cast(source, written),
        at,
        name_at,
    })
}

/// The cast type a call's second positional argument names, when it names one.
///
/// **A symbol and nothing else.** Rails' `resolve_type_name` looks a `Symbol` up in the type
/// registry and uses anything else *as* the type object. So a string is not a spelling of a cast
/// type (as it is of a table name), and `attribute :thing, Types::Money.new` is a type only running
/// Ruby can resolve. The keyword hash of `attribute :thing, default: 1` arrives here too, and is
/// not a symbol either.
fn cast(source: &str, node: Option<Node<'_>>) -> Option<Cast> {
    let (written, _) = node
        .filter(|node| node.as_symbol_node().is_some())
        .and_then(|node| symbol_or_string(source, &node))?;
    let (_, returns) = CAST_TYPES.iter().find(|(kind, _)| *kind == written)?;
    Some(Cast {
        written,
        returns: *returns,
    })
}

impl Attribute {
    /// The member this call names.
    pub(super) fn name(&self) -> &str {
        &self.name
    }

    /// The column this call re-types, where it re-types one.
    ///
    /// Only a written type does. `attributes.rb` says a call without one uses "the previously
    /// defined type (if any)", which is the column, so an untyped call withdraws nothing. A type
    /// this crate cannot name still re-types ([`Self::typed`]): the column then answers nothing,
    /// where it answered wrong.
    pub(super) fn retypes(&self) -> Option<&str> {
        self.typed.then_some(self.name.as_str())
    }

    /// Say the member.
    ///
    /// A typed call is optional **whatever the column said**, for the reason
    /// [`Enum::declare`](super::enums::Enum::declare) gives for the same `?`: nothing in an
    /// `attribute` call says the value is present (an ActiveModel attribute is `nil` until
    /// assigned, and a `default:` can still be assigned `nil`), so over-admitting `nil` is never
    /// wrong. It also bounds what withdrawing a column can cost: `Integer?` is strictly weaker than
    /// the `Integer` a `null: false` column claimed, so re-typing never turns a right answer into a
    /// wrong one.
    ///
    /// `admitted` is the host gate, which only an untyped call needs. A cast type is a **shape**
    /// neither serializer gem's macro can produce. Without one there is no shape left, so the
    /// caller decides by the **host** (a `module`, or a class the project treats as a model) and by
    /// whether a column already holds the name.
    ///
    /// `zoned` says the host is an ActiveRecord model in a project that keeps Rails' time-zone
    /// default, where a `datetime` is converted as a column is. Anywhere else the member is
    /// declared with no type: a module may be included into either kind of class.
    pub(super) fn declare(
        &self,
        facts: &mut Facts,
        file: &str,
        owner: &Owner,
        admitted: bool,
        zoned: bool,
    ) {
        let Some(cast) = self.cast.as_ref() else {
            if !admitted {
                return;
            }
            // The name, never the type: `delegate`'s inversion again.
            // `ActiveModel::AttributeMethods` defines the pair whatever the cast is, and
            // `Types::harvest` **drops** `untyped`, so the member exists for resolution and nothing
            // is claimed about what it holds.
            for (name, parameters, returns) in [
                (self.name.clone(), "()", "untyped"),
                (format!("{}=", self.name), "(untyped)", "void"),
            ] {
                facts.declare(Declared {
                    owner: owner.clone(),
                    name,
                    returns: returns.to_owned(),
                    parameters: parameters.to_owned(),
                    because: format!(
                        "From `{file}`, `attribute :{}`, which says nothing about the type.",
                        self.name
                    ),
                    at: Some((self.at, self.name_at)),
                    from: Source::Attribute,
                    overloads: Vec::new(),
                    private: false,
                });
            }
            return;
        };
        // Only a [`ZONED`] type has no class of its own, and converted or not it is one of the two
        //.
        let class = cast
            .returns
            .unwrap_or(if zoned { TIME_WITH_ZONE } else { EITHER_TIME });
        let returns = if class.contains(" | ") {
            format!("{class} | nil")
        } else {
            format!("{class}?")
        };
        facts.declare(Declared {
            owner: owner.clone(),
            name: self.name.clone(),
            returns,
            parameters: "()".to_owned(),
            because: format!(
                "From `{file}`, `attribute :{}, :{}`.",
                self.name, cast.written
            ),
            at: Some((self.at, self.name_at)),
            from: Source::Attribute,
            overloads: Vec::new(),
            private: false,
        });
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use crate::analysis::testing::*;
    use crate::generated::declaring;
    use std::collections::BTreeSet;

    use super::super::{Elsewhere, read_model};

    fn owned(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn declarations(source: &str) -> String {
        on_a_host(source, &BTreeSet::new(), &BTreeSet::new())
    }

    /// `models` is the host test and `columns` is what the schemas already declared.
    fn on_a_host(
        source: &str,
        models: &BTreeSet<String>,
        columns: &BTreeSet<(String, String)>,
    ) -> String {
        read_model(source)
            .signatures(
                "app/models/story.rb",
                &Elsewhere {
                    known: &owned(&["Story", "RateLimitable"]),
                    models,
                    columns,
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]))
            .rbs
    }

    /// A class the project does **not** treat as a model: a serializer, where most `attribute`
    /// calls are.
    fn rbs(body: &str) -> String {
        declarations(&format!("class Story < ApplicationRecord\n{body}end\n"))
    }

    /// The same body on a class the project does treat as one.
    fn on_a_model(body: &str) -> String {
        on_a_host(
            &format!("class Story < ApplicationRecord\n{body}end\n"),
            &owned(&["Story"]),
            &BTreeSet::new(),
        )
    }

    /// The whole of what one `attribute` declares, pinned as a document.
    #[test]
    fn the_rbs_an_attribute_declares() {
        assert_eq!(
            rbs("  attribute :count, :integer\n"),
            "\
class Story
  # From `app/models/story.rb`, `attribute :count, :integer`.
  def count: () -> Integer?
end
"
        );
    }

    /// Every cast type in Rails' registries, and what it reads as on either kind of host.
    ///
    /// A `datetime` is time-zone converted on an ActiveRecord model and on nothing else, so only a
    /// model in a project that keeps the default gets a class for it. `:bigint` is a schema's word
    /// and no cast type: Rails raises on it, and here it is the untyped call it looks like.
    #[test]
    fn every_cast_type_and_what_it_reads_as() {
        let reads = |declared: String| {
            declared
                .lines()
                .find_map(|line| line.trim().strip_prefix("def thing: () -> "))
                .unwrap_or("(none)")
                .to_owned()
        };
        let unzoned = |body: &str| {
            read_model(&format!("class Story < ApplicationRecord\n{body}end\n"))
                .signatures(
                    "app/models/story.rb",
                    &Elsewhere {
                        known: &owned(&["Story"]),
                        models: &owned(&["Story"]),
                        zoned: false,
                        ..Elsewhere::nothing()
                    },
                )
                .render(&declaring(&[]))
                .rbs
        };
        let rows: Vec<(&str, String, String, String)> = super::CAST_TYPES
            .iter()
            .map(|(kind, _)| *kind)
            .chain(["bigint"])
            .map(|kind| {
                let body = format!("  attribute :thing, :{kind}\n");
                (
                    kind,
                    reads(rbs(&body)),
                    reads(on_a_model(&body)),
                    reads(unzoned(&body)),
                )
            })
            .collect();
        let expected: Vec<(&str, String, String, String)> = [
            ("big_integer", "Integer?", "Integer?", "Integer?"),
            ("binary", "String?", "String?", "String?"),
            ("boolean", "bool?", "bool?", "bool?"),
            ("date", "Date?", "Date?", "Date?"),
            (
                "datetime",
                "(ActiveSupport::TimeWithZone | Time | nil)",
                "ActiveSupport::TimeWithZone?",
                "(ActiveSupport::TimeWithZone | Time | nil)",
            ),
            ("decimal", "BigDecimal?", "BigDecimal?", "BigDecimal?"),
            ("float", "Float?", "Float?", "Float?"),
            ("immutable_string", "String?", "String?", "String?"),
            ("integer", "Integer?", "Integer?", "Integer?"),
            ("string", "String?", "String?", "String?"),
            ("text", "String?", "String?", "String?"),
            ("bigint", "(none)", "untyped", "untyped"),
        ]
        .into_iter()
        .map(|(kind, plain, model, unzoned)| {
            (kind, plain.to_owned(), model.to_owned(), unzoned.to_owned())
        })
        .collect();
        assert_eq!(rows, expected);
    }

    /// The serializer gems in one test, on a host that is not a model.
    /// - A type object and a bare `default:` are `active_model_serializers`' options hash and
    ///   Rails' own untyped form: the same syntax.
    /// - A second symbol that is not a cast type is `jsonapi-serializer`'s list form.
    /// - A string is not a spelling of a cast type at all.
    ///
    /// None of them says anything about the **type**, and on a serializer none of them says
    /// anything at all.
    #[test]
    fn a_call_that_does_not_name_a_cast_type_declares_nothing_on_a_serializer() {
        for call in [
            "  attribute :thing, Types::Money.new\n",
            "  attribute :thing, \"integer\"\n",
            "  attribute :thing, default: 1\n",
            "  attribute :thing, key: :other\n",
            "  attribute :thing\n",
            "  attribute :thing, :json\n",
            "  attribute :name, :tag_line, :summary\n",
            "  attribute :thing do\n  end\n",
        ] {
            assert_eq!(rbs(call), "", "{call}");
        }
    }

    /// On a host that really is Rails', the same calls declare the **member**.
    ///
    /// The type is declined, never the name. `ActiveModel::AttributeMethods` defines the pair
    /// whatever the cast is, and `Types::harvest` drops `untyped`, so the member exists for
    /// resolution and nothing is claimed about what it holds.
    #[test]
    fn a_call_that_names_no_cast_type_still_names_a_member_on_a_model() {
        for call in [
            "  attribute :thing, Types::Money.new\n",
            "  attribute :thing, \"integer\"\n",
            "  attribute :thing, default: 1\n",
            "  attribute :thing, key: :other\n",
            "  attribute :thing\n",
            "  attribute :thing, :json\n",
            "  attribute :thing do\n  end\n",
        ] {
            let declared = on_a_model(call);
            assert!(declared.contains("def thing: () -> untyped\n"), "{call}");
            assert!(
                declared.contains("def thing=: (untyped value) -> void\n"),
                "{call}"
            );
        }
        // And a name it cannot read is still no member: the half the host test does not change.
        assert_eq!(on_a_model("  attribute(*names)\n"), "");
    }

    /// The column wins, and it wins from **another document**.
    ///
    /// `attributes.rb` says a call with no cast type uses "the previously defined type (if any)",
    /// which is the column, so the untyped member would be strictly worse. The two would sit in the
    /// model's generated document and the schema's, where two `def note:` lines are a silent
    /// overload set typed by whichever was harvested last.
    #[test]
    fn an_untyped_attribute_declines_to_a_column_of_the_same_name() {
        let columns: BTreeSet<(String, String)> = [("Story".to_owned(), "note".to_owned())]
            .into_iter()
            .collect();
        let declared = on_a_host(
            "class Story < ApplicationRecord\n  attribute :note\n  attribute :other\nend\n",
            &owned(&["Story"]),
            &columns,
        );
        assert!(!declared.contains("def note"), "{declared}");
        assert!(
            declared.contains("def other: () -> untyped\n"),
            "{declared}"
        );
    }

    /// A name that is not a literal is a member this cannot spell, so it declares nothing.
    #[test]
    fn a_call_this_cannot_read_a_name_out_of_declares_nothing() {
        for call in [
            "  attribute\n",
            "  attribute(*names)\n",
            "  attribute NAME, :integer\n",
        ] {
            assert_eq!(rbs(call), "", "{call}");
        }
    }

    /// A concern owns an `attribute` on the same terms a class does, unlike a `scope`.
    ///
    /// The member is one type whoever includes the module (`attribute :rate_limit, :boolean` is a
    /// `bool?` in every includer), whereas `scope :expired` is a different relation for each
    /// includer and is declined.
    #[test]
    fn a_concern_declares_its_attributes_on_the_module() {
        let rbs = declarations(
            "module RateLimitable\n  included do\n    attribute :rate_limit, :boolean\n  end\nend\n",
        );
        assert!(rbs.starts_with("module RateLimitable\n"), "{rbs}");
        assert!(rbs.contains("def rate_limit: () -> bool?"), "{rbs}");
    }

    /// Which columns the schema is told to withdraw; a module is not asked at all.
    ///
    /// A concern claims no table. The classes whose columns its `attribute` really re-types are its
    /// includers, which this pass cannot see.
    #[test]
    fn a_module_withdraws_no_column() {
        let model = read_model(
            "class Story < ApplicationRecord\n  attribute :price, :decimal\nend\n\
             module RateLimitable\n  included do\n    attribute :rate_limit, :boolean\n  end\nend\n",
        );
        assert_eq!(
            model.retyped_columns().collect::<Vec<_>>(),
            [("Story", "price")]
        );
    }

    /// A type this crate has no class for still replaces the column's, so the column is withdrawn;
    /// no type, or only keywords, keeps it.
    #[test]
    fn any_written_type_re_types_the_column() {
        let model = read_model(
            "class Story < ApplicationRecord\n  attribute :price, :money\n  \
             attribute :total, Money::Type.new\n  attribute :note\n  \
             attribute :count, default: 0\nend\n",
        );
        assert_eq!(
            model.retyped_columns().collect::<Vec<_>>(),
            [("Story", "price"), ("Story", "total")]
        );
    }

    /// `attribute`'s precedence, which Rails documents and which is easy to get backwards.
    ///
    /// `attributes.rb` says a cast type "will override the type of existing attributes if needed",
    /// and a call with no cast type keeps "the previously defined type". The two halves of this
    /// test are the two halves of that sentence.
    /// - `price` is re-typed and the schema withdraws its column: **one** `Story#price`, and a
    ///   chain that reaches the cast type, not the storage.
    /// - `note` names no type and the column already holds the name, so nothing is declared for it
    ///   and the column stays exactly where it was.
    #[test]
    fn an_attribute_re_types_the_column_it_overrides_and_defers_where_it_names_no_type() {
        let source = "Story.new.price.upcase\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let schema = harness.write(
            "db/schema.rb",
            "\
ActiveRecord::Schema[7.1].define(version: 2024_01_01_000000) do
  create_table \"stories\", force: :cascade do |t|
    t.integer \"price\", null: false
    t.string \"note\", null: false
  end
end
",
        );
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  attribute :price, :string\n  attribute :note\nend\n",
        );
        harness.watch(&[&schema, &story]);

        assert_eq!(
            harness.declarations_of("Story#price()"),
            1,
            "a column an `attribute` re-types is one declaration, not an overload"
        );
        let chained = card(&mut harness, &uri, source, "upcase");
        assert!(
            chained.contains("String#upcase"),
            "the cast type, not the integer it is stored as: {chained}"
        );
        // The other half: an `attribute` with no cast type over an existing column declares
        // nothing, so the column is the only declaration and still types the chain.
        assert_eq!(
            harness.declarations_of("Story#note()"),
            1,
            "an `attribute` that names no type leaves the column exactly where it was"
        );
        let note = "Story.new.note.upcase\n";
        let reads = harness.write("app/reads.rb", note);
        harness.watch(&[&reads]);
        let chained = card(&mut harness, &reads, note, "upcase");
        assert!(
            chained.contains("String#upcase"),
            "the column still types the chain: {chained}"
        );
    }

    /// The long tail's phase two: an alias takes the type of the column it aliases, across two
    /// generated documents.
    ///
    /// The second consumer of `Facts::returns`, and the shorter path: one hop where a `delegate`
    /// takes two. `title` is declared into `db/schema.rb`'s document and the alias into the
    /// model's, in the same pass, with nothing resolved and nothing indexed.
    #[test]
    fn an_alias_attribute_takes_the_type_of_the_column_it_aliases() {
        let source = "Story.new.headline.upcase\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  alias_attribute :headline, :title\nend\n",
        );
        harness.watch(&[&story]);

        assert!(
            harness.has("Story#headline?()"),
            "the pattern set, not only the reader"
        );
        let chained = card(&mut harness, &uri, source, "upcase");
        assert!(
            chained.contains("String#upcase"),
            "the aliased column's own type: {chained}"
        );
    }

    /// An `attribute` and a `def` of the same name are two places, and both are the user's own.
    ///
    /// `attribute :user_identifier, :string` with a `def user_identifier` under it gives the card a
    /// second place: the line that typed the member, named beside the line that wrote it. That is
    /// strictly better, even though a card containing "Defined in " twice can look like a
    /// name-based list to a sweep that counts places.
    #[test]
    fn an_attribute_beside_a_def_of_the_same_name_is_two_places() {
        let source = "Story.new.nickname\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  attribute :nickname, :string\n\n               def nickname\n    \"x\"\n  end\nend\n",
        );
        harness.watch(&[&story]);

        let card = card(&mut harness, &uri, source, "nickname");
        assert!(card.contains("Defined in 2 places"), "{card}");
    }
}
