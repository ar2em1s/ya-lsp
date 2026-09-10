//! `enum`, both of its spellings, and the 3 + 4N names one call installs.
//!
//! What Rails installs is read out of `activerecord/lib/active_record/enum.rb` rather than
//! remembered: `_enum` writes the attribute, its writer and `self.<name.pluralize>`, and
//! `define_enum_methods` writes `<label>?`, `<label>!`, `self.<label>` and `self.not_<label>` for
//! every value. The class-side pair really is `klass.scope`, so an `enum` feeds
//! [`super::models::Model::collections`] exactly as a `scope` does and reuses the relation class
//! and the model class side unchanged.
//!
//! # The two spellings, and the one Rails removed
//!
//! `enum(name, values = nil, **options)`, and `values, options = options, {} unless values`. So
//! `enum :status, { draft: 0 }` and `enum :status, draft: 0` are the same call written twice — in
//! the second there is no options hash at all, because the options hash *is* the values. The
//! older `enum status: { draft: 0 }` wore its options with a leading underscore (`_prefix`,
//! `_suffix`, `_scopes`, `_instance_methods`) and could define several enums in one call; Rails
//! 8.1 deleted it, and applications still carry it.
//!
//! # What is declined, and why each fails to nothing
//!
//! - **A values list that is not a literal** — `enum :locale, LANGUAGES_CONFIG.map { ... }.to_h`
//!   — declares the attribute and none of its values. Declining the whole call would be worse
//!   rather than safer: the three attribute names do not depend on the values, and the column
//!   underneath is an `Integer` that would otherwise be believed.
//! - **A label that is not a valid method name.** Rails installs `'ml-dsa-44': 2` under that
//!   exact name *and* under a transliterated alias; this declines both.
//! - **A `prefix:` or `suffix:` this cannot read** takes every value method with it, because the
//!   names would be wrong rather than missing. A `scopes:` or `instance_methods:` it cannot read
//!   is read as `false` for the same reason.

use ruby_prism::{AssocNode, CallNode, Node};

use super::inflect::pluralize;
use super::relation_of;
use super::syntax::{header, symbol_or_string};
use crate::generated::{Declared, Facts, Owner, Source};

/// One attribute one `enum` call declares.
#[derive(Debug)]
pub(super) struct Enum {
    /// The attribute: `status`.
    name: String,
    /// The whole `enum ...` header, and the attribute's own name inside it.
    at: (u32, u32),
    name_at: (u32, u32),
    /// Every value, in the order written. Empty is "the list said nothing this could read".
    labels: Vec<Label>,
    /// `scopes: false` drops the class-side pair; so does a `scopes:` this cannot read.
    scopes: bool,
    /// `instance_methods: false` drops the instance pair, the same way.
    instance_methods: bool,
}

/// One value, and the name Rails installs its four methods under.
#[derive(Debug)]
struct Label {
    /// The label as written: `draft`.
    value: String,
    /// Rails' `value_method_name` — `"#{prefix}#{label}#{suffix}"`.
    method: String,
    /// The whole `draft: 0`, and the label inside it.
    at: (u32, u32),
    name_at: (u32, u32),
}

/// The four options that change *what* an `enum` declares.
///
/// `default:` and `validate:` are the other two Rails takes and neither names a member, so
/// neither is here — which is the same rule that keeps `validates` out of [`super::MACROS`].
struct Options {
    /// The prefix and the suffix every label is wrapped in, or `None` when one of them is
    /// written and is not something this can read.
    affix: Option<(String, String)>,
    scopes: bool,
    instance_methods: bool,
}

impl Options {
    /// What a call with no options at all means.
    fn plain() -> Self {
        Self {
            affix: Some((String::new(), String::new())),
            scopes: true,
            instance_methods: true,
        }
    }

