//! `class_methods do`: the `def`s in it, and the module `ActiveSupport::Concern` builds for them.
//!
//! `ActiveSupport::Concern#class_methods` is four lines of Ruby, and every one matters here:
//!
//! ```ruby
//! def class_methods(&class_methods_module_definition)
//!   mod = const_defined?(:ClassMethods, false) ? const_get(:ClassMethods)
//!                                              : const_set(:ClassMethods, Module.new)
//!   mod.module_eval(&class_methods_module_definition)
//! end
//! ```
//!
//! It builds (or reopens) a nested `ClassMethods` module and evaluates the block on it, and
//! `append_features` ends with `base.extend const_get(:ClassMethods)`. So a `def` in that block is
//! a **class method of every class that includes the concern**. Three spellings reach that same
//! edge:
//! - `class_methods do … end`;
//! - a hand-written `module ClassMethods`;
//! - an `included do` holding a bare `extend M`, with no such module written anywhere.
//!
//! **No file writes the line that gets them there.** rubydex is right to find nothing:
//! `base.extend` runs at load time, and no source has an `extend` to record. This file spends
//! that edge: it writes each `def` onto the singleton of every class that includes the concern,
//! so an ordinary ancestor walk finds it and nothing outside this directory knows the convention.
//!
//! # Why the block is still not a macro host
//!
//! `models::HOSTS` declines `class_methods do`, correctly: the block is evaluated on a plain
//! `Module`, which has no `has_many`, so a macro there is broken Ruby, not a missed declaration.
//! The `def`s in it were never that test's question, and in practice the block holds `def`s and
//! no macros.
//!
//! # The spelling, and why it is a fan-out
//!
//! One `def self.` per including class: the shape a class-side [`scope`](super::relations)
//! already has.
//!
//! The obvious alternative is one `module <concern>::ClassMethods`, `extend`ed onto the includers
//! by a line this generator writes. It does not work: **a mixin that arrives in a document indexed
//! after its class was resolved is never linearized onto that class**, and a generated document
//! is always that shape.
//! - The instance-side `include` the route helpers use is the one exception.
//! - Every singleton-side spelling fails, in RBS and Ruby alike: `extend M`,
//!   `class << self; include M; end` and `singleton_class.include M` all leave the member
//!   unreachable, and re-indexing the class's own file afterwards does not repair it.
//!
//! What does reach a class object is a member written straight onto it, so that is what is
//! written.
//!
//! The fan-out needs the includers, and they can be enumerated: `Context::includers` in
//! `analysis::synthesize` resolves the `include` edges of the whole project after the walk,
//! transitively through concerns that include concerns, and hands the classes at the ends of
//! those chains to every reader in this directory.

use std::collections::{BTreeMap, BTreeSet};

use ruby_prism::{CallNode, StatementsNode};

use super::syntax::{constant_spelling, def_span, parameters_of, spellable, symbol_or_string};
use crate::generated::{At, Declared, Facts, Namespaces, Owner, Source};

/// One `def` written as a statement of a `class_methods do` block.
#[derive(Debug)]
pub struct ClassMethod {
    name: String,
    /// Which of the two spellings wrote it, for the provenance sentence: a reader sent to the `def`
    /// should be told whether the file says `class_methods do` or `module ClassMethods`, the two
    /// lines they will be looking at.
    spelled: &'static str,
    /// The RBS parameter list the `def`'s own parameters imply, every type `untyped`.
    parameters: String,
    /// The `def` keyword through the end of the parameter list, and the name inside it, so the
    /// rendered declaration maps back to the line a user wrote.
    at: (u32, u32),
    name_at: (u32, u32),
}

/// The block's statements, when this call is the one that opens one.
///
/// A `class_methods` written **in a class** is declined on purpose: `class_methods` is defined on
/// `ActiveSupport::Concern`, which is extended onto modules, so the call raises `NoMethodError` in
/// a class body.
///
/// Whether the call has a **receiver** is deliberately not asked here. `models::bare` already asked
/// it of every call it hands over, and only it knows the answer: a receiver naming the
/// `with_options` merger the enclosing block was handed is still the body's own call, and this
/// function cannot see that block. Asking twice would add an arm the caller makes unreachable.
pub(super) fn body<'pr>(
    node: &CallNode<'pr>,
    called: &str,
    module: bool,
) -> Option<StatementsNode<'pr>> {
    if !module || called != "class_methods" {
        return None;
    }
    node.block()?.as_block_node()?.body()?.as_statements_node()
}

