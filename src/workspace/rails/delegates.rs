//! `delegate`, and the two hops its type needs.
//!
//! The one generator in this crate that **derives**. Every other one reads a type out of the file
//! in front of it; this one reads a *name* and asks somewhere else what it returns.
//!
//! `delegate :name, to: :user` on `Story` returns what `User#name` returns. The schema generator
//! writes that into a different file's generated document in this same pass, so there is nothing
//! resolved and nothing indexed to ask. [`Facts::returns`] is the question the fact table's second
//! phase exists for, and [`Delegate::declare`] is its first caller.
//!
//! # What is declined is the *type*, never the member
//!
//! Every other reader here declines a whole declaration when it cannot name a class, because its
//! member's name is *derived*, and a bad derivation invents a member (`belongs_to :parent_comment`
//! with no `class_name:` would declare a `ParentComment` nobody has).
//!
//! A `delegate` derives nothing. The name is a symbol literal in the call, and `Module#delegate`
//! defines that method whatever `to:` holds at run time. So these declare the member as `untyped`:
//! - an ivar target;
//! - a `to:` that is not a literal;
//! - a first hop that answers nothing.
//!
//! That is safe, not just cheap: `types.rs` **drops** `untyped` from the return-type table. An
//! `untyped` member adds no entry, and a chain through it falls to the rung it would have reached
//! anyway. It does add a definition, mapped to the `:name` symbol inside the call, so **the
//! navigational half is free**.
//!
//! # What is declined outright: one thing
//!
//! A `prefix:` this cannot read, for [`super::enums`]' reason: the names would come out *wrong*,
//! not missing, and a wrong `def` is the failure every generator here avoids.
//!
//! `prefix: true` on anything but a method name is the same case, and Rails guards it too:
//! `Module#delegate` raises `ArgumentError`. A call that cannot run declares nothing.

use ruby_prism::{CallNode, Node};

use super::syntax::{constant_spelling, header, inherited, symbol_or_string};
use crate::generated::{Declared, Facts, Owner, Source};

/// What `to:` names, and therefore what the first hop asks.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// `to: :user`, or its string spelling. The only shape whose first hop is a question this pass
    /// can answer. `to: :class` lands here on purpose; see [`classify`].
    Method(String),
    /// `to: Settings`: a constant, so the receiver is that class's **singleton**, not an instance
    /// of it.
    Constant(String),
    /// `to: :@config`, or a `to:` that is not a literal at all. There is a receiver at run time and
    /// no name for its class here. In practice every one is an ivar.
    ///
    /// An ivar's type is `types.rs`' assignment rung, and reaching it would need the graph. This
    /// directory may not have one, and this pass could not use it anyway: `synthesize` runs before
    /// `resolve`.
    ///
    /// Reading the ivar's assignments out of the *file* was considered and declined. Only
    /// `@config = Const.new` names a class; `def initialize(config) = @config = config` names none
    /// anywhere in the file. The members are declared either way.
    Opaque,
}

/// One `delegate` call, read.
#[derive(Debug)]
pub(super) struct Delegate {
    target: Target,
    /// One member per name the call lists, with the symbol's own span. `delegate :name, :email` is
    /// two members and two jumps.
    names: Vec<(String, (u32, u32))>,
    /// What Rails puts in front of every name, the `_` included. Empty for no `prefix:`.
    prefix: String,
    /// `allow_nil: true`: the delegation answers `nil` where it would otherwise raise, so every
    /// type it declares admits `nil`.
    allow_nil: bool,
    /// The `delegate ...` header, for the jump's outer range.
    at: (u32, u32),
    /// How `to:` was written, sliced, for the provenance line.
    to: String,
}