    /// The options a call wrote, under whichever spelling wears them.
    ///
    /// `under` is the underscore the older keyword form puts in front of every option name, so
    /// that `prefix: true` and `_prefix: true` are one reader and not two. Rails is strict about
    /// it in both directions — the modern form *raises* on `_prefix` — and so is this: a name
    /// spelled the other form's way is not an option here, it is a value or an unknown keyword.
    fn read(source: &str, name: &str, pairs: &[AssocNode<'_>], under: &str) -> Self {
        let written = |option: &str| {
            let want = format!("{under}{option}");
            pairs
                .iter()
                .find(|pair| {
                    symbol_or_string(source, &pair.key()).is_some_and(|(key, _)| key == want)
                })
                .map(|pair| pair.value())
        };
        Self {
            affix: affix(source, written("prefix"), name, false).zip(affix(
                source,
                written("suffix"),
                name,
                true,
            )),
            scopes: flag(written("scopes")),
            instance_methods: flag(written("instance_methods")),
        }
    }
}

/// Rails' `prefix = prefix == true ? "#{name}_" : "#{prefix}_"`, and its suffix twin.
///
/// `None` is "written, and not readable" — which the caller turns into an enum with no values
/// rather than into values under the wrong names.
fn affix(source: &str, value: Option<Node<'_>>, name: &str, trailing: bool) -> Option<String> {
    let Some(node) = value else {
        return Some(String::new());
    };
    if node.as_false_node().is_some() || node.as_nil_node().is_some() {
        return Some(String::new());
    }
    let written = if node.as_true_node().is_some() {
        name.to_owned()
    } else {
        symbol_or_string(source, &node)?.0
    };
    Some(if trailing {
        format!("_{written}")
    } else {
        format!("{written}_")
    })
}

/// An option that is on unless the call says otherwise — and off unless it says so in a literal.
fn flag(value: Option<Node<'_>>) -> bool {
    value.is_none_or(|node| node.as_true_node().is_some())
}

/// The `key => value` pairs of a hash written either with braces or without.
fn pairs<'pr>(node: &Node<'pr>) -> Option<Vec<AssocNode<'pr>>> {
    let elements = match (node.as_hash_node(), node.as_keyword_hash_node()) {
        (Some(hash), _) => hash.elements(),
        (_, Some(hash)) => hash.elements(),
        _ => return None,
    };
    Some(
        elements
            .iter()
            .filter_map(|element| element.as_assoc_node())
            .collect(),
    )
}

/// One value as it is written: the label, the whole pair or element, and the label's own span.
type Written = (String, (u32, u32), (u32, u32));

/// Every label a values list names.
///
/// `None` is a values list that is not a literal at all; an element inside a literal one that
/// cannot be read is dropped on its own, because the values beside it are still exactly what
/// Rails will install.
fn values(source: &str, node: &Node<'_>) -> Option<Vec<Written>> {
    let span = |node: &Node<'_>| {
        let location = node.location();
        (location.start_offset() as u32, location.end_offset() as u32)
    };
    if let Some(array) = node.as_array_node() {
        let elements: Vec<Node<'_>> = array.elements().iter().collect();
        return Some(
            elements
                .iter()
                .filter_map(|element| {
                    let (label, name_at) = symbol_or_string(source, element)?;
                    Some((label, span(element), name_at))
                })
                .collect(),
        );
    }
    Some(
        pairs(node)?
            .iter()
            .filter_map(|pair| {
                let (label, name_at) = symbol_or_string(source, &pair.key())?;
                Some((label, span(&pair.as_node()), name_at))
            })
            .collect(),
    )
}

/// Whether a name can be written as a `def` rather than only reached through `send`.
///
/// Rails does not require it — `define_method` takes anything — so this is the one place the
/// reader is deliberately narrower than the framework. The alternative is a generated `def` RBS
/// cannot parse, which `Synthesized::record` refuses *as a whole document*: one strange label
/// would cost every declaration in the file.
fn is_method_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Every attribute one `enum` call declares. Empty for a call this reader will not read.
pub(super) fn read(source: &str, node: &CallNode<'_>) -> Vec<Enum> {
    attributes(source, node).unwrap_or_default()
}

