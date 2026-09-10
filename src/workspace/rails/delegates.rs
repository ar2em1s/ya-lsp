//! `delegate`, and the two hops its type needs.
//!
//! The one generator in this crate that **derives**. Every other reads a type out of the file in
//! front of it; this one reads a *name* and has to ask somewhere else entirely what it returns.
//! `delegate :name, to: :user` on `Story` is the return type of `User#name`, which the schema
//! generator writes into a different file's generated document in this same pass — so there is
//! nothing to resolve against and nothing indexed. [`Facts::returns`] is the question the fact
//! table's second phase exists for, and [`Delegate::declare`] is its first caller.
//!
//! # What is declined here is the *type*, and never the member
//!
//! Every other reader in this directory declines a whole declaration when it cannot name a class,
//! because the member's name is *derived* from something and a bad derivation invents a member:
//! `belongs_to :parent_comment` with no `class_name:` would declare a `ParentComment` no
//! application has. A `delegate` derives nothing — the name is a symbol literal in the call, and
//! `Module#delegate` defines that method whatever `to:` turns out to hold at run time. So an ivar
//! target, a `to:` that is not a literal, and a first hop that answers nothing all declare the
//! member with `untyped`.
//!
//! That is safe rather than merely cheap, for one reason in `types.rs`: `untyped` is **dropped**
//! from the return-type table, so an `untyped` member adds no entry and a chain through it falls
//! to exactly the rung it would have reached anyway. What it does add is a definition, mapped to
//! the `:name` symbol inside the call — so **the navigational half is free**.
//!
//! # What is declined outright, and it is one thing
//!
//! A `prefix:` this cannot read, for [`super::enums`]' reason rather than a new one: the names
//! would come out *wrong* rather than missing, and a wrong `def` is the failure every generator
//! here is built to avoid. `prefix: true` on anything but a method name is the same case wearing
//! Rails' own guard — `Module#delegate` raises `ArgumentError` on it — so a call that cannot run
//! declares nothing.

use ruby_prism::{CallNode, Node};

use super::syntax::{constant_spelling, header, inherited, symbol_or_string};
use crate::generated::{Declared, Facts, Owner, Source};

/// What `to:` names, and therefore what the first hop asks.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// `to: :user`, or the string spelling of it. 305 of the corpus' 351 calls, and the only
    /// shape whose first hop is a question this pass can answer. 295 of the 305 name a method
    /// that could answer it; the other 10 are `to: :class`, which lands here on purpose — see
    /// [`classify`].
    Method(String),
    /// `to: Settings` — a constant, so what is delegated to is that class's **singleton** and
    /// not an instance of it. 19 of the 351.
    Constant(String),
    /// `to: :@config`, and a `to:` that is not a literal at all. There is a receiver at run time
    /// and no name for its class here. 27 of the 351, and every one of them is an ivar: not one
    /// call in six applications writes a `to:` this cannot read.
    ///
    /// An ivar's type is `types.rs`' rung 3 — the assignments in its own class — and reaching
    /// for it would need the graph, which this directory may not have and this pass could not
    /// use anyway: `synthesize` runs before `resolve`.
    ///
    /// Reading the assignments out of the *file* instead is possible and was measured before it
    /// was declined. Of the 27, **25** assign the ivar somewhere in the file and only **9**
    /// assign it `Const.new` — the one shape a name can be taken from. **12** assign it from a
    /// local variable, which is `def initialize(config) = @config = config` and names no class
    /// anywhere in the file. So a syntactic ivar rung is worth 9 calls across six applications
    /// and cannot reach half of what it aims at; the members are declared either way.
    Opaque,
}

/// One `delegate` call, read.
#[derive(Debug)]
pub(super) struct Delegate {
    target: Target,
    /// One member per name the call lists, with the symbol's own span. `delegate :name, :email`
    /// is two members and two jumps, and 146 of the corpus' 351 calls list more than one.
    names: Vec<(String, (u32, u32))>,
    /// What Rails puts in front of every name, the `_` included. Empty for no `prefix:`.
    prefix: String,
    /// `allow_nil: true` — the delegation answers `nil` where it would otherwise raise, so every
    /// type it declares admits one. 24 of the 351.
    allow_nil: bool,
    /// The `delegate ...` header, for the jump's outer range.
    at: (u32, u32),
    /// How `to:` was written, sliced, for the provenance line.
    to: String,
}

