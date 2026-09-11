//! `class_methods do`: the `def`s in it, and the module `ActiveSupport::Concern` builds for them.
//!
//! `ActiveSupport::Concern#class_methods` is four lines of Ruby and every one of them matters
//! here:
//!
//! ```ruby
//! def class_methods(&class_methods_module_definition)
//!   mod = const_defined?(:ClassMethods, false) ? const_get(:ClassMethods)
//!                                              : const_set(:ClassMethods, Module.new)
//!   mod.module_eval(&class_methods_module_definition)
//! end
//! ```
//!
//! It builds — or reopens — a nested `ClassMethods` module and evaluates the block on it, and
//! `append_features` then ends with `base.extend const_get(:ClassMethods)`. So a `def` written in
//! that block is a **class method of every class that includes the concern**, by exactly the same
//! edge a hand-written `module ClassMethods` reaches — and by the same edge an `included do` holding
//! a bare `extend M` reaches, with no such module written anywhere.
//!
//! **Three spellings, one destination, and no file writes the line that gets them there.** rubydex
//! is right to find nothing: `base.extend` runs at load time, and there is no `extend` in anybody's
//! source to record. What this file does is spend that edge — writing each `def` onto the singleton
//! of every class that includes the concern, so an ordinary ancestor walk finds it and nothing
//! outside this directory knows the convention.
//!
//! # Why the block is still not a macro host
//!
//! [`super::models::HOSTS`] declines `class_methods do` and stays right: the block is evaluated on
//! a plain `Module`, which has no `has_many`, so a macro written there is broken Ruby rather than a
//! declaration this directory was missing. The `def`s in it were never the question that test
//! asked. Over six corpora the block holds **no macro at all** and **270 `def`s**.
//!
//! # The spelling, and why it is a fan-out
//!
//! One `def self.` per including class, which is the shape a class-side [`scope`](super::relations)
//! already has. The obvious alternative — one `module <concern>::ClassMethods`, `extend`ed onto the
//! includers by a line this generator writes — is not available, and the reason is measured rather
//! than stylistic: **a mixin that arrives in a document indexed after its class was resolved is
//! never linearized onto that class**, and a generated document is always that shape. The
//! instance-side `include` the route helpers use is the one exception; every singleton-side
//! spelling fails, in RBS and in Ruby alike — `extend M`, `class << self; include M; end` and
//! `singleton_class.include M` all leave the member unreachable, and re-indexing the class's own
//! file afterwards does not repair it. What does reach a class object is a member written straight
//! onto it, so that is what is written.
//!
//! The cost of the fan-out is the includers this pass cannot enumerate — but it can enumerate
//! them: [`Context::includers`](crate::analysis::synthesize) resolves the `include` edges of the
//! whole project after the walk, transitively through concerns that include concerns, and hands
//! the classes at the end of those chains to every reader in this directory.

use std::collections::{BTreeMap, BTreeSet};

use ruby_prism::{CallNode, StatementsNode};

use super::syntax::{constant_spelling, def_header, parameters_of, spellable, symbol_or_string};
use crate::generated::{At, Declared, Facts, Namespaces, Owner, Source};

/// One `def` written as a statement of a `class_methods do` block.
#[derive(Debug)]
pub struct ClassMethod {
    name: String,
    /// Which of the two spellings wrote it, for the provenance sentence alone: a reader sent to
    /// the `def` should be told whether the file says `class_methods do` or `module ClassMethods`,
    /// because those are the two lines they will be looking at.
    spelled: &'static str,
    /// The RBS parameter list the `def`'s own parameters imply, every type `untyped`.
    parameters: String,
    /// The `def` keyword through the end of the parameter list, and the name inside it — so the
    /// declaration this renders is mapped back to the line a user actually wrote.
    at: (u32, u32),
    name_at: (u32, u32),
}