/// The `def`s of a hand-written `module ClassMethods`, as statements of a module body.
///
/// The rarer spelling. Both build the same module (`ActiveSupport::Concern#class_methods` calls
/// `const_get(:ClassMethods)` when one already exists), so both feed one list, and a concern that
/// writes both gets one set of members.
///
/// **Read from the source, not taken off the graph**, although the graph does hold this module. A
/// source reader declares the fact whether or not the edge was recorded, and both spellings then
/// arrive by one road. The nested module is a real declaration either way; what neither spelling
/// gives anybody is the `extend` onto the includer, which [`declare`] writes.
///
/// A **class** is declined, for [`body`]'s reason: a class cannot be `include`d, so a
/// `ClassMethods` nested in one reaches no singleton.
///
/// Only statements of the body, this directory's rule everywhere: a `module ClassMethods` inside an
/// `if` is Ruby that only runs.
pub(super) fn nested(
    source: &str,
    statements: &StatementsNode<'_>,
    module: bool,
) -> Vec<ClassMethod> {
    if !module {
        return Vec::new();
    }
    let mut found = Vec::new();
    for statement in statements.body().iter() {
        let Some(nested) = statement.as_module_node() else {
            continue;
        };
        // The name exactly, not its last segment: `module Foo::ClassMethods` inside another module
        // belongs to `Foo`, which this concern does not extend onto anybody.
        if constant_spelling(source, &nested.constant_path()) != CLASS_METHODS {
            continue;
        }
        let Some(body) = nested.body().and_then(|body| body.as_statements_node()) else {
            continue;
        };
        found.extend(read(source, &body, MODULE));
    }
    found
}

/// One module an `included do` block extends onto every including class.
#[derive(Debug)]
pub(super) struct Extended {
    /// The constant as the file spells it, resolved against the project by the caller.
    pub name: String,
    /// The `extend Foo` statement and the constant inside it, so a reader can be sent to the line
    /// that installed the member when nothing better is available.
    pub at: At,
}

/// The modules an `included do` block `extend`s, which land on every including class.
///
/// `ActiveSupport::Concern` `class_eval`s the block on each includer, so a bare `extend M` in it
/// puts `M`'s **instance** methods on that class's singleton: the destination `class_methods do`
/// reaches, with no `ClassMethods` module at all.
///
/// Real example: `activemodel/lib/active_model/api.rb` holds `extend ActiveModel::Naming` and
/// `extend ActiveModel::Translation`. Every model reaches them through `ActiveRecord::Base`'s
/// `include ActiveModel::API`, and they install `model_name` and `human_attribute_name`.
///
/// **This returns a name, never a member.** That is how it differs from the two above, and why it
/// is finished elsewhere: the `def`s are in `M`'s own file, which no list in this directory would
/// hand a reader. `analysis::synthesize` asks the graph for them.
///
/// A **class** is declined for [`body`]'s reason: `included` is `ActiveSupport::Concern`'s, and an
/// `include` cannot name a class.
///
/// Only statements of the block, and only a bare `extend`: `Foo.extend M` is somebody else's
/// method, and an `extend` inside an `if` is Ruby that only runs.
pub(super) fn extended(
    source: &str,
    statements: &StatementsNode<'_>,
    module: bool,
) -> Vec<Extended> {
    if !module {
        return Vec::new();
    }
    let mut found = Vec::new();
    for statement in statements.body().iter() {
        let Some(call) = statement.as_call_node() else {
            continue;
        };
        if call.receiver().is_some() || call.name().as_slice() != b"included" {
            continue;
        }
        let Some(block) = call
            .block()
            .and_then(|block| block.as_block_node()?.body()?.as_statements_node())
        else {
            continue;
        };
        for inner in block.body().iter() {
            let Some(call) = inner.as_call_node() else {
                continue;
            };
            if call.receiver().is_some() || call.name().as_slice() != b"extend" {
                continue;
            }
            let Some(argument) = call
                .arguments()
                .and_then(|arguments| arguments.arguments().iter().next())
            else {
                continue;
            };
            // A constant and nothing else: `extend Module.new { … }` names no module this can
            // follow, so it names none.
            if argument.as_constant_read_node().is_none()
                && argument.as_constant_path_node().is_none()
            {
                continue;
            }
            let whole = call.location();
            let inside = argument.location();
            found.push(Extended {
                name: constant_spelling(source, &argument),
                at: (
                    (whole.start_offset() as u32, whole.end_offset() as u32),
                    (inside.start_offset() as u32, inside.end_offset() as u32),
                ),
            });
        }
    }
    found
}