/// Read one `delegate` call, or decline the whole of it.
///
/// `hosts` is the `with_options` blocks around it, exactly as [`super::models`] passes them: a
/// `delegate` inside one really does inherit its keywords, because `with_options` is a plain
/// method returning an `ActiveSupport::OptionMerger` and every call on it is merged. **No corpus
/// writes one** — 0 of the 351 — so this is uniformity rather than a measured need, and the
/// alternative was two different option-lookup rules in one directory.
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
    // Every leading literal is a name; the keyword hash and a `delegate(*NAMES, to: ...)` splat
    // both answer `None` here and are skipped rather than making the call unreadable. A call
    // that is nothing but a splat then has no name and declares nothing, which is the honest
    // answer for a list this cannot see: solidus and forem write the corpus' two.
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
/// `to: :class` is deliberately not a fourth. It means `self.class` in Rails and reads here as a
/// method named `class`, whose first hop finds nothing and whose members are therefore `untyped`
/// — which is the answer a case for it would have reached anyway, because all 10 of the corpus'
/// uses delegate to a `def self.` that is real Ruby in the file rather than a fact this pass
/// wrote.
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
/// `prefix: true` uses the `to:` spelling, and `Module#delegate`'s own first line is
/// `raise ArgumentError if prefix == true && /^[^a-z_]/.match?(to)` — so `prefix: true` on an
/// ivar or a constant is a call that raises at load time, and a reader that named the methods
/// anyway would be declaring members of a class that never finished being defined.
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
            // One character stricter than Rails' guard, for a reason Rails does not have:
            // `to: :user=` passes `/^[^a-z_]/` and makes `user=_name`, which `define_method`
            // takes happily and RBS cannot parse — and one unparsable `def` costs the whole
            // generated document rather than the one member.
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

/// Whether a name is an identifier carrying no punctuation at all.
///
/// What a *prefix* has to be, because a prefix is concatenated in front of another name and the
/// two together have to be one `def` RBS will take.
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
/// Deliberately **not** [`super::enums`]' test, which is stricter and right to be: an `enum`
/// label is *suffixed* into `draft?` and `draft!`, so the label itself must carry no
/// punctuation, while a `delegate` name is written as the method it already is. Names ending in
/// `?`, `=` and `!` are a large minority of real `delegate` names and dropping them would be
/// dropping a fifth of the feature. Nothing wider is allowed and nothing wider is needed: real
/// `delegate` names are never operators, and one strange name is a document
/// `Synthesized::record` refuses whole.
fn is_method_name(name: &str) -> bool {
    plain(name.strip_suffix(['?', '!', '=']).unwrap_or(name))
}

