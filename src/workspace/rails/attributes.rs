//! `attribute`, and the cast type that is the only evidence the call is Rails' at all.
//!
//! One name and one member, declared **only** where the second positional argument is a symbol
//! naming one of [`super::COLUMN_TYPES`] — the ten a migration writes, which is the registry
//! Rails looks a cast type up in.
//!
//! # `attribute` in a class body is not evidence that a method exists
//!
//! Three gems spell a macro `attribute`, and **only Rails' defines a method** — which is why this
//! reader is the narrowest in the directory:
//!
//! | who | what `attribute :name` does | second positional |
//! | --- | --- | --- |
//! | `ActiveModel::Attributes` / ActiveRecord | defines `name` and `name=` | the **cast type** |
//! | `active_model_serializers` | stores an `Attribute` in `_attributes_data` | an options hash |
//! | `jsonapi-serializer` | `alias_method :attribute, :attributes`, stores a `Scalar` | *another name* |
//!
//! Both serializer gems read the value off the record they are serializing, so a serializer
//! answers `respond_to?` **false** for every name its own macro wrote — unless its author also
//! wrote a `def` of that name, which is a definition rubydex already has. Declaring a member for
//! those calls would be declaring a method that does not exist, and they are the large majority.
//!
//! The cast type separates them by *syntax* rather than by a guess about the class.
//! `attribute :thing, :string` cannot be either serializer's call: `active_model_serializers`
//! would `fetch` on a `Symbol` and raise, and `jsonapi-serializer` would read `:string` as a
//! second attribute name. That is also why a type symbol ya-lsp cannot map is declined —
//! `attribute :name, :tag_line` **is** that gem's list form.
//!
//! The cost is a handful of real Rails attributes that name an unmapped type or no type at all,
//! and it is the direction every reader here picks: **the untyped half is worth less than
//! nothing**, because it is the shape both serializers write.
//!
//! # What Rails says about the precedence
//!
//! `activerecord/lib/active_record/attributes.rb` documents the macro in two sentences that
//! settle it:
//!
//! > Defines an attribute with a type on this model. **It will override the type of existing
//! > attributes if needed.** … If this parameter is not passed, **the previously defined type
//! > (if any) will be used**.
//!
//! So a written cast type *re-types its column*, exactly as an `enum` does — the attribute wins
//! and the column defers, not the other way round. The mechanism is the `enum`'s:
//! [`Model::retyped_columns`](super::Model::retyped_columns) tells the schema which columns to
//! withdraw, and no rank moves. The second sentence is why a call with no cast type costs so
//! little by declaring nothing: deferring to the column is what Rails does, and it is what
//! declining does.

use ruby_prism::{CallNode, Node};

use super::COLUMN_TYPES;
use super::syntax::{header, symbol_or_string};
use crate::generated::{Declared, Facts, Owner, Source};

/// The cast type a call wrote, where this crate has a class for it.
#[derive(Debug)]
pub(super) struct Cast {
    /// As written — `integer` — for the provenance line.
    written: String,
    /// The class it names: `Integer`.
    returns: &'static str,
}

/// One `attribute` call.
#[derive(Debug)]
pub(super) struct Attribute {
    /// The member's name: `count`.
    name: String,
    /// The cast type as written — `integer` — for the provenance line, where one was written.
    ///
    /// `None` is a call with no second positional argument, or one this crate has no class for.
    /// It says the **member** and nothing about the type, which is the same inversion `delegate`
    /// makes: what is declined is the type and never the name, because
    /// `ActiveModel::AttributeMethods` defines the pair whatever the cast is.
    cast: Option<Cast>,
    /// The whole `attribute ...` header, and the member's own name inside it.
    at: (u32, u32),
    name_at: (u32, u32),
}

/// Read one `attribute` call, or decline it.
///
/// Every `None` here is the same rule read from a different side: a call this cannot take a
/// **name** out of names a member it could not spell, and a call this cannot take a **cast type**
/// out of is not evidence that Rails' macro was the one written.
pub(super) fn read(source: &str, node: &CallNode<'_>) -> Option<Attribute> {
    let at = header(node)?;
    let mut arguments = node.arguments()?.arguments().iter();
    let (name, name_at) = symbol_or_string(source, &arguments.next()?)?;
    Some(Attribute {
        name,
        cast: cast(source, arguments.next()),
        at,
        name_at,
    })
}

/// The cast type a call's second positional argument names, when it names one.
///
/// **A symbol and nothing else.** Rails' `resolve_type_name` looks a `Symbol` up in the type
/// registry and uses anything else *as* the type object, so a string is not a spelling of a cast
/// type the way it is a spelling of a table name — and `attribute :thing, Types::Money.new` is a
/// type only Ruby that runs can resolve. The keyword hash of `attribute :thing, default: 1`
/// arrives here too and is not a symbol either.
fn cast(source: &str, node: Option<Node<'_>>) -> Option<Cast> {
    let (written, _) = node
        .filter(|node| node.as_symbol_node().is_some())
        .and_then(|node| symbol_or_string(source, &node))?;
    let (_, returns) = COLUMN_TYPES.iter().find(|(kind, _)| *kind == written)?;
    Some(Cast { written, returns })
}

impl Attribute {
    /// The member this call names.
    pub(super) fn name(&self) -> &str {
        &self.name
    }