fn attributes(source: &str, node: &CallNode<'_>) -> Option<Vec<Enum>> {
    let at = header(node)?;
    let arguments: Vec<Node<'_>> = node.arguments()?.arguments().iter().collect();
    let first = arguments.first()?;

    // The older keyword form. Every pair that is not an option defines an enum of its own —
    // `enum status: { ... }, kind: { ... }` is two — and the options apply to all of them.
    if first.as_keyword_hash_node().is_some() {
        let (defined, options): (Vec<_>, Vec<_>) = pairs(first)?.into_iter().partition(|pair| {
            symbol_or_string(source, &pair.key()).is_none_or(|(key, _)| !key.starts_with('_'))
        });
        return Some(
            defined
                .iter()
                .filter_map(|pair| {
                    let (name, name_at) = symbol_or_string(source, &pair.key())?;
                    let options = Options::read(source, &name, &options, "_");
                    Some(one(source, at, name, name_at, &pair.value(), &options))
                })
                .collect(),
        );
    }

    let (name, name_at) = symbol_or_string(source, first)?;
    let list = arguments.get(1)?;
    // `enum :status, draft: 0` — Rails swaps the options hash into `values` when nothing was
    // passed positionally, so a call written this way has no options at all and every pair in it
    // is a value.
    let options = match list.as_keyword_hash_node() {
        Some(_) => Options::plain(),
        None => Options::read(
            source,
            &name,
            &arguments.get(2).and_then(pairs).unwrap_or_default(),
            "",
        ),
    };
    Some(vec![one(source, at, name, name_at, list, &options)])
}

/// One attribute, its values and the options around them, assembled.
fn one(
    source: &str,
    at: (u32, u32),
    name: String,
    name_at: (u32, u32),
    list: &Node<'_>,
    options: &Options,
) -> Enum {
    let labels = options
        .affix
        .as_ref()
        .map(|(prefix, suffix)| {
            values(source, list)
                .unwrap_or_default()
                .into_iter()
                .map(|(value, at, name_at)| Label {
                    method: format!("{prefix}{value}{suffix}"),
                    value,
                    at,
                    name_at,
                })
                .filter(|label| is_method_name(&label.method))
                .collect()
        })
        .unwrap_or_default();
    Enum {
        name,
        at,
        name_at,
        labels,
        scopes: options.scopes,
        instance_methods: options.instance_methods,
    }
}

impl Enum {
    /// The column this re-types, so the schema generator can decline to declare it.
    pub(super) fn attribute(&self) -> &str {
        &self.name
    }

    /// Whether this installs class-side scopes, and so needs a relation class to return.
    pub(super) fn scoped(&self) -> bool {
        self.scopes && !self.labels.is_empty()
    }