/// The class an RBS return type names, when it names exactly one.
///
/// `User?` is a `User`: the `?` says the *first* hop can answer `nil`, and whether the
/// delegation can is `allow_nil:`'s to say and not this. Everything else in the type language
/// names no single class whose members could be asked for — `Array[Comment]`, `(A | B)`, `bool`,
/// `untyped`, `String?` where the string is a column. `Comment::Relation` is a class and is
/// deliberately one of the ones that passes: `delegate :first, to: :comments` really does reach
/// `Comment::Relation#first`, which is a member this pass wrote two items ago.
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
    /// `project` is every fact the pass has stated so far, merged, and it is built only because
    /// this reader asks for it. A `delegate` whose target is another
    /// `delegate` therefore answers `untyped`, and that is a property of the phase boundary
    /// rather than a rule anybody wrote: the second one's facts do not exist yet when the first
    /// asks. Making them exist would mean iterating to a fixed point over a graph a user can
    /// write a cycle into.
    pub(super) fn declare(&self, facts: &mut Facts, file: &str, owner: &Owner, project: &Facts) {
        let through = self.through(owner, project);
        for (name, name_at) in &self.names {
            let declared = format!("{}{name}", self.prefix);
            // The precedence table, spent by the loser declining. A `delegate :title` and the
            // `t.string "title"` it shadows land in *two* generated documents — the model's and
            // the schema's — where [`Facts`]' own precedence cannot see the pair, and two
            // `def title:` lines in two documents are an overload set whose type is whichever
            // was harvested last. An `enum` solves the same shape by telling the schema which
            // columns an `enum` re-types; here the loser is this one, because `delegate` is the
            // most derived thing in the table and the union is what it already holds.
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
                // What Rails writes is `def #{name}(...)`, so every argument is forwarded and
                // whatever arity the target takes is accepted — which `(*untyped)` is, and item
                // 8's partition is why it has to be: a `def` claiming the wrong arity answers
                // for a call nobody made. A setter is the one shape that takes exactly one
                // value however the target is written, so it says so. **RBS accepts either** —
                // `def name=: (*untyped) -> String` parses — so this is the signature being
                // true rather than the parser being strict.
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

    /// The owner the second hop asks — the first hop, resolved.
    ///
    /// A constant needs no hop at all: `delegate :foo, to: Settings` is `Settings.foo`, so the
    /// owner is that class's singleton and whether the application has ever heard of `Settings`
    /// is answered by the lookup finding nothing. That is why no `known` set is passed in: every
    /// owner in `project` is either a class `Context` already vouched for or one this pass
    /// invented, so a second gate against `Context::classes` could never refuse anything this
    /// one accepts.
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
        // A first hop whose answer is a type and not a class, which `bool` is: RBS spells it
        // `true | false`, and a union names nothing whose members could be asked for.
        member(
            Owner::Instance("Story".to_owned()),
            "flagged",
            "bool",
            Source::Column,
        );
        // A class name with an underscore in it, which Ruby allows and only an explicit
        // `class_name: "Legacy_Story"` can produce — the inflector never writes one.
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
        // Pinned whole for the reason the schema's and the model's are: every rule in this
        // reader shows up in the text, and asserting them one predicate at a time is how a
        // change to the shape passes ten green tests.
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

    /// A first hop whose answer is not one class's name, and one whose answer is a class this
    /// pass has nothing to say about. Both end at `untyped` and only the first tests
    /// [`class_named`].
    #[test]
    fn a_first_hop_this_cannot_ask_about_types_nothing() {
        // `tags` is an `Array[Tag]`: a real answer, and not an owner whose members exist here.
        // `title` is a `String`, which *is* a class — and no generator declares on `String`, so
        // the second hop is what fails.
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
    /// Three shapes and one answer. A `prefix:` that is not a literal at all; a `prefix:` that
    /// is a literal carrying punctuation; and — the one Rails' own guard lets through —
    /// `prefix: true` on a `to:` that ends in `=`, which makes `user=_username`, a name
    /// `define_method` takes and RBS will not parse.
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

    /// `prefix: false` and `prefix: nil` are the same as writing none, which is Rails' own
    /// `if prefix` and not a special case.
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

    /// `private: true` changes the visibility and not the table. rubydex reads the `def`s in the
    /// file for visibility, and there is no `def` here to read — so declaring it is the same
    /// answer with or without.
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
            // a splat this cannot see, which is solidus' spelling
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

    /// `to:` written as a string, which is one call in the six corpora.
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

    /// `with_options` merges its keywords into the calls inside it, and `delegate` is a call
    /// inside it like any other. **0 of the corpus' 351 is written this way** — this is here
    /// because the alternative was two option-lookup rules in one directory.
    #[test]
    fn a_delegate_inside_with_options_inherits_its_keywords() {
        assert!(
            rbs("class Story\n  with_options to: :user do\n    delegate :username\n  end\nend\n")
                .contains("def username: (*untyped) -> String")
        );
    }

    /// The two shapes of first-hop answer [`class_named`] has to tell apart, at the two ends.
    ///
    /// `bool` is an answer and is not a class — RBS spells it `true | false` — so nothing can be
    /// asked of it. `Legacy_Story` is a class whose name Ruby allows and the inflector would
    /// never write, so only an explicit `class_name:` produces one; it has to pass.
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

    /// A better generator's word in another document, and the rank spent by declining.
    ///
    /// `Story#title` is a column in `project()`, which is written into `db/schema.rb`'s
    /// generated document — where this file's [`Facts`] precedence can never see it. Two
    /// `def title:` lines in two documents are an overload set whose type is whichever was
    /// harvested last, so the loser has to say nothing at all.
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

    /// The gate on phase two's cost, which is the one thing about it a test can see directly.
    ///
    /// A workspace with no `delegate` must not pay a merge of every fact in the project, and the
    /// union is built only where this answers yes.
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

    /// The phase boundary, stated as a test rather than left to be discovered.
    ///
    /// `delegate :x, to: :user` then `delegate :y, to: :x` — the second is asking about a member
    /// the first declared, which does not exist when phase two starts. Both are declared; the
    /// second is `untyped`. Making it otherwise means iterating to a fixed point over a graph a
    /// user can write a cycle into.
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
}