    /// The column this call re-types, where it re-types one.
    ///
    /// Only a written cast type does. `attributes.rb` says a call without one uses "the
    /// previously defined type (if any)", which is the column — so an untyped call withdraws
    /// nothing and `retyped_columns` is unchanged by it.
    pub(super) fn retypes(&self) -> Option<&str> {
        self.cast.as_ref().map(|_| self.name.as_str())
    }

    /// Say the member.
    ///
    /// The type is **optional whatever the column said**, and the reason is the one
    /// [`Enum::declare`](super::enums::Enum::declare) gives for the same `?`: nothing in an
    /// `attribute` call says the value is present — an ActiveModel attribute is `nil` until it is
    /// assigned, and a `default:` can still be assigned `nil` — so over-admitting `nil` is the
    /// direction that is never wrong. It also bounds what withdrawing a column can cost:
    /// `Integer?` is a strictly weaker claim than the `Integer` a `null: false` column made, so
    /// re-typing can never turn a right answer into a wrong one.
    /// The host gate, and it is the one thing an untyped call needs that a typed one does not.
    ///
    /// A cast type is a **shape** neither serializer gem's macro can produce — 0 of 26 such
    /// calls in five corpora are on a serializer, because `active_model_serializers` would
    /// `fetch` on a `Symbol` and raise and `jsonapi-serializer` would read `:string` as an
    /// attribute called `string`. Without one there is no shape left, so the question moves to
    /// the **host**, which is the same admit list `has_many` uses: a `module`, or a class the
    /// project treats as a model.
    pub(super) fn declare(&self, facts: &mut Facts, file: &str, owner: &Owner, admitted: bool) {
        let Some(cast) = self.cast.as_ref() else {
            if !admitted {
                return;
            }
            // The name and never the type, which is `delegate`'s inversion arriving at a second
            // reader: `ActiveModel::AttributeMethods` defines the pair whatever the cast is, and
            // `Types::harvest` **drops** `untyped` — so the member exists for resolution and
            // nothing at all is claimed about what it holds.
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
                });
            }
            return;
        };
        facts.declare(Declared {
            owner: owner.clone(),
            name: self.name.clone(),
            returns: format!("{}?", cast.returns),
            parameters: "()".to_owned(),
            because: format!(
                "From `{file}`, `attribute :{}, :{}`.",
                self.name, cast.written
            ),
            at: Some((self.at, self.name_at)),
            from: Source::Attribute,
            overloads: Vec::new(),
        });
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
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

    /// A class the project does **not** treat as a model — a serializer, which is 142 of the
    /// corpus' 173 `attribute` calls.
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

    /// All ten of [`COLUMN_TYPES`], because the registry a cast type is looked up in is the one a
    /// migration writes into and a divergence between the two would be silent.
    #[test]
    fn every_column_type_is_a_cast_type() {
        for (kind, ruby) in super::COLUMN_TYPES {
            let declared = rbs(&format!("  attribute :thing, :{kind}\n"));
            assert!(
                declared.contains(&format!("def thing: () -> {ruby}?")),
                "{kind}: {declared}"
            );
        }
    }

    /// The serializer gems in one test, on a host that is not a model.
    ///
    /// A type object and a bare `default:` are `active_model_serializers`' options hash and
    /// Rails' own untyped form, which are the same syntax; a second symbol that is not a cast
    /// type is `jsonapi-serializer`'s list form; and a string is not a spelling of a cast type
    /// at all. None of them says anything about the **type**, and on a serializer — which is
    /// where 142 of the corpus' 173 calls are — none of them says anything at all.
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
    /// What is declined is the type and never the name. `ActiveModel::AttributeMethods` defines the pair whatever the cast
    /// is, and `Types::harvest` drops `untyped`, so the member exists for resolution and nothing
    /// at all is claimed about what it holds.
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
                declared.contains("def thing=: (untyped) -> void\n"),
                "{call}"
            );
        }
        // And a name it cannot read is still no member, which is the half the host test does
        // not change.
        assert_eq!(on_a_model("  attribute(*names)\n"), "");
    }

    /// The column wins, and it wins from **another document**.
    ///
    /// `attributes.rb` says a call with no cast type uses "the previously defined type (if
    /// any)", which is the column — so the untyped member would be a strictly worse answer, and
    /// the two are in the model's generated document and the schema's, where two `def note:`
    /// lines are a silent overload set and the type is whichever was harvested last.
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

    /// A concern owns an `attribute` on the same terms a class does, which a `scope` does not.
    ///
    /// A concern's rule read the other way: the member is one type whoever includes the module —
    /// `attribute :rate_limit, :boolean` is a `bool?` in every includer — where `scope :expired`
    /// is a different relation for each of them and is declined.
    #[test]
    fn a_concern_declares_its_attributes_on_the_module() {
        let rbs = declarations(
            "module RateLimitable\n  included do\n    attribute :rate_limit, :boolean\n  end\nend\n",
        );
        assert!(rbs.starts_with("module RateLimitable\n"), "{rbs}");
        assert!(rbs.contains("def rate_limit: () -> bool?"), "{rbs}");
    }

    /// Which columns the schema is told to withdraw, and a module is not asked at all.
    ///
    /// A concern claims no table, and the classes whose columns its `attribute` really does
    /// re-type are its includers, which this pass cannot see.
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
}