/// The block's statements, when this call is the one that opens one.
///
/// A `class_methods` written **in a class** is declined and is not an oversight: `class_methods`
/// is defined on `ActiveSupport::Concern`, which is extended onto modules, so the call raises
/// `NoMethodError` in a class body. Six corpora write it in a class **zero** times.
///
/// Whether the call has a **receiver** is not asked here, and deliberately: `models::bare` has
/// already asked it of every call it hands over, and it is the one that knows the answer — a
/// receiver naming the `with_options` merger the enclosing block was handed is still a call the
/// body itself is making, and this function cannot see that block. Asking twice would put an arm
/// here that the caller makes unreachable.
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

/// The `def`s of a `module ClassMethods` written out by hand, as statements of a module body.
///
/// The minority spelling and the one the crate read first: six corpora hold **17** files with a
/// hand-written `module ClassMethods` against **120** `class_methods do` calls. Both build the
/// same module — `ActiveSupport::Concern#class_methods` calls `const_get(:ClassMethods)` when one
/// already exists — so both feed one list and a concern that writes both gets one set of members.
///
/// **It is read from the source rather than taken off the graph**, although the graph really does
/// hold this module: a reader of the source declares the fact whether or not the edge was
/// recorded, and the two spellings then arrive by one road. The nested module is a real
/// declaration either way; what neither spelling gives anybody is the `extend` onto the includer,
/// which is what [`declare`] writes.
///
/// A **class** is declined, as [`body`] declines one and for the same sentence: a class cannot be
/// `include`d, so a `ClassMethods` nested in one reaches no singleton at all.
///
/// Only statements of the body, which is this directory's rule everywhere: a `module ClassMethods`
/// inside an `if` is Ruby that only runs.
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
        // The name exactly, and not its last segment: `module Foo::ClassMethods` written inside
        // another module is a module of `Foo`'s, which this concern does not extend onto anybody.
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
    /// The `extend Foo` statement and the constant inside it, so a reader can be sent to the
    /// line that installed the member when nothing better is available.
    pub at: At,
}

/// The modules an `included do` block `extend`s, which land on every including class.
///
/// `ActiveSupport::Concern` `class_eval`s the block on each includer, so a bare `extend M` in one
/// puts `M`'s **instance** methods on that class's singleton — the same destination
/// `class_methods do` reaches, by a spelling with no `ClassMethods` module in it at all.
/// `activemodel/lib/active_model/api.rb` is two such lines, `extend ActiveModel::Naming` and
/// `extend ActiveModel::Translation`, reached by every model in an application through
/// `ActiveRecord::Base`'s `include ActiveModel::API` — and they are what installs `model_name`
/// and `human_attribute_name`.
///
/// **What this returns is a name and never a member**, which is the difference between this half
/// and the two above and the reason it is finished elsewhere: the `def`s are in `M`'s own file,
/// which is not this one and which no list of this directory's would put in front of a reader.
/// `analysis::synthesize` asks the graph for them.
///
/// A **class** is declined for [`body`]'s reason: `included` is `ActiveSupport::Concern`'s and a
/// class is not something an `include` can name.
///
/// Only statements of the block, and only a bare `extend` — `Foo.extend M` is somebody else's
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
            // follow, and naming none is the answer.
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
/// module; this says what a class gains by it, and the two are in different files — which is why
/// this takes a name as well as a source, and why `analysis::synthesize` asks the graph which file
/// to hand over.
///
/// **The module's own body and never its ancestors.** `extend M` really does install the instance
/// methods of `M`'s ancestors, and following them is the right reading of Ruby and the wrong
/// reading of a project: Rails writes `include` statements inside a `def` in these modules, and a
/// walk over them made `Category.valid?` — which raises in Ruby — answer
/// `ActiveModel::Validations#valid?` and put six instance methods into a class object's
/// completion list. So this reads the statements of one body, as every reader here does.
///
/// Visibility, a `def self.` and an unspellable name are all [`read`]'s rules, unchanged: a
/// `private` `def` is not extended onto anybody and a singleton method of the module is not
/// installed by an `extend` at all.
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
/// [`super::models::Models::walk`]'s shape and for its reason: a generic visitor descends into
/// every method body in the file, which on a large one is thousands of frames on a 2 MiB stack.
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
        // A `class` body is descended into as well as a `module`'s, because the module being
        // looked for may be nested in one — `Random::Formatter` is the shape, and a walk that
        // only followed `module` would never reach it. What is *matched* is still a module: an
        // `extend` names one, and the name is the whole path rather than its last segment.
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

