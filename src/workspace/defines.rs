//! Ruby's `define_method` and `define_singleton_method`: the methods a class or module body makes
//! by name.
//!
//! rubydex reads `def` and `attr_*`, not a call that makes a method when the body runs, so
//! `define_method(:full_name) { … }` declares nothing, and every call of `full_name` answered
//! nothing. This reads the plain shape and says what it makes:
//!
//! - **A statement of a class, module or `class << self` body**, with no receiver. In a `def` or a
//!   block (`included do`, a loop over names) it runs when somebody runs it, with names only running
//!   Ruby knows, so it makes nothing here. `private define_method(…)` is the statement's own.
//! - **A symbol literal names the method.** A string, an interpolation or a variable declines, and
//!   so does a name RBS cannot spell: one bad name would cost the whole generated document.
//! - **The block is the body** ([`BLOCK`]): the method answers what its block does, read where it is
//!   written, with the block's parameters as its own. A proc or method handed instead
//!   (`define_method(:x, instance_method(:y))`) makes a method this cannot read: declared, untyped.
//! - **Visibility is Ruby's**, which `define_method` follows: a `private` or `protected` section, a
//!   `private define_method(…)`, or a later `private :x` makes it private here. `protected` is
//!   written private: refusing a call Ruby allows is the side that answers nothing wrong.
//! - **A `def` of the same name in the same body keeps the name.** It has a place and a body rubydex
//!   reads, and which of the two Ruby keeps depends on an order this does not follow.
//!
//! Text in, facts out, as `workspace/rails/` is. The orchestration is `knowledge::defines`'.

use std::collections::HashSet;

use ruby_prism::{
    BlockNode, CallNode, ClassNode, DefNode, ModuleNode, Node, ParametersNode, SingletonClassNode,
    Visit, parse,
};

use crate::generated::{At, BLOCK, Declared, Facts, Owner, Source};

/// The calls this reads, as a file spells them.
pub const DEFINERS: [&str; 2] = ["define_method", "define_singleton_method"];

/// One method a body makes with [`DEFINERS`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defined {
    /// Which side of which class or module it is a method of.
    pub owner: Owner,
    pub name: String,
    /// The RBS parameter list: the block's parameters, each `untyped`.
    pub parameters: String,
    /// Whether a block was written, whose value the method answers.
    pub block: bool,
    pub private: bool,
    /// The whole call, and the name inside its symbol: the call's start is where the block is
    /// found again ([`BLOCK`]).
    pub at: At,
}

/// Every method `source` makes with [`DEFINERS`], as the module header says.
#[must_use]
pub fn read_defines(source: &str) -> Vec<Defined> {
    let result = parse(source.as_bytes());
    let mut reader = Reader {
        source,
        nesting: Vec::new(),
        found: Vec::new(),
    };
    reader.visit(&result.node());
    reader.found
}

/// Which kind of body a statement is written in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Body {
    Class,
    Module,
    /// `class << self` straight in a class's body, or a module's (`true`).
    Singleton(bool),
}

/// [`read_defines`]' walk: every class, module and `class << self` body, each read statement by
/// statement.
struct Reader<'s> {
    source: &'s str,
    nesting: Vec<String>,
    found: Vec<Defined>,
}

impl<'pr> Visit<'pr> for Reader<'_> {
    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        self.nested(&node.constant_path(), node.body(), Body::Module);
    }

    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        self.nested(&node.constant_path(), node.body(), Body::Class);
    }

    /// A method's body runs when somebody calls it: nothing in it is the class's.
    fn visit_def_node(&mut self, _node: &DefNode<'pr>) {}
}