/// The `def`s one module declares in its own body, read out of that module's own file.
///
/// The far end of [`extended`]. `extend ActiveModel::Naming` inside an `included do` says which
/// module; this says what a class gains by it. The two are in different files, which is why this
/// takes a name as well as a source, and why `analysis::synthesize` asks the graph which file to
/// hand over.
///
/// **The module's own body, never its ancestors.** `extend M` really does install the instance
/// methods of `M`'s ancestors, and following them is right for Ruby and wrong for a project: Rails
/// writes `include` statements inside a `def` in these modules. Walking them made `Category.valid?`
/// (which raises in Ruby) answer `ActiveModel::Validations#valid?`, and put instance methods into a
/// class object's completion list. So this reads the statements of one body, as every reader here
/// does.
///
/// Visibility, `def self.` and unspellable names follow [`read`]'s rules unchanged: a `private`
/// `def` is extended onto nobody, and a singleton method of the module is not installed by an
/// `extend` at all.
#[must_use]
pub fn installed(source: &str, module_name: &str) -> Vec<ClassMethod> {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut found = Vec::new();
    let mut nesting: Vec<String> = Vec::new();
    walk_bodies(
        source,
        parsed
            .node()
            .as_program_node()
            .map(|program| program.statements()),
        module_name,
        &mut nesting,
        &mut found,
    );
    found
}

/// One body, then the class and module bodies written as statements of it.
///
/// The shape of `models::Models::walk`, for its reason: a generic visitor descends into every
/// method body in the file, which on a large one is thousands of frames on a 2 MiB stack.
fn walk_bodies(
    source: &str,
    statements: Option<StatementsNode<'_>>,
    wanted: &str,
    nesting: &mut Vec<String>,
    found: &mut Vec<ClassMethod>,
) {
    let Some(statements) = statements else {
        return;
    };
    if !nesting.is_empty() && nesting.join("::") == wanted {
        found.extend(read(source, &statements, EXTENDED));
        return;
    }

    for statement in statements.body().iter() {
        // A `class` body is descended into as well as a `module`'s, because the wanted module may
        // be nested in one (`Random::Formatter`). What is *matched* is still a module: an `extend`
        // names one, and the name is the whole path, not its last segment.
        let (path, body) = if let Some(module) = statement.as_module_node() {
            (module.constant_path(), module.body())
        } else if let Some(class) = statement.as_class_node() {
            (class.constant_path(), class.body())
        } else {
            continue;
        };
        nesting.push(constant_spelling(source, &path));
        walk_bodies(
            source,
            body.and_then(|body| body.as_statements_node()),
            wanted,
            nesting,
            found,
        );
        nesting.pop();
    }
}

/// The nested module Rails builds, spelled as Ruby spells it.
///
/// Public because [`Analysis::contribution`](crate::analysis) filters documents on it. A file that
/// is only a hand-written `module ClassMethods` calls nothing and references nothing, so only the
/// module's name can put it in front of a reader. Exported for [`MODEL_CALLS`](super::MODEL_CALLS)'
/// reason: the table that decides which documents a generator sees lives outside this directory, so
/// its words must come from inside it.
pub const CLASS_METHODS: &str = "ClassMethods";

/// The two spellings, as a provenance sentence names them.
pub(super) const BLOCK: &str = "class_methods do";
const MODULE: &str = "module ClassMethods";
const EXTENDED: &str = "included do … extend";