/// Read one `delegate` call, or decline the whole of it.
///
/// `hosts` is the `with_options` blocks around it, passed exactly as [`super::models`] passes them.
/// A `delegate` inside one really does inherit its keywords: `with_options` returns an
/// `ActiveSupport::OptionMerger`, which merges into every call on it. Few applications write this;
/// it is here so the directory has one option-lookup rule, not two.
pub(super) fn read<'pr>(
    source: &str,
    node: &CallNode<'pr>,
    hosts: &[CallNode<'pr>],
) -> Option<Delegate> {
    let at = header(node)?;
    let written = inherited(node, hosts, "to")?;
    let location = written.location();
    let to = source
        .get(location.start_offset()..location.end_offset())?
        .to_owned();
    let target = classify(source, &written);
    // Every leading literal is a name. The keyword hash and a `delegate(*NAMES, to: ...)` splat
    // both answer `None` here and are skipped, not treated as unreadable. A call that is only a
    // splat then has no name and declares nothing: the honest answer for a list this cannot see.
    let names: Vec<(String, (u32, u32))> = node
        .arguments()?
        .arguments()
        .iter()
        .filter_map(|argument| symbol_or_string(source, &argument))
        .filter(|(name, _)| is_method_name(name))
        .collect();
    if names.is_empty() {
        return None;
    }
    Some(Delegate {
        prefix: prefix(source, node, hosts, &target)?,
        allow_nil: inherited(node, hosts, "allow_nil")
            .is_some_and(|value| value.as_true_node().is_some()),
        target,
        names,
        at,
        to,
    })
}

/// Which of the three shapes a `to:` value is.
///
/// `to: :class` is deliberately not a fourth. Rails reads it as `self.class`; here it reads as a
/// method named `class`, whose first hop finds nothing, so its members are `untyped`. A dedicated
/// case would reach the same answer: real uses delegate to a `def self.` in the file, not to a fact
/// this pass wrote.
fn classify(source: &str, node: &Node<'_>) -> Target {
    if node.as_constant_read_node().is_some() || node.as_constant_path_node().is_some() {
        return Target::Constant(constant_spelling(source, node));
    }
    match symbol_or_string(source, node) {
        Some((name, _)) if is_method_name(&name) => Target::Method(name),
        _ => Target::Opaque,
    }
}