impl Reader<'_> {
    fn nested(&mut self, path: &Node<'_>, body: Option<Node<'_>>, kind: Body) {
        let spelled = self.spelling(path);
        // `class ::Foo` names the top level, whatever it is written in.
        let saved = spelled
            .starts_with("::")
            .then(|| std::mem::take(&mut self.nesting));
        self.nesting
            .push(spelled.trim_start_matches("::").to_owned());
        self.statements(body.as_ref(), kind);
        // Classes and modules written inside this body, at any depth a walk reaches.
        if let Some(body) = body {
            self.visit(&body);
        }
        self.nesting.pop();
        if let Some(saved) = saved {
            self.nesting = saved;
        }
    }

    /// One body's statements, in order: the visibility sections, and each definer.
    fn statements(&mut self, body: Option<&Node<'_>>, kind: Body) {
        let Some(statements) = body.and_then(Node::as_statements_node) else {
            return;
        };
        let statements: Vec<Node<'_>> = statements.body().iter().collect();
        let kept = defs(&statements);
        let hidden = hidden(&statements);
        let mut private = false;
        for statement in &statements {
            if let Some(singleton) = statement.as_singleton_class_node() {
                self.singleton_class(&singleton, kind);
                continue;
            }
            let Some(call) = statement.as_call_node() else {
                continue;
            };
            if call.receiver().is_some() {
                continue;
            }
            match (call.name().as_slice(), call.arguments().is_some()) {
                (b"private" | b"protected", false) => private = true,
                (b"public", false) => private = false,
                (b"private" | b"protected", true) => {
                    // `private define_method(:x) { … }`: the definer is the one argument.
                    if let Some(inner) = only_call(&call) {
                        self.defined(&inner, kind, true, &kept, &hidden);
                    }
                }
                _ => self.defined(&call, kind, private, &kept, &hidden),
            }
        }
    }

    /// `class << self`, straight in a class's or module's body: its definers are the class
    /// object's.
    fn singleton_class(&mut self, node: &SingletonClassNode<'_>, outer: Body) {
        if node.expression().as_self_node().is_none() {
            return;
        }
        let module = match outer {
            Body::Class => false,
            Body::Module => true,
            Body::Singleton(_) => return,
        };
        self.statements(node.body().as_ref(), Body::Singleton(module));
    }

    /// The method one definer call makes, if it is one this reads.
    fn defined(
        &mut self,
        call: &CallNode<'_>,
        kind: Body,
        private: bool,
        kept: &HashSet<String>,
        hidden: &HashSet<String>,
    ) {
        if call.receiver().is_some() {
            return;
        }
        let singleton = match call.name().as_slice() {
            b"define_method" => false,
            b"define_singleton_method" => true,
            _ => return,
        };
        let Some(arguments) = call.arguments() else {
            return;
        };
        let arguments: Vec<Node<'_>> = arguments.arguments().iter().collect();
        let Some((symbol, value)) = arguments
            .first()
            .and_then(Node::as_symbol_node)
            .and_then(|symbol| symbol.value_loc().map(|value| (symbol, value)))
        else {
            return;
        };
        let name = String::from_utf8_lossy(symbol.unescaped()).into_owned();
        if !spellable(&name) {
            return;
        }
        let class = self.nesting.join("::");
        let owner = match (kind, singleton) {
            (Body::Class, false) => Owner::Instance(class),
            (Body::Class, true) | (Body::Singleton(false), false) => Owner::Singleton(class),
            (Body::Module, false) => Owner::Module(class),
            (Body::Module, true) | (Body::Singleton(true), false) => Owner::ModuleSingleton(class),
            // A class object's own singleton method: a class nothing names.
            (Body::Singleton(_), true) => return,
        };
        // A `def` of the name in this body is the one with a place, and only its own side's.
        if kept.contains(&name) && matches!(owner, Owner::Instance(_) | Owner::Module(_)) {
            return;
        }
        let block = call.block().as_ref().and_then(Node::as_block_node);
        let (parameters, block) = match (block, arguments.len()) {
            (Some(block), 1) => (parameters_of(self.source, &block), true),
            // A proc, a `Method` or an `&block` handed instead: some method, of any shape.
            (None, 2) => ("(*untyped, **untyped)".to_owned(), false),
            _ => return,
        };
        let whole = call.location();
        self.found.push(Defined {
            owner,
            private: private || hidden.contains(&name),
            name,
            parameters,
            block,
            at: (
                (whole.start_offset() as u32, whole.end_offset() as u32),
                (value.start_offset() as u32, value.end_offset() as u32),
            ),
        });
    }

    fn spelling(&self, node: &Node<'_>) -> String {
        let location = node.location();
        self.source
            .get(location.start_offset()..location.end_offset())
            .unwrap_or_default()
            .to_owned()
    }
}