/// Every `def` this block installs on the includer's singleton, in source order.
///
/// Visibility is the file's own, read exactly as [`super::entrypoints`] reads a mailer's: a bare
/// `private` or `protected` closes the public section, and `private :name` names one already
/// written. Bare `private` inside these blocks is common, not hypothetical.
///
/// Only statements of the block, the rule every reader in this directory keeps: a `def` inside an
/// `if` inside the block is Ruby that only runs.
///
/// A `def self.` is declined. It is a singleton method of the `ClassMethods` module itself, which
/// `extend` installs on nothing: the module is what is extended, not what extends.
///
/// `private` **is** asked for a receiver, unlike in [`body`], for the opposite reason: this walk is
/// its own, and nothing upstream has narrowed these statements. `something.private` calls somebody
/// else's method and closes nothing.
pub(super) fn read(
    source: &str,
    block: &StatementsNode<'_>,
    spelled: &'static str,
) -> Vec<ClassMethod> {
    let mut found: Vec<ClassMethod> = Vec::new();
    let mut hidden: Vec<String> = Vec::new();
    let mut visible = true;
    for statement in block.body().iter() {
        if let Some(call) = statement.as_call_node()
            && call.receiver().is_none()
            && matches!(call.name().as_slice(), b"private" | b"protected")
        {
            match call.arguments() {
                None => visible = false,
                Some(arguments) => hidden.extend(
                    arguments
                        .arguments()
                        .iter()
                        .filter_map(|argument| symbol_or_string(source, &argument))
                        .map(|(name, _)| name),
                ),
            }
        }
        if let Some(written) = statement.as_def_node()
            && visible
            && written.receiver().is_none()
        {
            let name = String::from_utf8_lossy(written.name().as_slice()).into_owned();
            if !spellable(&name) {
                continue;
            }
            let at = written.name_loc();
            found.push(ClassMethod {
                parameters: parameters_of(source, written.parameters().as_ref()),
                name,
                spelled,
                at: def_span(&written),
                name_at: (at.start_offset() as u32, at.end_offset() as u32),
            });
        }
    }
    found.retain(|method| !hidden.contains(&method.name));
    found
}

/// Where a set of class methods came from, for the sentence above each declaration.
///
/// Three spellings reach one `declare`, and a reader sent to the `def` should be told which line of
/// which file installed it on the class they asked about.
pub struct From<'a> {
    /// The file the `def`s were read out of.
    pub file: &'a str,
    /// The module a class `include`s, which is what makes it an includer.
    pub concern: &'a str,
    /// The module an `included do … extend` names, for that shape. `None` for the two spellings
    /// whose `def`s are in the concern's own body.
    pub via: Option<&'a str>,
}

