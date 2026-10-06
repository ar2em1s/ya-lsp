//! `ActiveSupport::CurrentAttributes`: each `attribute :x` makes a reader and a writer
//! on the class's one per-thread instance, and the same pair on the class object, which hands them
//! to that instance. rubydex sees none of them (`generated_attribute_methods` and `delegate`), so
//! they are declared here.
//!
//! The readers return [`HELD`]: whatever the writer is given on that class object or its instance,
//! which the types table reads off the application's calls, `Current.set(x: v)` included. `nil`
//! joins, since each request starts empty. A literal `default:` joins its type instead of `nil`
//! (Rails 7.2); any other default refuses, as only running Ruby says what it builds.

use ruby_prism::{CallNode, Node};

use super::syntax::{header, keyword, symbol_or_string};
use crate::generated::{Declared, Facts, HELD, Owner, Source};

/// The class every current-attributes class inherits from.
pub const BASE: &str = "ActiveSupport::CurrentAttributes";

/// One `attribute` call in a class body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentAttribute {
    /// The call's name and arguments.
    pub at: (u32, u32),
    /// Each name it declares, and its span inside its symbol or string.
    pub names: Vec<(String, (u32, u32))>,
    /// What `default:` says.
    pub default: Default,
}

/// What an `attribute` call's `default:` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Default {
    /// None written.
    None,
    /// A literal of this RBS type.
    Literal(&'static str),
    /// Anything else: a proc, a constant, a call. Only running Ruby says what it is.
    Dynamic,
}

/// The call `attribute :a, :b, default: …` as a current-attributes class writes it.
#[must_use]
pub fn read(source: &str, call: &CallNode<'_>) -> Option<CurrentAttribute> {
    let names: Vec<(String, (u32, u32))> = call
        .arguments()?
        .arguments()
        .iter()
        .filter(|argument| argument.as_keyword_hash_node().is_none())
        .map(|argument| symbol_or_string(source, &argument))
        .collect::<Option<_>>()?;
    let default = match keyword(call, "default") {
        None => Default::None,
        Some(value) => literal(&value).map_or(Default::Dynamic, Default::Literal),
    };
    (!names.is_empty()).then_some(CurrentAttribute {
        at: header(call)?,
        names,
        default,
    })
}

/// The RBS type of a literal default, or `None` for anything else.
fn literal(node: &Node<'_>) -> Option<&'static str> {
    if node.as_string_node().is_some() {
        Some("String")
    } else if node.as_symbol_node().is_some() {
        Some("Symbol")
    } else if node.as_integer_node().is_some() {
        Some("Integer")
    } else if node.as_float_node().is_some() {
        Some("Float")
    } else if node.as_true_node().is_some() || node.as_false_node().is_some() {
        Some("bool")
    } else if node.as_array_node().is_some() {
        Some("Array[untyped]")
    } else if node.as_hash_node().is_some() {
        Some("Hash[untyped, untyped]")
    } else {
        None
    }
}

/// The readers and writers each of `attributes` makes on `class` and its class object, placed at
/// the name. A name the class body `def`s itself is left to it (`defined`: `(a def self., name)`).
pub fn declare(
    facts: &mut Facts,
    file: &str,
    class: &str,
    attributes: &[CurrentAttribute],
    defined: &std::collections::BTreeSet<(bool, String)>,
) {
    for attribute in attributes {
        let returns = match attribute.default {
            Default::None => HELD.to_owned(),
            Default::Literal(class) => format!("{class} | {HELD}"),
            Default::Dynamic => "untyped".to_owned(),
        };
        for (name, at) in &attribute.names {
            for singleton in [false, true] {
                let owner = if singleton {
                    Owner::Singleton(class.to_owned())
                } else {
                    Owner::Instance(class.to_owned())
                };
                for (spelled, parameters, returns) in [
                    (name.clone(), "()", returns.clone()),
                    (format!("{name}="), "(untyped value)", "untyped".to_owned()),
                ] {
                    if defined.contains(&(singleton, spelled.clone())) {
                        continue;
                    }
                    facts.declare(Declared {
                        owner: owner.clone(),
                        name: spelled,
                        returns,
                        parameters: parameters.to_owned(),
                        because: format!("From `{file}`, `attribute :{name}` on `{BASE}`."),
                        at: Some((attribute.at, *at)),
                        from: Source::Derived,
                        overloads: Vec::new(),
                        private: false,
                    });
                }
            }
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::declaring;

    fn attributes(source: &str) -> Vec<CurrentAttribute> {
        let parsed = ruby_prism::parse(source.as_bytes());
        parsed
            .node()
            .as_program_node()
            .expect("a program")
            .statements()
            .body()
            .iter()
            .filter_map(|statement| read(source, &statement.as_call_node()?))
            .collect()
    }

    #[test]
    fn an_attribute_declares_a_reader_and_a_writer_on_the_class_and_its_instance() {
        let source = "\
attribute :account, \"user\"
attribute :locale, default: \"en\"
attribute :count, default: 0
attribute :ratio, default: 0.5
attribute :flag, default: false
attribute :on, default: true
attribute :kind, default: :plain
attribute :list, default: []
attribute :map, default: {}
attribute :made, default: -> { Time.now }
attribute :written
attribute NAMES
attribute
";
        let found = attributes(source);
        assert_eq!(
            found.len(),
            11,
            "a name only Ruby knows, or none, declares nothing"
        );
        assert_eq!(
            found[0]
                .names
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["account", "user"]
        );
        let mut facts = Facts::default();
        let defined = std::collections::BTreeSet::from([(false, "written=".to_owned())]);
        declare(
            &mut facts,
            "app/models/current.rb",
            "Current",
            &found,
            &defined,
        );
        let rbs = facts.render(&declaring(&[])).rbs;
        for line in [
            "def account: () -> WrittenOnItsObjectOrClass",
            "def self.account: () -> WrittenOnItsObjectOrClass",
            "def account=: (untyped value) -> untyped",
            "def self.user=: (untyped value) -> untyped",
            "def locale: () -> (String | WrittenOnItsObjectOrClass)",
            "def count: () -> (Integer | WrittenOnItsObjectOrClass)",
            "def ratio: () -> (Float | WrittenOnItsObjectOrClass)",
            "def flag: () -> (bool | WrittenOnItsObjectOrClass)",
            "def on: () -> (bool | WrittenOnItsObjectOrClass)",
            "def kind: () -> (Symbol | WrittenOnItsObjectOrClass)",
            "def list: () -> (Array[untyped] | WrittenOnItsObjectOrClass)",
            "def map: () -> (Hash[untyped, untyped] | WrittenOnItsObjectOrClass)",
            "def made: () -> untyped",
            "def self.written=: (untyped value) -> untyped",
        ] {
            assert!(rbs.contains(line), "{line}: {rbs}");
        }
        assert!(
            !rbs.contains("def written=:"),
            "the body's own `def` keeps it: {rbs}"
        );
    }
}