/// What Rails puts in front of every delegated name, or `None` to decline the call.
///
/// `prefix: true` uses the `to:` spelling. `Module#delegate`'s first line is
/// `raise ArgumentError if prefix == true && /^[^a-z_]/.match?(to)`. So `prefix: true` on an ivar
/// or a constant raises at load time, and naming the methods anyway would declare members of a
/// class that never finished being defined.
fn prefix(
    source: &str,
    node: &CallNode<'_>,
    hosts: &[CallNode<'_>],
    target: &Target,
) -> Option<String> {
    let Some(value) = inherited(node, hosts, "prefix") else {
        return Some(String::new());
    };
    if value.as_true_node().is_some() {
        return match target {
            // One character stricter than Rails' guard, for a reason Rails does not have.
            // `to: :user=` passes `/^[^a-z_]/` and makes `user=_name`: `define_method` takes it,
            // RBS cannot parse it, and one unparsable `def` costs the whole generated document.
            Target::Method(name) if plain(name) => Some(format!("{name}_")),
            _ => None,
        };
    }
    if value.as_false_node().is_some() || value.as_nil_node().is_some() {
        return Some(String::new());
    }
    let written = symbol_or_string(source, &value)?.0;
    plain(&written).then(|| format!("{written}_"))
}

/// Whether a name is an identifier with no punctuation at all.
///
/// What a *prefix* must be: it is concatenated in front of another name, and the two together must
/// be one `def` RBS accepts.
fn plain(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes
        .first()
        .is_some_and(|first| first.is_ascii_lowercase() || *first == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

/// Whether a delegated name can be written as a `def` in RBS.
///
/// Deliberately **not** [`super::enums`]' test, which is stricter and right to be:
/// - an `enum` label is *suffixed* into `draft?` and `draft!`, so the label itself must carry no
///   punctuation;
/// - a `delegate` name is written as the method it already is, and names ending in `?`, `=` and `!`
///   are common.
///
/// Nothing wider is allowed or needed: real `delegate` names are never operators, and one strange
/// name is a document `Synthesized::record` refuses whole.
fn is_method_name(name: &str) -> bool {
    plain(name.strip_suffix(['?', '!', '=']).unwrap_or(name))
}

/// The class an RBS return type names, when it names exactly one.
///
/// `User?` is a `User`: the `?` says the *first* hop can answer `nil`, and whether the delegation
/// can is for `allow_nil:` to say. Everything else in the type language names no single class to
/// ask for members: `Array[Comment]`, `(A | B)`, `bool`, `untyped`, a `String?` column.
///
/// `Comment::Relation` is a class and deliberately passes: `delegate :first, to: :comments` really
/// does reach `Comment::Relation#first`, which this pass writes.
fn class_named(returns: &str) -> Option<String> {
    let name = returns.strip_suffix('?').unwrap_or(returns);
    let bytes = name.as_bytes();
    (bytes.first().is_some_and(u8::is_ascii_uppercase)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b':'))
    .then(|| name.to_owned())
}

impl Delegate {
    /// Say every name this call delegates, typed where both hops answer.
    ///
    /// `project` is every fact the pass has stated so far, merged. It is built only because this
    /// reader asks for it.
    ///
    /// A `delegate` whose target is another `delegate` therefore answers `untyped`. That is the
    /// phase boundary, not a rule anybody wrote: the second one's facts do not exist yet when the
    /// first asks. Making them exist would mean iterating to a fixed point over a graph a user can
    /// write a cycle into.
    pub(super) fn declare(&self, facts: &mut Facts, file: &str, owner: &Owner, project: &Facts) {
        let through = self.through(owner, project);
        for (name, name_at) in &self.names {
            let declared = format!("{}{name}", self.prefix);
            // The precedence table, enforced by the loser declining. A `delegate :title` and the
            // `t.string "title"` it shadows land in *two* generated documents (the model's and the
            // schema's), where [`Facts`]' own precedence cannot see the pair. Two `def title:`
            // lines in two documents are an overload set typed by whichever was harvested last. An
            // `enum` solves the same shape by telling the schema which columns it re-types; here
            // the loser is this one, because `delegate` is the most derived thing in the table and
            // already holds the union.
            if project
                .source(owner, &declared)
                .is_some_and(|held| held.outranks(Source::Delegated))
            {
                continue;
            }
            let returns = through
                .as_ref()
                .and_then(|target| project.returns(target, name))
                .filter(|returns| *returns != "untyped")
                .map_or_else(|| "untyped".to_owned(), |returns| self.nullable(returns));
            facts.declare(Declared {
                owner: owner.clone(),
                name: declared,
                returns,
                // Rails writes `def #{name}(...)`, forwarding every argument, so any arity the
                // target takes is accepted. `(*untyped)` says exactly that, and the arity partition
                // is why it must: a `def` claiming the wrong arity answers for a call nobody made.
                // A setter always takes exactly one value, so it says so. **RBS accepts either**
                // (`def name=: (*untyped) -> String` parses), so this is the signature being true,
                // not the parser being strict.
                parameters: if name.ends_with('=') {
                    "(untyped)"
                } else {
                    "(*untyped)"
                }
                .to_owned(),
                because: format!("From `{file}`, `delegate :{name}, to: {}`.", self.to),
                at: Some((self.at, *name_at)),
                from: Source::Delegated,
                overloads: Vec::new(),
            });
        }
    }

    /// The owner the second hop asks: the first hop, resolved.
    ///
    /// A constant needs no hop: `delegate :foo, to: Settings` is `Settings.foo`, so the owner is
    /// that class's singleton. If the application never heard of `Settings`, the lookup simply
    /// finds nothing.
    ///
    /// That is why no `known` set is passed in. Every owner in `project` is a class `Context`
    /// already vouched for or one this pass invented, so a second gate against `Context::classes`
    /// could never refuse anything this one accepts.
    fn through(&self, owner: &Owner, project: &Facts) -> Option<Owner> {
        match &self.target {
            Target::Constant(name) => Some(Owner::Singleton(name.clone())),
            Target::Opaque => None,
            Target::Method(name) => {
                Some(Owner::Instance(class_named(project.returns(owner, name)?)?))
            }
        }
    }

    /// `allow_nil: true` on a type that does not already admit `nil`.
    fn nullable(&self, returns: &str) -> String {
        if self.allow_nil && !returns.ends_with('?') {
            format!("{returns}?")
        } else {
            returns.to_owned()
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::super::read_model;
    use super::*;
    use crate::analysis::testing::*;
    use crate::generated::declaring;

    /// The facts a schema and a `belongs_to` would have written before phase two runs.
    fn project() -> Facts {
        let mut facts = Facts::default();
        let mut member = |owner: Owner, name: &str, returns: &str, from: Source| {
            facts.declare(Declared {
                owner,
                name: name.to_owned(),
                returns: returns.to_owned(),
                parameters: "()".to_owned(),
                because: String::new(),
                at: None,
                from,
                overloads: Vec::new(),
            });
        };
        member(
            Owner::Instance("Story".to_owned()),
            "user",
            "User",
            Source::Association,
        );
        member(
            Owner::Instance("Story".to_owned()),
            "editor",
            "User?",
            Source::Association,
        );
        member(
            Owner::Instance("Story".to_owned()),
            "comments",
            "Comment::Relation",
            Source::Association,
        );
        member(
            Owner::Instance("Story".to_owned()),
            "tags",
            "Array[Tag]",
            Source::Association,
        );
        member(
            Owner::Instance("Story".to_owned()),
            "title",
            "String",
            Source::Column,
        );
        member(
            Owner::Instance("User".to_owned()),
            "username",
            "String",
            Source::Column,
        );
        member(
            Owner::Instance("User".to_owned()),
            "banned_at",
            "Time?",
            Source::Column,
        );
        member(
            Owner::Instance("User".to_owned()),
            "shadow",
            "untyped",
            Source::Column,
        );
        member(
            Owner::Instance("Comment::Relation".to_owned()),
            "first",
            "Comment?",
            Source::Interface,
        );
        // A first hop whose answer is a type and not a class: `bool` is `true | false` in RBS, and
        // a union names nothing to ask for members.
        member(
            Owner::Instance("Story".to_owned()),
            "flagged",
            "bool",
            Source::Column,
        );
        // A class name with an underscore, which Ruby allows and only an explicit
        // `class_name: "Legacy_Story"` can produce. The inflector never writes one.
        member(
            Owner::Instance("Story".to_owned()),
            "legacy",
            "Legacy_Story",
            Source::Association,
        );
        member(
            Owner::Instance("Legacy_Story".to_owned()),
            "headline",
            "String",
            Source::Column,
        );
        member(
            Owner::Singleton("Settings".to_owned()),
            "host",
            "String",
            Source::Annotated,
        );
        facts
    }

    fn rbs(source: &str) -> String {
        read_model(source)
            .derived("app/models/story.rb", &project())
            .render(&declaring(&[]))
            .rbs
    }

    #[test]
    fn the_rbs_a_delegate_declares() {
        // Pinned whole, like the schema's and the model's: every rule in this reader shows in the
        // text, and asserting one predicate at a time lets a change of shape pass ten green tests.
        assert_eq!(
            rbs("\
class Story < ApplicationRecord
  delegate :username, :banned_at, to: :user
  delegate :username, to: :user, prefix: true
  delegate :username, to: :user, prefix: :nilable, allow_nil: true
  delegate :banned_at, to: :user, prefix: :author, allow_nil: true
  delegate :first, to: :comments
  delegate :host, to: Settings
  delegate :missing, to: :user
  delegate :shadow, to: :user
  delegate :anything, to: :@config
  delegate :name=, to: :user
end
"),
            "\
class Story
  # From `app/models/story.rb`, `delegate :username, to: :user`.
  def username: (*untyped) -> String
  # From `app/models/story.rb`, `delegate :banned_at, to: :user`.
  def banned_at: (*untyped) -> Time?
  # From `app/models/story.rb`, `delegate :username, to: :user`.
  def user_username: (*untyped) -> String
  # From `app/models/story.rb`, `delegate :username, to: :user`.
  def nilable_username: (*untyped) -> String?
  # From `app/models/story.rb`, `delegate :banned_at, to: :user`.
  def author_banned_at: (*untyped) -> Time?
  # From `app/models/story.rb`, `delegate :first, to: :comments`.
  def first: (*untyped) -> Comment?
  # From `app/models/story.rb`, `delegate :host, to: Settings`.
  def host: (*untyped) -> String
  # From `app/models/story.rb`, `delegate :missing, to: :user`.
  def missing: (*untyped) -> untyped
  # From `app/models/story.rb`, `delegate :shadow, to: :user`.
  def shadow: (*untyped) -> untyped
  # From `app/models/story.rb`, `delegate :anything, to: :@config`.
  def anything: (*untyped) -> untyped
  # From `app/models/story.rb`, `delegate :name=, to: :user`.
  def name=: (untyped) -> untyped
end
"
        );
    }

    /// Two first hops that end at `untyped`: one whose answer is not one class's name, and one
    /// whose class this pass has nothing to say about. Only the first tests [`class_named`].
    #[test]
    fn a_first_hop_this_cannot_ask_about_types_nothing() {
        // `tags` is an `Array[Tag]`: a real answer, but not an owner with members here. `title` is
        // a `String`, which *is* a class, but no generator declares on `String`, so the second hop
        // fails.
        let rendered = rbs("\
class Story
  delegate :length, to: :tags
  delegate :upcase, to: :title
end
");
        assert!(
            rendered.contains("def length: (*untyped) -> untyped"),
            "{rendered}"
        );
        assert!(
            rendered.contains("def upcase: (*untyped) -> untyped"),
            "{rendered}"
        );
    }

    /// The `?` of an optional first hop is not the delegation's.
    #[test]
    fn an_optional_first_hop_still_reaches_its_class() {
        assert!(
            rbs("class Story\n  delegate :username, to: :editor\nend\n")
                .contains("def username: (*untyped) -> String")
        );
    }

    /// In a concern, and the one thing that changes: the owner is the module.
    #[test]
    fn a_delegate_in_a_concern_declares_on_the_module() {
        let mut facts = Facts::default();
        facts.declare(Declared {
            owner: Owner::Module("Storyish".to_owned()),
            name: "user".to_owned(),
            returns: "User".to_owned(),
            parameters: "()".to_owned(),
            because: String::new(),
            at: None,
            from: Source::Association,
            overloads: Vec::new(),
        });
        facts.declare(Declared {
            owner: Owner::Instance("User".to_owned()),
            name: "username".to_owned(),
            returns: "String".to_owned(),
            parameters: "()".to_owned(),
            because: String::new(),
            at: None,
            from: Source::Column,
            overloads: Vec::new(),
        });
        let rendered = read_model(
            "module Storyish\n  extend ActiveSupport::Concern\n\n  included do\n    belongs_to \
             :user\n    delegate :username, to: :user\n  end\nend\n",
        )
        .derived("app/models/concerns/storyish.rb", &facts)
        .render(&declaring(&[]))
        .rbs;
        assert_eq!(
            rendered,
            "\
module Storyish
  # From `app/models/concerns/storyish.rb`, `delegate :username, to: :user`.
  def username: (*untyped) -> String
end
"
        );
    }

    /// Rails raises on `prefix: true` with anything but a method name, so nothing is declared.
    #[test]
    fn prefix_true_on_a_target_that_is_not_a_method_declares_nothing() {
        assert_eq!(
            rbs("class Story\n  delegate :host, to: Settings, prefix: true\nend\n"),
            ""
        );
        assert_eq!(
            rbs("class Story\n  delegate :anything, to: :@config, prefix: true\nend\n"),
            ""
        );
    }

    /// A `prefix:` this cannot make one `def` out of would name the members wrong.
    ///
    /// Three shapes, one answer:
    /// 1. a `prefix:` that is not a literal;
    /// 2. a literal `prefix:` with punctuation;
    /// 3. `prefix: true` on a `to:` ending in `=`, which Rails' own guard lets through. It makes
    ///    `user=_username`: `define_method` takes it, RBS will not parse it.
    #[test]
    fn a_prefix_that_would_not_make_one_def_declares_nothing() {
        for written in ["prefix: PREFIX", "prefix: :\"odd?\"", "prefix: \"\""] {
            assert_eq!(
                rbs(&format!(
                    "class Story\n  delegate :username, to: :user, {written}\nend\n"
                )),
                "",
                "{written}"
            );
        }
        assert_eq!(
            rbs("class Story\n  delegate :username, to: :user=, prefix: true\nend\n"),
            ""
        );
    }

    /// `prefix: false` and `prefix: nil` are the same as writing none: Rails' own `if prefix`, not
    /// a special case.
    #[test]
    fn a_falsey_prefix_is_no_prefix() {
        for written in ["false", "nil"] {
            assert!(
                rbs(&format!(
                    "class Story\n  delegate :username, to: :user, prefix: {written}\nend\n"
                ))
                .contains("def username:"),
                "prefix: {written}"
            );
        }
    }

    /// `private: true` changes the visibility, not the table. rubydex reads visibility from the
    /// file's `def`s, and there is no `def` here to read, so the answer is the same either way.
    #[test]
    fn private_still_declares() {
        assert!(
            rbs("class Story\n  delegate :username, to: :user, private: true\nend\n")
                .contains("def username: (*untyped) -> String")
        );
    }

    /// A call Rails would raise on, and two it would define nothing from.
    #[test]
    fn the_calls_that_declare_nothing_at_all() {
        for source in [
            // no `to:`: `Module#delegate` raises `ArgumentError`
            "class Story\n  delegate :username\nend\n",
            // no name: nothing to define
            "class Story\n  delegate to: :user\nend\n",
            // a splat this cannot see
            "class Story\n  delegate(*NAMES, to: :user)\nend\n",
            // an operator, which RBS would take and `is_method_name` deliberately will not
            "class Story\n  delegate :<=>, to: :user\nend\n",
            // not a statement of the class body
            "class Story\n  def wrap\n    delegate :username, to: :user\n  end\nend\n",
        ] {
            assert_eq!(rbs(source), "", "{source}");
        }
    }

    /// A name beside one this cannot read is still declared; only the bad one is dropped.
    #[test]
    fn one_unreadable_name_does_not_take_the_others() {
        let rendered = rbs("class Story\n  delegate :<=>, :username, to: :user\nend\n");
        assert!(rendered.contains("def username:"), "{rendered}");
        assert_eq!(rendered.matches("def ").count(), 1, "{rendered}");
    }

    /// `to:` written as a string.
    #[test]
    fn a_string_target_reads_as_the_method_it_names() {
        assert!(
            rbs("class Story\n  delegate :username, to: \"user\"\nend\n")
                .contains("def username: (*untyped) -> String")
        );
    }

    /// A `to:` that is not a literal at all has a receiver and no name for it.
    #[test]
    fn an_unreadable_target_still_declares_the_member() {
        assert!(
            rbs("class Story\n  delegate :username, to: some_method_call\nend\n")
                .contains("def username: (*untyped) -> untyped")
        );
    }

    /// A qualified constant is one name, spelled as written.
    #[test]
    fn a_qualified_constant_target_is_asked_about_by_its_whole_name() {
        let mut facts = Facts::default();
        facts.declare(Declared {
            owner: Owner::Singleton("Spree::Config".to_owned()),
            name: "currency".to_owned(),
            returns: "String".to_owned(),
            parameters: "()".to_owned(),
            because: String::new(),
            at: None,
            from: Source::Annotated,
            overloads: Vec::new(),
        });
        assert!(
            read_model("class Story\n  delegate :currency, to: Spree::Config\nend\n")
                .derived("app/models/story.rb", &facts)
                .render(&declaring(&[]))
                .rbs
                .contains("def currency: (*untyped) -> String")
        );
    }

    /// `with_options` merges its keywords into the calls inside it, and `delegate` is a call like
    /// any other. Rare in practice; here so the directory has one option-lookup rule, not two.
    #[test]
    fn a_delegate_inside_with_options_inherits_its_keywords() {
        assert!(
            rbs("class Story\n  with_options to: :user do\n    delegate :username\n  end\nend\n")
                .contains("def username: (*untyped) -> String")
        );
    }

    /// The two first-hop answers [`class_named`] must tell apart, one at each end.
    /// - `bool` is an answer but not a class (`true | false` in RBS), so nothing can be asked of
    ///   it.
    /// - `Legacy_Story` is a class name Ruby allows and the inflector never writes; only an
    ///   explicit `class_name:` produces one. It must pass.
    #[test]
    fn a_type_that_is_not_a_class_and_a_class_name_the_inflector_would_not_write() {
        let rendered = rbs("\
class Story
  delegate :to_s, to: :flagged
  delegate :headline, to: :legacy
end
");
        assert!(
            rendered.contains("def to_s: (*untyped) -> untyped"),
            "{rendered}"
        );
        assert!(
            rendered.contains("def headline: (*untyped) -> String"),
            "{rendered}"
        );
    }

    /// A better generator's word in another document, and the rank enforced by declining.
    ///
    /// `Story#title` is a column in `project()`, written into `db/schema.rb`'s generated document,
    /// where this file's [`Facts`] precedence cannot see it. Two `def title:` lines in two
    /// documents are an overload set typed by whichever was harvested last, so the loser must say
    /// nothing.
    #[test]
    fn a_better_generator_in_another_document_keeps_the_member() {
        assert_eq!(
            rbs("class Story\n  delegate :title, :username, to: :user\nend\n"),
            "\
class Story
  # From `app/models/story.rb`, `delegate :username, to: :user`.
  def username: (*untyped) -> String
end
"
        );
    }

    /// The gate on phase two's cost, the one thing about it a test can see directly.
    ///
    /// A workspace with no `delegate` must not pay for merging every fact in the project; the union
    /// is built only where this answers yes.
    #[test]
    fn a_file_with_no_delegate_asks_for_no_second_phase() {
        assert!(!read_model("class Story\n  has_many :comments\nend\n").derives());
        assert!(!read_model("class Story\n  delegate :username\nend\n").derives());
        assert!(read_model("class Story\n  delegate :username, to: :user\nend\n").derives());
    }

    /// The spans, which are what the jump lands on: the whole call, and each name's own symbol.
    #[test]
    fn each_name_maps_to_its_own_symbol() {
        let source = "class Story\n  delegate :username, :banned_at, to: :user\nend\n";
        let declarations = read_model(source)
            .derived("app/models/story.rb", &project())
            .render(&declaring(&[]));
        let spans: Vec<(&str, &str)> = declarations
            .spans
            .iter()
            .map(|span| {
                (
                    &source[span.declared.0 as usize..span.declared.1 as usize],
                    &source[span.selection.0 as usize..span.selection.1 as usize],
                )
            })
            .collect();
        assert_eq!(
            spans,
            [
                ("delegate :username, :banned_at, to: :user", "username"),
                ("delegate :username, :banned_at, to: :user", "banned_at"),
            ]
        );
    }

    /// The phase boundary, stated as a test.
    ///
    /// `delegate :x, to: :user`, then `delegate :y, to: :x`: the second asks about a member the
    /// first declared, which does not exist when phase two starts. Both are declared; the second is
    /// `untyped`. Anything else means iterating to a fixed point over a graph a user can write a
    /// cycle into.
    #[test]
    fn a_delegate_through_a_delegate_is_untyped() {
        // `owner` is nothing until this call declares it, and this call's own facts are not in
        // `project`. So the second `delegate` asks about a member that does not exist yet.
        assert_eq!(
            rbs("\
class Story
  delegate :owner, to: :user
  delegate :username, to: :owner
end
"),
            "\
class Story
  # From `app/models/story.rb`, `delegate :owner, to: :user`.
  def owner: (*untyped) -> untyped
  # From `app/models/story.rb`, `delegate :username, to: :owner`.
  def username: (*untyped) -> untyped
end
"
        );
    }

    /// A schema, two models and a `delegate` between them: the two hops, end to end.
    ///
    /// `Story#user` is a `belongs_to` the model generator writes. `User#username` is a column the
    /// *schema* generator writes into a different file's generated document. Nothing has resolved
    /// when either is asked: the two-phase seam in a fixture.
    fn delegates_project(caller: &str) -> (Harness, DocUri, DocUri) {
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let story = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  \
             belongs_to :user\n  \
             delegate :username, :description, to: :user\n  \
             delegate :username, to: :user, prefix: true\n  \
             delegate :title, to: :user\n  \
             delegate :name=, to: :user\n  \
             delegate :anything, to: :@config\n\
             end\n",
        );
        harness.write(
            "app/models/user.rb",
            "class User < ApplicationRecord\nend\n",
        );
        harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[7.1].define(version: 2024_01_01_000000) do\n  \
             create_table \"stories\", force: :cascade do |t|\n    \
             t.string \"title\", null: false\n  \
             end\n\n  \
             create_table \"users\", force: :cascade do |t|\n    \
             t.string \"username\", null: false\n  \
             end\n\
             end\n",
        );
        let uri = harness.write("app/main.rb", caller);
        harness.index();
        harness.index_gems();
        (harness, story, uri)
    }

    #[test]
    fn a_delegate_types_through_two_hops_and_jumps_to_its_own_symbol() {
        // A `delegate` in one expression. Three things must hold at once, as for every generator in
        // this half:
        // 1. the member exists;
        // 2. the chain off it is typed, which needs *both* hops: an association in this file and a
        //    column in another;
        // 3. the jump lands on the `:username` symbol inside the call, not elsewhere in the class.
        let source = "Story.new.username.upcase\n";
        let (mut harness, story, uri) = delegates_project(source);

        assert!(
            harness.has("Story#username()"),
            "the delegated name is not a member"
        );

        let card = card(&mut harness, &uri, source, "upcase");
        assert!(card.contains("String#upcase"), "{card}");

        let definition = harness.definition_at(&uri, source, "username");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(story.as_str()),
            "{definition}"
        );
        // `  delegate :username, :description, to: :user` on line 2, revealed whole, with the one
        // name asked for selected past its colon.
        assert_eq!(
            (
                &definition[0]["targetRange"]["start"]["line"],
                &definition[0]["targetRange"]["start"]["character"],
                &definition[0]["targetSelectionRange"]["start"]["character"],
            ),
            (
                &serde_json::json!(2),
                &serde_json::json!(2),
                &serde_json::json!(12),
            ),
            "{definition}"
        );
    }

    #[test]
    fn a_hover_on_a_delegated_name_says_which_file_and_which_call_it_came_from() {
        // The provenance rule again, and load-bearing here: a delegated type is two derivations
        // deep, so a card that did not say so would present the *target's* schema as if this class
        // declared it.
        let source = "Story.new.username\n";
        let (mut harness, _story, uri) = delegates_project(source);

        let card = card(&mut harness, &uri, source, "username");
        assert!(card.contains("Story#username"), "{card}");
        assert!(card.contains("app/models/story.rb"), "{card}");
        assert!(card.contains("delegate :username"), "{card}");
        assert!(card.contains("to: :user"), "{card}");
    }

    #[test]
    fn what_a_delegate_declares_and_what_it_declines_to_type() {
        // The decline direction, where this reader differs from every other one here: it declines
        // the **type**, never the member. Rails defines all four methods whatever `to:` holds at
        // run time, so all four exist. The two that cannot be typed carry no return type, which
        // `Types::harvest` drops rather than believing.
        let source = "Story.new.username.upcase\nStory.new.description.length\n";
        let (mut harness, _story, uri) = delegates_project(source);

        assert!(harness.has("Story#username()"), "both hops answered");
        assert!(harness.has("Story#user_username()"), "prefix: true");
        assert!(
            harness.has("Story#description()"),
            "a name the target's schema does not hold is still a member"
        );
        assert!(
            harness.has("Story#anything()"),
            "an ivar target is still a member"
        );
        assert!(
            harness.has("Story#name=()"),
            "a setter is a name RBS takes, and one this document would be refused whole for"
        );

        // The chain tells the two apart, and is why an untyped declaration is safe. `untyped` is
        // dropped from the return table, so `.upcase` off the untyped one falls to the name rung
        // exactly as with no declaration at all, while the typed one resolves.
        let derived = card(&mut harness, &uri, source, "upcase");
        assert!(derived.contains("String#upcase"), "{derived}");
        assert!(
            !derived.contains("Matched on the method name alone"),
            "{derived}"
        );
        let guessed = card(&mut harness, &uri, source, "length");
        assert!(
            guessed.contains("Matched on the method name alone"),
            "an untyped delegation must add no entry to the return table: {guessed}"
        );
    }

    #[test]
    fn a_column_outranks_a_delegate_of_the_same_name() {
        // A column over a delegation, and why a rank must be enforceable from *outside* one
        // document. `Story` has a `title` column and a `delegate :title, to: :user`. The column is
        // what the database holds; the delegation is a claim about another class that does not even
        // hold the name. They land in two generated documents, where `Facts`' own precedence cannot
        // see the pair. Without the decline: two `def title:` lines, one place too many in the
        // card, and a type decided by whichever document `Types::harvest` read last.
        let source = "Story.new.title.upcase\n";
        let (mut harness, _story, uri) = delegates_project(source);

        let column = card(&mut harness, &uri, source, "title");
        assert!(
            column.contains("db/schema.rb"),
            "the delegate won: {column}"
        );
        assert!(!column.contains("delegate :title"), "{column}");
        assert!(!column.contains("Defined in"), "two declarations: {column}");
        // And the type is the column's, not the `untyped` the delegation would have carried into
        // the same key.
        let chained = card(&mut harness, &uri, source, "upcase");
        assert!(chained.contains("String#upcase"), "{chained}");
    }
}
