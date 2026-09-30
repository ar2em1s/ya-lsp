//! Ruby's `Singleton`: the classes a file writes `include Singleton` in, and the `instance` each
//! then answers.
//!
//! `Singleton.included` extends the class with `SingletonClassMethods`, whose `instance` builds the
//! class's one object: `ActivityPub::TagManager.instance`. That happens in a hook, so rubydex sees
//! an `include` and no `instance`, and the RBS Ruby ships leaves `SingletonClassMethods` empty.
//!
//! Text in, facts out, as `workspace/rails/` is. The orchestration is `knowledge::singletons`'.

use ruby_prism::{CallNode, ClassNode, DefNode, ModuleNode, Node, Visit, parse};

use crate::generated::{At, Declared, Facts, Owner, Source};

/// Ruby's module, as a file spells it.
pub const SINGLETON: &str = "Singleton";

/// One class that includes [`SINGLETON`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Included {
    /// The class, spelled from the top level as the file's nesting spells it; also where the
    /// `Singleton` it writes is looked up from.
    pub class: String,
    /// The whole `include`, and the `Singleton` in it.
    pub at: At,
}

/// Every class in `source` whose body writes `include Singleton` (or `::Singleton`).
#[must_use]
pub fn read_singletons(source: &str) -> Vec<Included> {
    let result = parse(source.as_bytes());
    let mut reader = Reader {
        source,
        nesting: Vec::new(),
        in_class: false,
        found: Vec::new(),
    };
    reader.visit(&result.node());
    reader.found
}

/// [`read_singletons`]' walk.
struct Reader<'s> {
    source: &'s str,
    nesting: Vec<String>,
    /// Whether the innermost body is a `class`'s: a module's `include Singleton` is its includers'.
    in_class: bool,
    found: Vec<Included>,
}

impl<'pr> Visit<'pr> for Reader<'_> {
    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        self.nested(&node.constant_path(), node.body(), false);
    }

    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        self.nested(&node.constant_path(), node.body(), true);
    }

    /// A method's body runs when somebody calls it: an `include` there is not the class's.
    fn visit_def_node(&mut self, _node: &DefNode<'pr>) {}

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        if self.in_class
            && node.receiver().is_none()
            && node.name().as_slice() == b"include"
            && let Some(arguments) = node.arguments()
        {
            for argument in arguments.arguments().iter() {
                // Spelled `Singleton` or `::Singleton`, so a constant: nothing else is written so.
                if self.spelling(&argument).trim_start_matches("::") == SINGLETON {
                    let (whole, name) = (node.location(), argument.location());
                    self.found.push(Included {
                        class: self.nesting.join("::"),
                        at: (
                            (whole.start_offset() as u32, whole.end_offset() as u32),
                            (name.start_offset() as u32, name.end_offset() as u32),
                        ),
                    });
                }
            }
        }
        ruby_prism::visit_call_node(self, node);
    }
}

impl Reader<'_> {
    fn nested(&mut self, path: &Node<'_>, body: Option<Node<'_>>, class: bool) {
        let spelled = self.spelling(path);
        let pushed = spelled.trim_start_matches("::").to_owned();
        let outer = std::mem::replace(&mut self.in_class, class);
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
        self.in_class = outer;
    }

    fn spelling(&self, node: &Node<'_>) -> String {
        let location = node.location();
        self.source
            .get(location.start_offset()..location.end_offset())
            .unwrap_or_default()
            .to_owned()
    }
}

/// `instance` on each class's class side: the class's one object.
#[must_use]
pub fn facts(included: &[Included], caption: &str) -> Facts {
    let mut facts = Facts::default();
    for found in included {
        facts.declare(Declared {
            owner: Owner::Singleton(found.class.clone()),
            name: "instance".to_owned(),
            returns: format!("::{}", found.class),
            parameters: "()".to_owned(),
            because: format!(
                "From `{caption}`: `include Singleton` gives the class `instance`, its one object."
            ),
            at: Some(found.at),
            from: Source::Convention,
            overloads: Vec::new(),
            private: false,
        });
    }
    facts
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::declaring;

    #[test]
    fn a_class_that_includes_singleton_answers_instance() {
        let source = "\
module ActivityPub
  class TagManager
    include Singleton
    include Other, ::Singleton

    def helper
      include Singleton
    end
  end

  module Mixin
    include Singleton
  end
end

module Outer
  class ::Top
    include Singleton
  end
end

class Plain
  include Enumerable
  extend Singleton
  include
  include Singleton.dup
end
";
        let found = read_singletons(source);
        let named: Vec<&str> = found.iter().map(|found| found.class.as_str()).collect();
        assert_eq!(
            named,
            ["ActivityPub::TagManager", "ActivityPub::TagManager", "Top"]
        );
        let ((start, end), (name, name_end)) = found[0].at;
        assert_eq!(&source[start as usize..end as usize], "include Singleton");
        assert_eq!(&source[name as usize..name_end as usize], "Singleton");
        let rbs = facts(&found[..1], "app/lib/tag_manager.rb")
            .render(&declaring(&["ActivityPub"]))
            .rbs;
        assert!(
            rbs.contains("def self.instance: () -> ::ActivityPub::TagManager"),
            "{rbs}"
        );
    }
}