/// Declare each of them on the singleton of every class that includes the concern.
///
/// **The name Ruby gives the module is not the name this writes.**
/// `ActiveSupport::Concern#class_methods` builds `<concern>::ClassMethods`, and `append_features`
/// ends with `base.extend const_get(:ClassMethods)`, so the *fact* is one module and one `extend`.
/// Writing that down does not make it true for the graph: the `extend` would arrive in a generated
/// document, always indexed after the includer was resolved, and such a mixin is never linearized
/// (see the module docs). So the edge is spent here, once per includer, and the includer ends up
/// holding what Ruby would put in front of it.
///
/// **The span is still the `def` a person typed**, however many classes it is written onto: the
/// class-side `scope`'s rule, for its reason. One `def` named by several declarations is still one
/// place.
///
/// `Source::Convention`, as for a mailer's action: the `def` is really in the file and the class
/// method is really installed, but the *type* is this table's, not the file's.
///
/// A concern **nothing includes** declares nothing. That is Ruby, not caution: the module itself
/// never answers these names (`Tallyable.tally_by` raises), and with no includer there is no class
/// object that could.
pub fn declare(
    facts: &mut Facts,
    from: &From<'_>,
    methods: &[ClassMethod],
    includers: &BTreeMap<String, BTreeSet<String>>,
    namespaces: &Namespaces,
) {
    if methods.is_empty() {
        return;
    }
    let From { file, concern, via } = *from;
    for includer in includers.get(concern).into_iter().flatten() {
        // The one decline this generator owes, and it is the joined-name rule, not a judgement
        // about the includer. `Owner::Singleton` opens `class <includer>`, so a name under a
        // namespace nothing declares would introduce that namespace. That RBS does not parse, and
        // `Synthesized::record` refuses the whole document for it, costing every other declaration
        // in the file.
        if !namespaces.spellable(includer) {
            continue;
        }
        for method in methods {
            facts.declare(Declared {
                owner: Owner::Singleton(includer.clone()),
                name: method.name.clone(),
                returns: "untyped".to_owned(),
                parameters: method.parameters.clone(),
                because: match via {
                    None => format!(
                        "From `{file}`, `def {}` in `{}` in `{concern}`, which `{includer}` \
                         includes.",
                        method.name, method.spelled
                    ),
                    Some(module) => format!(
                        "From `{file}`, `def {}` in `{module}`, which `{concern}`'s `{}` puts on \
                         every including class — here `{includer}`.",
                        method.name, method.spelled
                    ),
                },
                at: Some((method.at, method.name_at)),
                from: Source::Convention,
                overloads: Vec::new(),
            });
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::super::models::{Elsewhere, read_model};
    use super::{BTreeMap, BTreeSet};
    use crate::generated::declaring;

    /// The one include every test but two shares.
    const INCLUDED: &[(&str, &str)] = &[("Tallyable", "Ledger")];

    /// What one file declares, given which namespaces the project writes down and which class
    /// includes the concern: all this reader needs from elsewhere.
    fn rbs(source: &str, modules: &[&str], includes: &[(&str, &str)]) -> String {
        let namespaces = declaring(modules);
        let mut includers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (concern, includer) in includes {
            includers
                .entry((*concern).to_owned())
                .or_default()
                .insert((*includer).to_owned());
        }
        let model = read_model(source);
        let mut facts = model.signatures(
            "app/models/concerns/tallyable.rb",
            &Elsewhere {
                namespaces: &namespaces,
                includers: &includers,
                ..Elsewhere::nothing()
            },
        );
        // The second entry point, which a concern's class methods come out of: see
        // [`super::models::Model::class_methods`].
        facts.extend(model.class_methods(
            "app/models/concerns/tallyable.rb",
            &includers,
            &namespaces,
        ));
        facts.render(&namespaces).rbs
    }

    /// Pinned whole, like the schema's and the delegate's: every rule in this reader shows in the
    /// text, and asserting one predicate at a time lets a change of shape pass five green tests.
    ///
    /// The owner is the **includer**, never the concern: Ruby puts these in front of `Ledger`, and
    /// `Tallyable` itself answers none of them.
    #[test]
    fn the_rbs_a_class_methods_block_declares() {
        assert_eq!(
            rbs(
                "\
module Tallyable
  extend ActiveSupport::Concern

  class_methods do
    def tally_by(column, limit = nil, *rest, scale:, unit: nil, **options)
    end

    def primary_key=(value)
    end

    def ==(other)
    end

    def []=(key, value)
    end

    def self.not_extended
    end

    def named_private
    end

    private :named_private

    Foo.private

    def still_public
    end

    private

    def after_private
    end
  end
end
",
                &["Tallyable", "Ledger"],
                &[("Tallyable", "Ledger")],
            ),
            "\
class Ledger
  # From `app/models/concerns/tallyable.rb`, `def tally_by` in `class_methods do` in `Tallyable`, \
which `Ledger` includes.
  def self.tally_by: (untyped, ?untyped, *untyped, scale: untyped, ?unit: untyped, **untyped) -> \
untyped
  # From `app/models/concerns/tallyable.rb`, `def primary_key=` in `class_methods do` in \
`Tallyable`, which `Ledger` includes.
  def self.primary_key=: (untyped) -> untyped
  # From `app/models/concerns/tallyable.rb`, `def still_public` in `class_methods do` in \
`Tallyable`, which `Ledger` includes.
  def self.still_public: () -> untyped
end
"
        );
    }

    /// The other spelling, reaching the same list.
    ///
    /// `ActiveSupport::Concern#class_methods` reopens the module a file declares instead of
    /// building a second one, so a concern that writes both hands its includer one set of members.
    ///
    /// **The block speaks first, which is Ruby's order**, not the file's: `module_eval` runs the
    /// block on the module the hand-written one already declared, so a name written both ways is
    /// the block's. `Facts` keeps the first of an equal-ranked pair, so stating the block's `def`s
    /// first makes that true.
    #[test]
    fn a_hand_written_class_methods_module_is_the_same_list() {
        let rendered = rbs(
            "\
module Tallyable
  extend ActiveSupport::Concern

  module ClassMethods
    def counted_name(scope)
    end

    def self.not_extended
    end

    def hidden
    end

    private :hidden
  end

  class_methods do
    def tally_by(column)
    end
  end
end
",
            &["Tallyable", "Ledger"],
            INCLUDED,
        );
        assert_eq!(
            rendered,
            "\
class Ledger
  # From `app/models/concerns/tallyable.rb`, `def tally_by` in `class_methods do` in `Tallyable`, \
which `Ledger` includes.
  def self.tally_by: (untyped) -> untyped
  # From `app/models/concerns/tallyable.rb`, `def counted_name` in `module ClassMethods` in \
`Tallyable`, which `Ledger` includes.
  def self.counted_name: (untyped) -> untyped
end
"
        );
    }

    /// A `module ClassMethods` nested in a **class** reaches nobody: a class cannot be `include`d,
    /// so nothing runs the `extend`. And a `module Foo::ClassMethods` belongs to `Foo`, not this
    /// concern, so the name is matched whole, not by its last segment.
    #[test]
    fn a_class_methods_module_that_extends_onto_nothing_declares_nothing() {
        for source in [
            "class Ledger\n  module ClassMethods\n    def counted_name\n    end\n  end\nend\n",
            "module Tallyable\n  module Foo::ClassMethods\n    def counted_name\n    end\n  \
             end\nend\n",
            "module Tallyable\n  if true\n    module ClassMethods\n      def counted_name\n  \
             end\n    end\n  end\nend\n",
            // A module with nothing in it has no statements to read.
            "module Tallyable\n  module ClassMethods\n  end\nend\n",
        ] {
            assert_eq!(
                rbs(source, &["Tallyable", "Ledger", "Foo"], INCLUDED),
                "",
                "{source}"
            );
        }
    }

    /// Every shape of `included do` that extends nothing, each a different sentence of Ruby:
    /// - a statement that is not a call;
    /// - a call that is not `extend`;
    /// - an `extend` with a receiver, or with no argument;
    /// - an argument that is not a constant (`Module.new { … }` names no module to follow).
    ///
    /// A block that is not written out reaches none of them, and an `include` cannot name a
    /// `class`.
    #[test]
    fn an_included_block_that_extends_nothing_installs_nothing() {
        for source in [
            "module Nameable\n  included do\n    LIMIT = 5\n  end\nend\n",
            "module Nameable\n  included do\n    validates :title\n  end\nend\n",
            "module Nameable\n  included do\n    Foo.extend Naming\n  end\nend\n",
            "module Nameable\n  included do\n    extend\n  end\nend\n",
            "module Nameable\n  included do\n    extend Module.new { }\n  end\nend\n",
            "module Nameable\n  included(&:naming)\nend\n",
            "class Nameable\n  included do\n    extend Naming\n  end\nend\n",
        ] {
            let read = read_model(source);
            let found: Vec<(&str, &str)> = read
                .extended()
                .map(|(concern, module, _)| (concern, module))
                .collect();
            assert!(found.is_empty(), "{source}: {found:?}");
        }
        // And the one that does, so the loop above asserts an absence the reader can see.
        let read =
            read_model("module Nameable\n  included do\n    extend Ns::Naming\n  end\nend\n");
        let found: Vec<(&str, &str)> = read
            .extended()
            .map(|(concern, module, _)| (concern, module))
            .collect();
        assert_eq!(found, vec![("Nameable", "Ns::Naming")]);
    }

    /// What an `included do … extend M` installs, read out of `M`'s own file.
    ///
    /// The far end of the third spelling, and the only reader in this directory handed a **name**
    /// as well as a source: the module is in a file the concern only mentions.
    #[test]
    fn the_defs_a_module_installs_when_it_is_extended() {
        let source = "\
module Outer
  module Naming
    def model_name
    end

    def self.not_installed
    end

    def hidden
    end

    private :hidden
  end
end

module Empty
end

class Bare
end

class Holder
  module Nested
    def held
    end
  end
end
";
        let names = |module: &str| -> Vec<String> {
            super::installed(source, module)
                .into_iter()
                .map(|method| method.name)
                .collect()
        };
        assert_eq!(names("Outer::Naming"), vec!["model_name".to_owned()]);
        // A `class` body is descended into as well as a `module`'s: `Random::Formatter` is a module
        // nested in a class, and a walk that followed only `module` would never reach it.
        assert_eq!(names("Holder::Nested"), vec!["held".to_owned()]);
        // The name is the whole path, never its last segment, and a name nothing declares declares
        // nothing.
        assert!(names("Naming").is_empty());
        assert!(names("Outer::Missing").is_empty());
    }

    /// `class_methods` is defined on `ActiveSupport::Concern`, which is extended onto **modules**,
    /// so the call raises `NoMethodError` in a class body.
    #[test]
    fn a_class_methods_in_a_class_declares_nothing() {
        assert_eq!(
            rbs(
                "class Ledger\n  class_methods do\n    def never_reached\n    end\n  end\nend\n",
                &["Ledger"],
                &[("Ledger", "Ledger")],
            ),
            ""
        );
    }

    /// The three shapes that are the name without the block, each reaching a `None` the reader must
    /// handle: no block, a block passed as an argument, and a written-out block with nothing in it.
    #[test]
    fn a_class_methods_without_a_written_block_declares_nothing() {
        for source in [
            "module Tallyable\n  class_methods\nend\n",
            "module Tallyable\n  class_methods(&:tally)\nend\n",
            "module Tallyable\n  class_methods do\n  end\nend\n",
        ] {
            assert_eq!(
                rbs(source, &["Tallyable", "Ledger"], INCLUDED),
                "",
                "{source}"
            );
        }
    }

    /// A call **on** something is not this one. `Foo.class_methods do … end` is whatever `Foo`
    /// defines, and nothing here knows what that is.
    #[test]
    fn a_class_methods_with_a_receiver_is_not_the_concerns() {
        assert_eq!(
            rbs(
                "module Tallyable\n  Foo.class_methods do\n    def tally_by(column)\n    end\n  \
                 end\nend\n",
                &["Tallyable", "Ledger"],
                INCLUDED,
            ),
            ""
        );
    }

    /// A concern nothing includes declares nothing. That is Ruby, not caution: the module itself
    /// never answers these names.
    #[test]
    fn a_concern_nothing_includes_declares_nothing() {
        let source = "module Tallyable\n  class_methods do\n    def tally_by(column)\n    end\n  \
                      end\nend\n";
        assert!(
            rbs(source, &["Tallyable", "Ledger"], INCLUDED).contains("def self.tally_by"),
            "a class that includes it gets them"
        );
        assert_eq!(rbs(source, &["Tallyable", "Ledger"], &[]), "");
    }

    /// The joined name must introduce no namespace, and the name that could is the **includer's**:
    /// RBS that does not parse, for which `Synthesized::record` refuses the whole document.
    #[test]
    fn an_includer_nothing_declares_is_not_joined_onto() {
        let source = "module Tallyable\n  class_methods do\n    def tally_by(column)\n    end\n  \
                      end\nend\n";
        assert!(
            rbs(
                source,
                &["Tallyable", "Api", "Api::Ledger"],
                &[("Tallyable", "Api::Ledger")]
            )
            .contains("def self.tally_by"),
            "`Api` is spellable once the project declares the name under it"
        );
        assert_eq!(
            rbs(source, &["Tallyable"], &[("Tallyable", "Api::Ledger")]),
            ""
        );
    }
}