/// The nested module Rails builds, spelled the way Ruby spells it.
///
/// Public because [`Analysis::contribution`](crate::analysis) filters documents on it — a file
/// whose whole content is a hand-written `module ClassMethods` calls nothing and references
/// nothing, so the only thing that can put it in front of a reader is the name of the module
/// itself. It is exported for [`MODEL_CALLS`](super::MODEL_CALLS)' reason: the table that decides
/// which documents a generator sees lives outside this directory, so the words in it have to come
/// from inside it.
pub const CLASS_METHODS: &str = "ClassMethods";

/// The two spellings, as a provenance sentence names them.
pub(super) const BLOCK: &str = "class_methods do";
const MODULE: &str = "module ClassMethods";
const EXTENDED: &str = "included do … extend";

/// Every `def` this block installs on the includer's singleton, in source order.
///
/// Visibility is the file's own, read exactly as [`super::entrypoints`] reads a mailer's: a bare
/// `private` or `protected` closes the public section and `private :name` names one already
/// written. The corpora write **19** bare ones inside these blocks, so it is the common case
/// rather than a guard against a hypothetical.
///
/// Only statements of the block, which is the rule every reader in this directory keeps: a `def`
/// inside an `if` inside the block is Ruby that only runs.
///
/// A `def self.` is declined. It is a singleton method of the `ClassMethods` module itself, which
/// `extend` does not install on anything — the module is the thing being extended, not a thing
/// extending. Six corpora write **zero** of them.
///
/// `private` **is** asked for a receiver, where [`body`] is not, and for the opposite reason:
/// this walk is its own and nothing upstream has narrowed these statements. `something.private`
/// is a call to somebody else's method and closes nothing.
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
                at: def_header(&written),
                name_at: (at.start_offset() as u32, at.end_offset() as u32),
            });
        }
    }
    found.retain(|method| !hidden.contains(&method.name));
    found
}

/// Where a set of class methods came from, for the sentence above each declaration.
///
/// Three spellings reach one `declare`, and a reader sent to the `def` should be told which line
/// of which file installed it on the class they asked about.
pub struct From<'a> {
    /// The file the `def`s were read out of.
    pub file: &'a str,
    /// The module a class writes `include` for, which is what makes it an includer.
    pub concern: &'a str,
    /// The module an `included do … extend` names, where that is the shape. `None` for the two
    /// spellings whose `def`s are in the concern's own body.
    pub via: Option<&'a str>,
}

