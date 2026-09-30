//! Ruby's `Module#include` and `Module#prepend` called on a class from outside its body:
//! `Paperclip::Attachment.prepend(Paperclip::AttachmentExtensions)`, and the plugin's
//! `Post.include(PostVoting::PostExtension)`.
//!
//! rubydex reads an `include` written in a body. A call made on the class from somewhere else is a
//! method call to it, so the module never joins the class's ancestors: the class's members are not
//! found from the module's code, its instance variables have no writes, and the module's methods
//! are not the class's. The call is Ruby's own, so the fact is written as an `include` or
//! `prepend` line on the class, which rubydex links however late it is indexed.
//!
//! Text in, facts out, as `workspace/rails/` is. The orchestration is `knowledge::mixins`'.

use ruby_prism::{
    BlockNode, CallNode, CaseMatchNode, CaseNode, ClassNode, DefNode, ForNode, IfNode, LambdaNode,
    ModuleNode, Node, UnlessNode, UntilNode, Visit, WhileNode, parse,
};

use crate::generated::{Facts, Owner};

/// The text that lists a file for [`read_mixins`]: a call of either method with a receiver. A
/// `.include?` is not one, and the reader declines whatever else matches.
pub const SPELLED: [&str; 4] = [".include(", ".include ", ".prepend(", ".prepend "];

/// One `Const.include(Mod)` or `Const.prepend(Mod)`, as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mixed {
    /// The class or module the call is made on, spelled as written (a leading `::` kept).
    pub target: String,
    /// The module it mixes in, spelled as written; `self` is the enclosing module's joined name,
    /// with a leading `::`.
    pub module: String,
    /// The body the call is written in, joined from the top level: where both names are looked up.
    pub nesting: String,
    /// `prepend` rather than `include`.
    pub prepend: bool,
}

/// One [`Mixed`] once both names are known: what [`facts`] writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The class or module mixed into, from the top level.
    pub target: String,
    /// Whether the target is a `module`, which is the keyword its body opens with.
    pub target_is_module: bool,
    /// The module mixed in, from the top level.
    pub module: String,
    /// `prepend` rather than `include`.
    pub prepend: bool,
}

/// Every such call in `source` that runs when the file loads.
///
/// - **Where it runs, not only where it is a statement.** A plugin writes these in an
///   `after_initialize do` block and an application in `config.to_prepare do`, both run at boot,
///   so a block's body is read. A `def`, a lambda, a loop and every conditional (`if`, `unless`,
///   `case`, a modifier) are Ruby that only runs sometimes, and are not.
/// - **Every argument a constant, or `self` straight in a module's body.** `self` in a class is a
///   class, which Ruby refuses to mix in; in a block it is whatever the block runs on. One argument
///   that is neither declines the whole call.
/// - **In Ruby's order.** `include(A, B)` puts `A` in front of `B`, as `include B` then `include A`
///   would, so the arguments are read last to first.
#[must_use]
pub fn read_mixins(source: &str) -> Vec<Mixed> {
    let result = parse(source.as_bytes());
    let mut reader = Reader {
        source,
        nesting: Vec::new(),
        in_module: false,
        blocks: 0,
        found: Vec::new(),
    };
    reader.visit(&result.node());
    reader.found
}

/// [`read_mixins`]' walk.
struct Reader<'s> {
    source: &'s str,
    nesting: Vec<String>,
    /// Whether the innermost body is a `module`'s, where `self` is a module that can be mixed in.
    in_module: bool,
    /// How many blocks deep the walk is inside that body: `self` there is not the body's.
    blocks: usize,
    found: Vec<Mixed>,
}