    /// Say the 3 + 4N names this call installs.
    ///
    /// `relation` is whether the class it is written on has a generated relation class — the
    /// same condition a `has_many` carries, and for the same reason: `Story.draft`
    /// returns a `Story::Relation`, and a project that wrote its own `Story::Relation` meant
    /// something by it. Without one the class-side pair is declined and the rest stands.
    pub(super) fn declare(&self, facts: &mut Facts, file: &str, class: &str, relation: bool) {
        let instance = Owner::Instance(class.to_owned());
        let singleton = Owner::Singleton(class.to_owned());
        let whole = Some((self.at, self.name_at));
        // The label and not the value it is stored as: `EnumType#deserialize` answers with the
        // key of the mapping, and the keys of a `HashWithIndifferentAccess` are strings. The `?`
        // is the column's — an enum call cannot see whether the column is `null: false`, and
        // over-admitting `nil` is the direction that is never wrong.
        facts.declare(Declared {
            owner: instance.clone(),
            name: self.name.clone(),
            returns: "String?".to_owned(),
            parameters: "()".to_owned(),
            because: format!(
                "From `{file}`, `enum :{}`. The label, not the stored value.",
                self.name
            ),
            at: whole,
            from: Source::Enum,
            overloads: Vec::new(),
        });
        facts.declare(Declared {
            owner: instance.clone(),
            name: format!("{}=", self.name),
            returns: "untyped".to_owned(),
            parameters: "(untyped)".to_owned(),
            because: format!(
                "From `{file}`, `enum :{}`. Takes a label or the value behind it.",
                self.name
            ),
            at: whole,
            from: Source::Enum,
            overloads: Vec::new(),
        });
        // `singleton_class.define_method(name.pluralize) { enum_values }`. The real class is an
        // `ActiveSupport::HashWithIndifferentAccess`, which no project without Rails in its
        // bundle has indexed; `Hash` is the class it subclasses, so every member it answers is
        // one this declares and none of them is invented.
        facts.declare(Declared {
            owner: singleton.clone(),
            name: pluralize(&self.name),
            returns: "Hash[String, untyped]".to_owned(),
            parameters: "()".to_owned(),
            because: format!(
                "From `{file}`, `enum :{}`. Every label, and the value it stores.",
                self.name
            ),
            at: whole,
            from: Source::Enum,
            overloads: Vec::new(),
        });

        for label in &self.labels {
            let at = Some((label.at, label.name_at));
            if self.instance_methods {
                facts.declare(Declared {
                    owner: instance.clone(),
                    name: format!("{}?", label.method),
                    returns: "bool".to_owned(),
                    parameters: "()".to_owned(),
                    because: format!(
                        "From `{file}`, `enum :{}`, value `{}`.",
                        self.name, label.value
                    ),
                    at,
                    from: Source::Enum,
                    overloads: Vec::new(),
                });
                facts.declare(Declared {
                    owner: instance.clone(),
                    name: format!("{}!", label.method),
                    returns: "bool".to_owned(),
                    parameters: "()".to_owned(),
                    because: format!(
                        "From `{file}`, `enum :{}`: sets it to `{}`.",
                        self.name, label.value
                    ),
                    at,
                    from: Source::Enum,
                    overloads: Vec::new(),
                });
            }
            if !(self.scopes && relation) {
                continue;
            }
            // `klass.scope value_method_name, -> { where(name => value) }`, and its `not_` twin.
            // The lambda takes nothing, so the parameter list is exact rather than the
            // `(*untyped)` a hand-written `scope` has to settle for.
            for (name, sense) in [
                (label.method.clone(), "is"),
                (format!("not_{}", label.method), "is not"),
            ] {
                facts.declare(Declared {
                    owner: singleton.clone(),
                    name,
                    returns: relation_of(class),
                    parameters: "()".to_owned(),
                    because: format!(
                        "From `{file}`, `enum :{}`: every record whose `{}` {sense} `{}`.",
                        self.name, self.name, label.value
                    ),
                    at,
                    from: Source::Enum,
                    overloads: Vec::new(),
                });
            }
        }
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

    /// One class in a file, with a relation class available to it.
    fn model(body: &str) -> String {
        format!("class Story < ApplicationRecord\n{body}end\n")
    }

    fn rbs(source: &str) -> String {
        declarations(source, &owned(&["Story"])).rbs
    }

    fn declarations(source: &str, relations: &BTreeSet<String>) -> crate::generated::Declarations {
        read_model(source)
            .signatures(
                "app/models/story.rb",
                &Elsewhere {
                    known: &owned(&["Story"]),
                    relations,
                    ..Elsewhere::nothing()
                },
            )
            .render(&declaring(&[]))
    }

    /// Every `def` in a rendering, in order, so a test can name what changed rather than pin it.
    fn names(source: &str) -> Vec<String> {
        rbs(source)
            .lines()
            .filter_map(|line| line.trim().strip_prefix("def "))
            .map(|line| line.split(':').next().unwrap_or_default().to_owned())
            .collect()
    }

    /// The whole of what one ordinary `enum` declares, pinned.
    ///
    /// Pinned as a document for the reason the schema's and the model's are: 3 + 4N is a claim
    /// about Rails, and asserting it one predicate at a time is how a change to the shape passes
    /// ten green tests. Every name here was read out of `_enum` and `define_enum_methods`.
    #[test]
    fn the_rbs_an_enum_declares() {
        assert_eq!(
            rbs(&model("  enum :status, { draft: 0, published: 1 }\n")),
            "\
class Story
  # From `app/models/story.rb`, `enum :status`. The label, not the stored value.
  def status: () -> String?
  # From `app/models/story.rb`, `enum :status`. Takes a label or the value behind it.
  def status=: (untyped) -> untyped
  # From `app/models/story.rb`, `enum :status`. Every label, and the value it stores.
  def self.statuses: () -> Hash[String, untyped]
  # From `app/models/story.rb`, `enum :status`, value `draft`.
  def draft?: () -> bool
  # From `app/models/story.rb`, `enum :status`: sets it to `draft`.
  def draft!: () -> bool
  # From `app/models/story.rb`, `enum :status`: every record whose `status` is `draft`.
  def self.draft: () -> Story::Relation
  # From `app/models/story.rb`, `enum :status`: every record whose `status` is not `draft`.
  def self.not_draft: () -> Story::Relation
  # From `app/models/story.rb`, `enum :status`, value `published`.
  def published?: () -> bool
  # From `app/models/story.rb`, `enum :status`: sets it to `published`.
  def published!: () -> bool
  # From `app/models/story.rb`, `enum :status`: every record whose `status` is `published`.
  def self.published: () -> Story::Relation
  # From `app/models/story.rb`, `enum :status`: every record whose `status` is not `published`.
  def self.not_published: () -> Story::Relation
end
"
        );
    }

    /// The spelling Rails 8.1 deleted, which one application in six carries all of.
    #[test]
    fn the_older_keyword_spelling_declares_the_same_names() {
        assert_eq!(
            names(&model("  enum status: { draft: 0, published: 1 }\n")),
            names(&model("  enum :status, { draft: 0, published: 1 }\n"))
        );
    }

    /// `enum status: { ... }, kind: { ... }` is two enums, because Rails loops over the hash.
    #[test]
    fn one_older_call_can_declare_several_enums() {
        let declared = names(&model("  enum status: { draft: 0 }, kind: { link: 0 }\n"));
        assert!(declared.contains(&"draft?".to_owned()), "{declared:?}");
        assert!(declared.contains(&"link?".to_owned()), "{declared:?}");
        assert!(
            declared.contains(&"self.statuses".to_owned()),
            "{declared:?}"
        );
        assert!(declared.contains(&"self.kinds".to_owned()), "{declared:?}");
    }

    /// `values, options = options, {} unless values` — so a call written without braces has no
    /// options at all, and a pair that looks like one is a value.
    #[test]
    fn a_braceless_hash_is_the_values_and_never_the_options() {
        let declared = names(&model("  enum :status, draft: 0, prefix: true\n"));
        assert!(declared.contains(&"draft?".to_owned()), "{declared:?}");
        assert!(declared.contains(&"prefix?".to_owned()), "{declared:?}");
        assert!(
            !declared.iter().any(|name| name.starts_with("status_")),
            "{declared:?}"
        );
    }

    /// `prefix == true ? "#{name}_" : "#{prefix}_"`, and the suffix twin, under both spellings.
    #[test]
    fn a_prefix_and_a_suffix_rename_every_value_method() {
        for (call, want) in [
            (
                "  enum :status, { draft: 0 }, prefix: true\n",
                "status_draft",
            ),
            ("  enum :status, { draft: 0 }, prefix: :s\n", "s_draft"),
            (
                "  enum :status, { draft: 0 }, suffix: true\n",
                "draft_status",
            ),
            ("  enum :status, { draft: 0 }, suffix: \"s\"\n", "draft_s"),
            (
                "  enum :status, { draft: 0 }, prefix: :a, suffix: :b\n",
                "a_draft_b",
            ),
            (
                "  enum status: { draft: 0 }, _prefix: true\n",
                "status_draft",
            ),
        ] {
            let declared = names(&model(call));
            assert!(declared.contains(&format!("{want}?")), "{call}{declared:?}");
            assert!(
                declared.contains(&format!("self.not_{want}")),
                "{call}{declared:?}"
            );
            // The attribute itself is never renamed — Rails wraps the *label*.
            assert!(
                declared.contains(&"status".to_owned()),
                "{call}{declared:?}"
            );
        }
    }

    /// An option is spelled one way per form, and Rails raises on the other. So does this.
    #[test]
    fn an_option_written_for_the_other_spelling_is_not_an_option() {
        // `_prefix` in the modern form is a keyword Rails rejects; here it is simply not read,
        // and the value methods keep their plain names.
        let declared = names(&model("  enum :status, { draft: 0 }, _prefix: true\n"));
        assert!(declared.contains(&"draft?".to_owned()), "{declared:?}");
        // `prefix` in the older form is another enum, exactly as Rails reads it.
        let declared = names(&model("  enum status: { draft: 0 }, prefix: { on: 0 }\n"));
        assert!(declared.contains(&"draft?".to_owned()), "{declared:?}");
        assert!(declared.contains(&"on?".to_owned()), "{declared:?}");
    }

    /// `scopes: false` drops the class side, `instance_methods: false` drops the instance side,
    /// and both leave the three names the attribute itself owns.
    #[test]
    fn the_two_options_that_take_names_away() {
        let declared = names(&model("  enum :status, { draft: 0 }, scopes: false\n"));
        assert!(declared.contains(&"draft?".to_owned()), "{declared:?}");
        assert!(!declared.contains(&"self.draft".to_owned()), "{declared:?}");
        assert!(
            declared.contains(&"self.statuses".to_owned()),
            "{declared:?}"
        );

        let declared = names(&model(
            "  enum :status, { draft: 0 }, instance_methods: false\n",
        ));
        assert!(!declared.contains(&"draft?".to_owned()), "{declared:?}");
        assert!(declared.contains(&"self.draft".to_owned()), "{declared:?}");
        assert!(declared.contains(&"status".to_owned()), "{declared:?}");
    }

    /// An option this cannot read is refused rather than approximated, and the two halves refuse
    /// differently on purpose: a prefix it cannot read would name every value method *wrongly*,
    /// where a `scopes:` it cannot read might only mean there are none.
    #[test]
    fn an_option_that_is_not_a_literal_is_declined() {
        let declared = names(&model("  enum :status, { draft: 0 }, prefix: SETTING\n"));
        assert_eq!(declared, ["status", "status=", "self.statuses"]);

        let declared = names(&model("  enum :status, { draft: 0 }, scopes: SETTING\n"));
        assert!(declared.contains(&"draft?".to_owned()), "{declared:?}");
        assert!(!declared.contains(&"self.draft".to_owned()), "{declared:?}");
    }

    /// Rails' `if prefix` — `false` and `nil` are no prefix, not the string "false".
    #[test]
    fn a_prefix_that_is_false_is_no_prefix() {
        for call in [
            "  enum :status, { draft: 0 }, prefix: false\n",
            "  enum :status, { draft: 0 }, suffix: nil\n",
        ] {
            assert!(names(&model(call)).contains(&"draft?".to_owned()), "{call}");
        }
    }

    /// `values.respond_to?(:each_pair) ? values.each_pair : values.each_with_index` — an array
    /// is a values list too, and its labels are its elements.
    #[test]
    fn an_array_of_values_is_read_like_a_hash_of_them() {
        assert_eq!(
            names(&model("  enum :status, [:draft, \"published\"]\n")),
            names(&model("  enum :status, { draft: 0, published: 1 }\n"))
        );
    }

    /// A non-literal values list declares the attribute, not nothing.
    ///
    /// The three names an attribute owns do not depend on its values at all, and the column
    /// underneath is the integer the labels are stored as — so declaring nothing here does not
    /// leave the answer absent, it leaves it wrong. `synthesized.md` has the argument.
    #[test]
    fn a_values_list_that_is_not_a_literal_still_declares_its_attribute() {
        for call in [
            "  enum :status, STATUSES\n",
            "  enum status: STATUSES\n",
            "  enum :status, LANGUAGES.map { |key| [key, key] }.to_h\n",
        ] {
            assert_eq!(
                names(&model(call)),
                ["status", "status=", "self.statuses"],
                "{call}"
            );
        }
    }

    /// A label Rails installs through `define_method` and this cannot write as a `def`.
    ///
    /// One label in the six corpora — mastodon's `'ml-dsa-44': 2` — and Rails also installs a
    /// transliterated alias for it, which this deliberately does not: see `synthesized.md`. The
    /// values beside a declined one are still declared, because Rails still installs them.
    #[test]
    fn a_label_that_cannot_be_written_as_a_def_is_declined() {
        let declared = names(&model(
            "  enum :kind, { rsa: 0, 'ml-dsa-44': 1, Ed25519: 2, \"\": 3, KEY => 4, _x: 5 }\n",
        ));
        assert_eq!(
            declared,
            [
                "kind",
                "kind=",
                "self.kinds",
                "rsa?",
                "rsa!",
                "self.rsa",
                "self.not_rsa",
                "_x?",
                "_x!",
                "self._x",
                "self.not__x",
            ]
        );
    }

    /// A prefix can make a legal label illegal, which is the case that says the check belongs
    /// after the affixes rather than before them.
    #[test]
    fn a_prefix_is_part_of_the_name_that_has_to_be_spellable() {
        assert_eq!(
            names(&model("  enum :status, { draft: 0 }, prefix: \"a-b\"\n")),
            ["status", "status=", "self.statuses"]
        );
    }

    /// Four ways an `enum` call says nothing this reader can use.
    #[test]
    fn an_enum_with_nothing_to_read_declares_nothing() {
        for call in [
            "  enum\n",
            "  enum :status\n",
            "  enum STATUSES\n",
            "  enum 1, { draft: 0 }\n",
        ] {
            assert_eq!(names(&model(call)), Vec::<String>::new(), "{call}");
        }
    }

    /// The class-side pair needs a relation class, the same way a `has_many` does.
    #[test]
    fn without_a_relation_class_the_scopes_are_declined() {
        let declared = declarations(&model("  enum :status, { draft: 0 }\n"), &BTreeSet::new());
        assert!(declared.rbs.contains("def draft?"), "{}", declared.rbs);
        assert!(!declared.rbs.contains("def self.draft"), "{}", declared.rbs);
    }

    /// An `enum` makes its own class a collection, because its scopes return one.
    #[test]
    fn an_enum_asks_for_a_relation_class_and_a_scopeless_one_does_not() {
        let nothing = BTreeSet::new();
        let has = |body: &str| {
            read_model(&model(body))
                .collections(&nothing)
                .any(|element| element == "Story")
        };
        assert!(has("  enum :status, { draft: 0 }\n"));
        assert!(!has("  enum :status, { draft: 0 }, scopes: false\n"));
        assert!(!has("  enum :status, STATUSES\n"));
    }

    /// The columns the schema has to decline, which is the whole of what this tells another
    /// generator.
    #[test]
    fn the_attributes_an_enum_re_types() {
        let model = read_model(&model(
            "  enum :status, { draft: 0 }\n  enum kind: STATUSES\n",
        ));
        assert_eq!(
            model.retyped_columns().collect::<Vec<_>>(),
            [("Story", "status"), ("Story", "kind")]
        );
    }

    /// Where each jump lands: the attribute's three names on the call, and a value's four on the
    /// value.
    #[test]
    fn where_a_generated_name_says_it_was_declared() {
        let source = model("  enum :status, { draft: 0, published: 1 }\n");
        let declared = declarations(&source, &owned(&["Story"]));
        let at = |span: (u32, u32)| &source[span.0 as usize..span.1 as usize];
        let places: Vec<(&str, &str)> = declared
            .spans
            .iter()
            .map(|span| (at(span.declared), at(span.selection)))
            .collect();
        assert_eq!(
            places,
            [
                ("enum :status, { draft: 0, published: 1 }", "status"),
                ("enum :status, { draft: 0, published: 1 }", "status"),
                ("enum :status, { draft: 0, published: 1 }", "status"),
                ("draft: 0", "draft"),
                ("draft: 0", "draft"),
                ("draft: 0", "draft"),
                ("draft: 0", "draft"),
                ("published: 1", "published"),
                ("published: 1", "published"),
                ("published: 1", "published"),
                ("published: 1", "published"),
            ]
        );
    }

    /// An array's elements are their own selection, so the jump lands on the label either way.
    #[test]
    fn an_array_value_is_its_own_selection() {
        let source = model("  enum :status, [:draft]\n");
        let declared = declarations(&source, &owned(&["Story"]));
        let at = |span: (u32, u32)| &source[span.0 as usize..span.1 as usize];
        let last = declared.spans.last().expect("a value method");
        assert_eq!((at(last.declared), at(last.selection)), (":draft", "draft"));
    }

    /// `singleton_class.define_method(name.pluralize)`, including the words Rails' inflector has
    /// a rule for and the ones it refuses.
    #[test]
    fn the_class_method_is_the_attribute_pluralized() {
        for (attribute, plural) in [
            ("status", "self.statuses"),
            ("category", "self.categories"),
            ("series", "self.series"),
        ] {
            let declared = names(&model(&format!("  enum :{attribute}, {{ draft: 0 }}\n")));
            assert!(declared.contains(&plural.to_owned()), "{declared:?}");
        }
    }

    /// An `enum` written where a macro is not a statement of a class body declares nothing —
    /// the bounding rule the rest of this directory follows, applied to the new reader.
    #[test]
    fn only_a_statement_of_a_class_body_is_a_macro() {
        for body in [
            "  included do\n    enum :status, { draft: 0 }\n  end\n",
            "  def setup\n    enum :status, { draft: 0 }\n  end\n",
            "  if Rails.env.test?\n    enum :status, { draft: 0 }\n  end\n",
            "  self.enum :status, { draft: 0 }\n",
        ] {
            assert_eq!(names(&model(body)), Vec::<String>::new(), "{body}");
        }
    }
}