/// Declare each of them on the singleton of every class that includes the concern.
///
/// **The name Ruby gives the module is not the name this writes.**
/// `ActiveSupport::Concern#class_methods` builds `<concern>::ClassMethods` and `append_features`
/// ends with `base.extend const_get(:ClassMethods)`, so the *fact* is one module and one `extend`.
/// Writing that fact down does not state it: the `extend` would arrive in a generated document,
/// which is always indexed after the includer was resolved, and such a mixin is never linearized —
/// the module's doc has the whole measurement. So the edge is spent here instead, once per
/// includer, and what the includer holds afterwards is what Ruby would have put in front of it.
///
/// **The span is still the `def` a person typed**, however many classes this writes it onto, which
/// is the class-side `scope`'s rule and for its reason: one `def` several declarations name is
/// still one place.
///
/// `Source::Convention` for the reason a mailer's action is: the `def` is really in the file and
/// the class method is really installed, but the *type* is this table's and not the file's.
///
/// A concern **nothing includes** declares nothing, which is Ruby rather than caution: the module
/// itself never answers these names — `Tallyable.tally_by` raises — and where no class includes it
/// there is no class object that could.
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
        // The one decline this generator owes, and it is the joined-name rule rather than a
        // judgement about the includer: `Owner::Singleton` opens `class <includer>`, so a name
        // with a namespace above it that nothing declares would introduce that namespace — RBS
        // that does not parse, which `Synthesized::record` answers by refusing the whole
        // document, costing every other declaration the file makes.
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

    /// What one file declares, told which namespaces the project writes down and which class
    /// includes the concern — which is the whole of what this reader needs from elsewhere.
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
        // The second entry point, which is what a concern's class methods come out of — see
        // [`super::models::Model::class_methods`].
        facts.extend(model.class_methods(
            "app/models/concerns/tallyable.rb",
            &includers,
            &namespaces,
        ));
        facts.render(&namespaces).rbs
    }

    /// Pinned whole, for the reason the schema's and the delegate's are: every rule in this
    /// reader shows up in the text, and asserting them one predicate at a time is how a change to
    /// the shape passes five green tests.
    ///
    /// The owner is the **includer** and never the concern: `Ledger` is what Ruby puts these in
    /// front of, and `Tallyable` itself answers none of them.
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

    /// The other spelling, and it reaches the same list.
    ///
    /// A hand-written `module ClassMethods` is 17 files in six corpora against 120
    /// `class_methods do` calls, and `ActiveSupport::Concern#class_methods` reopens the module a
    /// file declares rather than building a second one — so a concern that writes both hands its
    /// includer one set of members.
    ///
    /// **The block speaks first and that is the order Ruby has**, not the file's: `module_eval`
    /// runs the block on the module a hand-written one already declared, so a name written both
    /// ways is the block's. `Facts` keeps the first of an equal-ranked pair, so stating the
    /// block's `def`s first is what makes that true.
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
    /// so nothing ever runs the `extend`. And a `module Foo::ClassMethods` is a module of `Foo`'s
    /// rather than this concern's, so the name is matched whole and not by its last segment.
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

    /// Every shape of `included do` that extends nothing, each a different sentence of Ruby.
    ///
    /// A statement that is not a call; a call that is not `extend`; an `extend` with a receiver or
    /// with no argument at all; and an argument that is not a constant — `Module.new { … }` names
    /// no module anything can follow. A block that is not written out reaches none of them, and a
    /// `class` is not something an `include` can name.
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
        // And the one that does, so the loop above is asserting an absence the reader can see.
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
    /// The far end of the third spelling, and the only reader in this directory that is handed a
    /// **name** as well as a source: the module is in a file the concern only mentions.
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
        // A `class` body is descended into as well as a `module`'s: `Random::Formatter` is a
        // module nested in a class, and a walk that followed only `module` would never reach it.
        assert_eq!(names("Holder::Nested"), vec!["held".to_owned()]);
        // The name is the whole path and never its last segment, and a name nothing declares
        // declares nothing.
        assert!(names("Naming").is_empty());
        assert!(names("Outer::Missing").is_empty());
    }

    /// `class_methods` is defined on `ActiveSupport::Concern`, which is extended onto **modules**,
    /// so the call raises `NoMethodError` in a class body. Six corpora write it in one zero times.
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

    /// The three shapes that are the name without the block, each of which would otherwise reach
    /// a `None` the reader has to answer for: no block at all, a block passed as an argument
    /// rather than written out, and one written out with nothing in it.
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

    /// A concern nothing includes declares nothing, which is Ruby rather than caution: the module
    /// itself never answers these names.
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

    /// The joined name has to introduce no namespace, and the name that could is the
    /// **includer's** — RBS that does not parse, which `Synthesized::record` answers by refusing
    /// the whole document.
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