impl<'pr> Visit<'pr> for Reader<'_> {
    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        self.nested(&node.constant_path(), node.body(), true);
    }

    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        self.nested(&node.constant_path(), node.body(), false);
    }

    fn visit_block_node(&mut self, node: &BlockNode<'pr>) {
        self.blocks += 1;
        ruby_prism::visit_block_node(self, node);
        self.blocks -= 1;
    }

    /// A method's body runs when somebody calls it.
    fn visit_def_node(&mut self, _node: &DefNode<'pr>) {}

    /// A lambda's body runs when somebody calls it.
    fn visit_lambda_node(&mut self, _node: &LambdaNode<'pr>) {}

    /// Both arms of a conditional run only sometimes: `X.include(Y) if defined?(Z)`.
    fn visit_if_node(&mut self, _node: &IfNode<'pr>) {}

    fn visit_unless_node(&mut self, _node: &UnlessNode<'pr>) {}

    fn visit_case_node(&mut self, _node: &CaseNode<'pr>) {}

    fn visit_case_match_node(&mut self, _node: &CaseMatchNode<'pr>) {}

    /// A loop's body runs as many times as it says, which may be none.
    fn visit_while_node(&mut self, _node: &WhileNode<'pr>) {}

    fn visit_until_node(&mut self, _node: &UntilNode<'pr>) {}

    fn visit_for_node(&mut self, _node: &ForNode<'pr>) {}

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        if let Some(found) = self.mixed(node) {
            self.found.extend(found);
        }
        ruby_prism::visit_call_node(self, node);
    }
}

impl Reader<'_> {
    fn nested(&mut self, path: &Node<'_>, body: Option<Node<'_>>, module: bool) {
        let spelled = self.spelling(path);
        let pushed = spelled.trim_start_matches("::").to_owned();
        let outer = (
            std::mem::replace(&mut self.in_module, module),
            std::mem::take(&mut self.blocks),
        );
        // `class ::Foo` names the top level, whatever it is written in.
        let saved = spelled
            .starts_with("::")
            .then(|| std::mem::take(&mut self.nesting));
        self.nesting.push(pushed);
        if let Some(body) = body {
            self.visit(&body);
        }
        self.nesting.pop();
        if let Some(saved) = saved {
            self.nesting = saved;
        }
        (self.in_module, self.blocks) = outer;
    }

    /// The call as one [`Mixed`] per argument, last argument first, or `None` where it is not
    /// this shape.
    fn mixed(&self, call: &CallNode<'_>) -> Option<Vec<Mixed>> {
        let prepend = match call.name().as_slice() {
            b"include" => false,
            b"prepend" => true,
            _ => return None,
        };
        let receiver = call.receiver()?;
        if !is_constant(&receiver) {
            return None;
        }
        let target = self.spelling(&receiver);
        let nesting = self.nesting.join("::");
        let mut found = Vec::new();
        for argument in call.arguments()?.arguments().iter() {
            let module = if is_constant(&argument) {
                self.spelling(&argument)
            } else if argument.as_self_node().is_some() && self.in_module && self.blocks == 0 {
                format!("::{nesting}")
            } else {
                return None;
            };
            found.push(Mixed {
                target: target.clone(),
                module,
                nesting: nesting.clone(),
                prepend,
            });
        }
        found.reverse();
        Some(found)
    }

    fn spelling(&self, node: &Node<'_>) -> String {
        let location = node.location();
        self.source
            .get(location.start_offset()..location.end_offset())
            .unwrap_or_default()
            .to_owned()
    }
}

/// A constant as written: `Post`, `Paperclip::Attachment` or `::Post`.
fn is_constant(node: &Node<'_>) -> bool {
    node.as_constant_read_node().is_some() || node.as_constant_path_node().is_some()
}

/// The names a spelling may mean from where it is written, nearest first: Ruby's lexical lookup,
/// read off the joined nesting as `generated::candidates` reads it.
#[must_use]
pub fn meanings(nesting: &str, spelled: &str) -> Vec<String> {
    if let Some(top) = spelled.strip_prefix("::") {
        return vec![top.to_owned()];
    }
    if nesting.is_empty() {
        return vec![spelled.to_owned()];
    }
    crate::generated::candidates(nesting, spelled)
}

/// Every name [`resolve`] asks about for one call: each meaning of both names, and every
/// namespace above each, since a body is opened by its joined name.
#[must_use]
pub fn asked(found: &[Mixed]) -> Vec<String> {
    let mut asked = Vec::new();
    for mixed in found {
        for name in meanings(&mixed.nesting, &mixed.target)
            .into_iter()
            .chain(meanings(&mixed.nesting, &mixed.module))
        {
            asked.extend(
                name.match_indices("::")
                    .map(|(at, _)| name[..at].to_owned()),
            );
            asked.push(name);
        }
    }
    asked
}

/// Both names of one call, as Ruby would find them: the nearest meaning something declares.
///
/// `kinds` is which names are declared, each with whether it is a `module`. The call is declined
/// where either name means nothing, where the nearest meaning of the mixed-in name is a class
/// (Ruby raises), and where a namespace above the target is declared nowhere (its body could not
/// be opened by that name).
#[must_use]
pub fn resolve(mixed: &Mixed, kinds: &dyn Fn(&str) -> Option<bool>) -> Option<Resolved> {
    let target = meanings(&mixed.nesting, &mixed.target)
        .into_iter()
        .find(|name| kinds(name).is_some())?;
    let module = meanings(&mixed.nesting, &mixed.module)
        .into_iter()
        .find(|name| kinds(name).is_some())?;
    let above_all_declared = target
        .match_indices("::")
        .all(|(at, _)| kinds(&target[..at]).is_some());
    (kinds(&module)? && above_all_declared).then(|| Resolved {
        target_is_module: kinds(&target) == Some(true),
        target,
        module,
        prepend: mixed.prepend,
    })
}

/// The `include` and `prepend` lines, on the body of each target.
#[must_use]
pub fn facts(resolved: &[Resolved]) -> Facts {
    let mut facts = Facts::default();
    for found in resolved {
        let owner = if found.target_is_module {
            Owner::Module(found.target.clone())
        } else {
            Owner::Instance(found.target.clone())
        };
        let module = format!("::{}", found.module);
        if found.prepend {
            facts.prepend(owner, module);
        } else {
            facts.mixin(owner, module);
        }
    }
    facts
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::declaring;

    fn read(source: &str) -> Vec<String> {
        read_mixins(source)
            .into_iter()
            .map(|found| {
                format!(
                    "{} {}.{}({})",
                    if found.nesting.is_empty() {
                        "-"
                    } else {
                        &found.nesting
                    },
                    found.target,
                    if found.prepend { "prepend" } else { "include" },
                    found.module
                )
            })
            .collect()
    }

    #[test]
    fn a_call_on_a_class_that_runs_when_the_file_loads_is_read() {
        let source = "\
Paperclip::Attachment.prepend(Paperclip::AttachmentExtensions)
::Post.include Tagging, ::Voting

module ShopPromotions
  module ShipmentPatch
    Spree::Shipment.prepend self
    Spree::Shipment.prepend(ShipmentPatch)
  end
end

after_initialize do
  Topic.include(PostVoting::TopicExtension)
end

module Outer
  class ::Top
    Top.include Mixin
  end
end
";
        assert_eq!(
            read(source),
            [
                "- Paperclip::Attachment.prepend(Paperclip::AttachmentExtensions)",
                "- ::Post.include(::Voting)",
                "- ::Post.include(Tagging)",
                "ShopPromotions::ShipmentPatch Spree::Shipment.prepend(::ShopPromotions::ShipmentPatch)",
                "ShopPromotions::ShipmentPatch Spree::Shipment.prepend(ShipmentPatch)",
                "- Topic.include(PostVoting::TopicExtension)",
                "Top Top.include(Mixin)",
            ]
        );
    }

    #[test]
    fn a_call_that_only_sometimes_runs_or_names_no_constant_is_not() {
        let source = "\
module Empty
end

class Shelf
  Shelf.include self
  def self.load
    Post.include Loaded
  end
end

module Setup
  configure do
    Post.include self
  end
end

Post.include Tagging if defined?(Tagging)
unless ENV['X'] then Post.include Other end
case mode
when :a then Post.include Cased
end
case mode
in :a then Post.include Matched
end
while ready do Post.include Looped end
until ready do Post.include Looped end
for x in list do Post.include Looped end
hook = -> { Post.include Later }
[Post, Topic].each { |klass| klass.include Tagging }
Post.include(Tagging, Module.new)
Post.include
Post.extend Tagging
include Tagging
Post.new.include Tagging
";
        assert_eq!(read(source), Vec::<String>::new());
    }

    #[test]
    fn each_name_is_the_nearest_meaning_something_declares() {
        let kinds = |name: &str| match name {
            "Spree" | "ShopPromotions" | "Mixins" | "Mixins::Shared" => Some(true),
            "Spree::Shipment" | "Post" | "Shared" | "Loose::Class" => Some(false),
            "ShopPromotions::Discounts" => Some(true),
            _ => None,
        };
        let mixed = |target: &str, module: &str, nesting: &str| Mixed {
            target: target.to_owned(),
            module: module.to_owned(),
            nesting: nesting.to_owned(),
            prepend: true,
        };
        // The module is found in the nesting before the top level, the target at the top level.
        assert_eq!(
            resolve(
                &mixed("Spree::Shipment", "Discounts", "ShopPromotions::Patch"),
                &kinds
            ),
            Some(Resolved {
                target: "Spree::Shipment".to_owned(),
                target_is_module: false,
                module: "ShopPromotions::Discounts".to_owned(),
                prepend: true,
            })
        );
        // A module mixed into a module opens `module`.
        assert_eq!(
            resolve(&mixed("::Mixins", "Mixins::Shared", ""), &kinds)
                .map(|found| found.target_is_module),
            Some(true)
        );
        // The nearest `Shared` inside `Mixins` is a module, so the top-level class is not asked.
        assert!(resolve(&mixed("Post", "Shared", "Mixins"), &kinds).is_some());
        // At the top level the nearest `Shared` is a class: Ruby raises, so nothing is written.
        assert_eq!(resolve(&mixed("Post", "Shared", ""), &kinds), None);
        // Either name meaning nothing declines.
        assert_eq!(resolve(&mixed("Ghost", "Mixins", ""), &kinds), None);
        assert_eq!(resolve(&mixed("Post", "Ghost", ""), &kinds), None);
        // A target whose namespace nothing declares cannot be opened by its joined name.
        assert_eq!(resolve(&mixed("Loose::Class", "Mixins", ""), &kinds), None);

        let asked = asked(&[mixed(
            "Spree::Shipment",
            "Discounts",
            "ShopPromotions::Patch",
        )]);
        for name in [
            "Spree",
            "Spree::Shipment",
            "ShopPromotions::Patch::Discounts",
            "ShopPromotions::Discounts",
            "Discounts",
            "ShopPromotions",
        ] {
            assert!(asked.contains(&name.to_owned()), "{name}: {asked:?}");
        }
    }

    #[test]
    fn the_lines_are_written_on_the_target_s_own_body() {
        let resolved = [
            Resolved {
                target: "Spree::Shipment".to_owned(),
                target_is_module: false,
                module: "ShopPromotions::Discounts".to_owned(),
                prepend: true,
            },
            Resolved {
                target: "Spree::Shipment".to_owned(),
                target_is_module: false,
                module: "Tagging".to_owned(),
                prepend: false,
            },
            Resolved {
                target: "Mixins".to_owned(),
                target_is_module: true,
                module: "Shared".to_owned(),
                prepend: false,
            },
        ];
        assert_eq!(
            facts(&resolved)
                .render(&declaring(&["Spree", "ShopPromotions"]))
                .rbs,
            "\
module Spree
class Shipment
  include ::Tagging
  prepend ::ShopPromotions::Discounts
end
end
module Mixins
  include ::Shared
end
"
        );
    }
}