/// The one argument of `private x` when it is a call: `private define_method(:x) { … }`.
fn only_call<'pr>(call: &CallNode<'pr>) -> Option<CallNode<'pr>> {
    let mut arguments = call.arguments()?.arguments().iter();
    match (arguments.next(), arguments.next()) {
        (Some(only), None) => only.as_call_node(),
        _ => None,
    }
}

/// The names this body writes an instance `def` of.
fn defs(statements: &[Node<'_>]) -> HashSet<String> {
    statements
        .iter()
        .filter_map(Node::as_def_node)
        .filter(|definition| definition.receiver().is_none())
        .map(|definition| String::from_utf8_lossy(definition.name().as_slice()).into_owned())
        .collect()
}

/// The names this body makes private or protected by name, wherever it says so: `private :x`,
/// `protected :x, :y`.
fn hidden(statements: &[Node<'_>]) -> HashSet<String> {
    statements
        .iter()
        .filter_map(Node::as_call_node)
        .filter(|call| {
            call.receiver().is_none() && matches!(call.name().as_slice(), b"private" | b"protected")
        })
        .filter_map(|call| call.arguments())
        .flat_map(|arguments| arguments.arguments().iter().collect::<Vec<_>>())
        .filter_map(|argument| {
            argument
                .as_symbol_node()
                .map(|symbol| String::from_utf8_lossy(symbol.unescaped()).into_owned())
        })
        .collect()
}

/// Whether RBS can spell `name` as a `def`: an identifier, and at most one of `?`, `!` or `=` after
/// it.
fn spellable(name: &str) -> bool {
    let mut characters = name.strip_suffix(['?', '!', '=']).unwrap_or(name).chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|rest| rest.is_ascii_alphanumeric() || rest == '_')
}

/// The RBS parameter list a block's parameters give the method it becomes, each `untyped`.
///
/// `define_method` makes a lambda of the block, so its arity is exact: a missing argument raises,
/// as it does for a `def`. `_1` and `it` are required positionals.
fn parameters_of(source: &str, block: &BlockNode<'_>) -> String {
    let Some(written) = block.parameters() else {
        return "()".to_owned();
    };
    if let Some(numbered) = written.as_numbered_parameters_node() {
        let count = usize::from(numbered.maximum());
        return format!("({})", vec!["untyped"; count].join(", "));
    }
    if written.as_it_parameters_node().is_some() {
        return "(untyped)".to_owned();
    }
    let Some(parameters) = written
        .as_block_parameters_node()
        .and_then(|block| block.parameters())
    else {
        return "()".to_owned();
    };
    spelled(source, &parameters)
}

/// [`parameters_of`] for a written list.
fn spelled(source: &str, parameters: &ParametersNode<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.extend(parameters.requireds().iter().map(|_| "untyped".to_owned()));
    parts.extend(parameters.optionals().iter().map(|_| "?untyped".to_owned()));
    // `|a, *rest|` takes any number more; `|a,|` (Prism's implicit rest) takes one and drops the
    // rest, which a lambda does not do, so it says nothing.
    if parameters
        .rest()
        .is_some_and(|rest| rest.as_rest_parameter_node().is_some())
    {
        parts.push("*untyped".to_owned());
    }
    parts.extend(parameters.posts().iter().map(|_| "untyped".to_owned()));
    // A keyword is an identifier, which RBS spells even where Ruby reserves it (`if: untyped`).
    for keyword in parameters.keywords().iter() {
        let location = keyword.location();
        let written = source
            .get(location.start_offset()..location.end_offset())
            .unwrap_or_default();
        let name = written.split_once(':').map_or(written, |(name, _)| name);
        let optional = keyword.as_optional_keyword_parameter_node().is_some();
        parts.push(format!(
            "{}{name}: untyped",
            if optional { "?" } else { "" }
        ));
    }
    // `**nil` takes none, which RBS says by saying nothing.
    if parameters
        .keyword_rest()
        .is_some_and(|rest| rest.as_keyword_rest_parameter_node().is_some())
    {
        parts.push("**untyped".to_owned());
    }
    let block = if parameters.block().is_some() {
        " ?{ (?) -> untyped }"
    } else {
        ""
    };
    format!("({}){block}", parts.join(", "))
}

/// Each method as a member: returning its block's value where one was written, placed at the call.
#[must_use]
pub fn facts(defined: &[Defined], caption: &str) -> Facts {
    let mut facts = Facts::default();
    for found in defined {
        let (definer, returns) = match (&found.owner, found.block) {
            (Owner::Singleton(_) | Owner::ModuleSingleton(_), true) => {
                ("define_singleton_method", BLOCK)
            }
            (Owner::Singleton(_) | Owner::ModuleSingleton(_), false) => {
                ("define_singleton_method", "untyped")
            }
            (_, true) => ("define_method", BLOCK),
            (_, false) => ("define_method", "untyped"),
        };
        facts.declare(Declared {
            owner: found.owner.clone(),
            name: found.name.clone(),
            returns: returns.to_owned(),
            parameters: found.parameters.clone(),
            because: format!(
                "From `{caption}`: `{definer}(:{})` makes this method{}.",
                found.name,
                if found.block {
                    ", which answers what its block does"
                } else {
                    ""
                }
            ),
            at: Some(found.at),
            from: Source::Defined,
            overloads: Vec::new(),
            private: found.private,
        });
    }
    facts
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::declaring;

    fn read(source: &str) -> Vec<String> {
        read_defines(source)
            .iter()
            .map(|found| {
                let side = match &found.owner {
                    Owner::Instance(name) | Owner::Module(name) => format!("{name}#"),
                    Owner::Singleton(name) | Owner::ModuleSingleton(name) => format!("{name}."),
                };
                format!(
                    "{side}{}{}{}{}",
                    found.name,
                    found.parameters,
                    if found.block { " block" } else { "" },
                    if found.private { " private" } else { "" }
                )
            })
            .collect()
    }

    #[test]
    fn a_body_s_definers_make_methods_on_the_side_they_are_written_for() {
        let source = "\
module Shop
  class Widget
    define_method(:plain) { 1 }
    define_method(:takes) do |a, b = 1, *c, d, e:, f: 2, **g, &h|
      a
    end
    define_method(:numbered) { _1 + _2 }
    define_method(:with_it) { it }
    define_method(:bare, instance_method(:plain))
    define_singleton_method(:build) { new }
    define_method(:asked?) { true }
    define_method(:weird, ->(a) { a }) { 1 }
    define_method(\"string\") { 1 }
    define_method(:\"odd name\") { 1 }
    define_method(:+) { 1 }
    define_method(name) { 1 }
    define_method(:trailing) { |a,| a }
    define_method(:keyed) { |if:, **nil| 1 }
    define_method(:empty) { || 1 }
    define_method

    class << self
      define_method(:made) { self }
      define_singleton_method(:nowhere) { 1 }

      class << self
        define_method(:deeper) { 1 }
      end
    end

    class << other
      define_method(:elsewhere) { 1 }
    end

    %i[a b].each { |n| define_method(n) { 1 } }

    def helper
      define_method(:inside) { 1 }
    end

    self.define_method(:on_self) { 1 }
    Widget.define_method(:on_constant) { 1 }
  end

  class Empty
  end

  module Mixin
    define_method(:mixed) { 1 }
    define_singleton_method(:module_level) { 1 }

    class << self
      define_method(:also_module_level) { 1 }
    end
  end
end

class ::Top
  define_method(:top) { 1 }
end

define_method(:main) { 1 }
";
        assert_eq!(
            read(source),
            [
                "Shop::Widget#plain() block",
                "Shop::Widget#takes(untyped, ?untyped, *untyped, untyped, e: untyped, ?f: untyped, **untyped) ?{ (?) -> untyped } block",
                "Shop::Widget#numbered(untyped, untyped) block",
                "Shop::Widget#with_it(untyped) block",
                "Shop::Widget#bare(*untyped, **untyped)",
                "Shop::Widget.build() block",
                "Shop::Widget#asked?() block",
                "Shop::Widget#trailing(untyped) block",
                "Shop::Widget#keyed(if: untyped) block",
                "Shop::Widget#empty() block",
                "Shop::Widget.made() block",
                "Shop::Mixin#mixed() block",
                "Shop::Mixin.module_level() block",
                "Shop::Mixin.also_module_level() block",
                "Top#top() block",
            ]
        );
    }

    #[test]
    fn visibility_is_the_body_s_as_ruby_reads_it() {
        let source = "\
class Widget
  define_method(:open) { 1 }
  define_method(:hidden_later) { 1 }
  private define_method(:wrapped) { 1 }

  private

  define_method(:sectioned) { 1 }

  public

  define_method(:reopened) { 1 }

  protected

  define_method(:guarded) { 1 }
  private :hidden_later
  private weird
  protected :open_later, :also
  private self.define_method(:on_self) { 1 }
  define_method(:open_later) { 1 }
end
";
        assert_eq!(
            read(source),
            [
                "Widget#open() block",
                "Widget#hidden_later() block private",
                "Widget#wrapped() block private",
                "Widget#sectioned() block private",
                "Widget#reopened() block",
                "Widget#guarded() block private",
                "Widget#open_later() block private",
            ]
        );
    }

    #[test]
    fn a_def_of_the_same_name_keeps_it() {
        let source = "\
class Widget
  define_method(:title) { 1 }
  define_singleton_method(:title) { 2 }

  def title
    \"x\"
  end

  def self.plain
  end
end
";
        assert_eq!(read(source), ["Widget.title() block"]);
    }

    #[test]
    fn each_method_is_a_member_placed_at_its_call() {
        let source = "\
class Widget
  define_method(:loud) { |times| \"x\" * times }
  define_method(:bare, instance_method(:loud))
  private define_singleton_method(:build) { new }
  define_singleton_method(:handed, method(:build))
end
";
        let found = read_defines(source);
        let ((start, end), (name, name_end)) = found[0].at;
        assert_eq!(
            &source[start as usize..end as usize],
            "define_method(:loud) { |times| \"x\" * times }"
        );
        assert_eq!(&source[name as usize..name_end as usize], "loud");
        let rbs = facts(&found, "app/models/widget.rb")
            .render(&declaring(&[]))
            .rbs;
        assert!(
            rbs.contains("def loud: (untyped) -> ReturnedByItsBlock"),
            "{rbs}"
        );
        assert!(
            rbs.contains(
                "`define_method(:loud)` makes this method, which answers what its block does."
            ),
            "{rbs}"
        );
        assert!(
            rbs.contains("def bare: (*untyped, **untyped) -> untyped"),
            "{rbs}"
        );
        assert!(
            rbs.contains("private def self.build: () -> ReturnedByItsBlock"),
            "{rbs}"
        );
        assert!(rbs.contains("`define_singleton_method(:build)`"), "{rbs}");
        assert!(
            rbs.contains("def self.handed: (*untyped, **untyped) -> untyped"),
            "{rbs}"
        );
    }
}
