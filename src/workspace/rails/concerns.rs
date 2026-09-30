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

use super::syntax::{
    bodies_named, constant_spelling, def_span, parameters_of, spellable, symbol_or_string,
};
use crate::generated::{At, DEFINED, Declared, Facts, Namespaces, Owner, Source};

/// One method written as a statement of a `class_methods do` block: a `def`, or one side of an
/// `attr_*`.
#[derive(Debug)]
pub struct ClassMethod {
    pub(super) name: String,
    /// What the line says, for the provenance sentence: `def tally_by`, or `attr_accessor :limit`
    /// for both the reader and the writer it makes.
    pub(super) written: String,
    /// Which of the two spellings wrote it, for the provenance sentence: a reader sent to the `def`
    /// should be told whether the file says `class_methods do` or `module ClassMethods`, the two
    /// lines they will be looking at.
    spelled: &'static str,
    /// The RBS parameter list the `def`'s own parameters imply, every type `untyped`.
    pub(super) parameters: String,
    /// The `def` keyword through the end of the parameter list, and the name inside it, so the
    /// rendered declaration maps back to the line a user wrote.
    pub(super) at: (u32, u32),
    pub(super) name_at: (u32, u32),
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

/// Where each block `ActiveSupport::Concern` evaluates on the including class is passed: the start
/// of every `included do` and `prepended do` written as a statement of a module body.
///
/// `append_features` and `prepend_features` `class_eval` the stored block on each class that
/// includes the concern, so its `self` is a class some **other** file's `include` decides, and a
/// callback block inside it runs against that class's records. rubydex files the block's calls on
/// the module's class object, which Ruby never makes `self` there; the caller says so with
/// `Facts::runs` and no class, which the type side reads as a refusal.
///
/// A **class** is declined for [`extended`]'s reason, and only a bare call with a block counts.
pub(super) fn evaluated_elsewhere(statements: &StatementsNode<'_>, module: bool) -> Vec<u32> {
    if !module {
        return Vec::new();
    }
    statements
        .body()
        .iter()
        .filter_map(|statement| statement.as_call_node())
        .filter(|call| {
            call.receiver().is_none()
                && matches!(call.name().as_slice(), b"included" | b"prepended")
                && call
                    .block()
                    .is_some_and(|block| block.as_block_node().is_some())
        })
        .map(|call| call.location().start_offset() as u32)
        .collect()
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
    let mut bodies = Vec::new();
    bodies_named(
        source,
        parsed
            .node()
            .as_program_node()
            .map(|program| program.statements()),
        module_name,
        &mut Vec::new(),
        &mut bodies,
    );
    bodies
        .iter()
        .flat_map(|body| read(source, body, EXTENDED))
        .collect()
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

/// Every method this block installs on the includer's singleton, in source order.
///
/// **A `def`, and each side of an `attr_accessor`, `attr_reader` or `attr_writer`**: `module_eval`
/// runs those on the module like any other statement, so `ActiveRecord::Inheritance`'s
/// `attr_accessor :abstract_class` is what `self.abstract_class = true` calls. The name must be
/// one Ruby accepts for an attribute (no `?`, `!` or `=`); a name only running Ruby knows is
/// skipped alone.
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
                written: format!("def {name}"),
                name,
                spelled,
                at: def_span(&written),
                name_at: (at.start_offset() as u32, at.end_offset() as u32),
            });
        }
        if let Some(call) = statement.as_call_node()
            && visible
            && call.receiver().is_none()
            && let Some((reads, writes)) = accessor(call.name().as_slice())
            && let Some(arguments) = call.arguments()
        {
            let word = String::from_utf8_lossy(call.name().as_slice());
            let at = call.location();
            let at = (at.start_offset() as u32, at.end_offset() as u32);
            for (name, name_at) in arguments
                .arguments()
                .iter()
                .filter_map(|argument| symbol_or_string(source, &argument))
                .filter(|(name, _)| spellable(name) && !name.ends_with(['?', '!', '=']))
            {
                let written = format!("{word} :{name}");
                let side = |name: String, parameters: &str| ClassMethod {
                    name,
                    written: written.clone(),
                    spelled,
                    parameters: parameters.to_owned(),
                    at,
                    name_at,
                };
                if reads {
                    found.push(side(name.clone(), "()"));
                }
                if writes {
                    found.push(side(format!("{name}="), "(untyped)"));
                }
            }
        }
    }
    found.retain(|method| !hidden.contains(&method.name));
    found
}

/// Which sides an `attr_*` call makes, reader then writer: `None` for any other call.
fn accessor(called: &[u8]) -> Option<(bool, bool)> {
    match called {
        b"attr_accessor" => Some((true, true)),
        b"attr_reader" => Some((true, false)),
        b"attr_writer" => Some((false, true)),
        _ => None,
    }
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
/// method is really installed.
///
/// **Its type is the `def`'s own** ([`DEFINED`]): the types table reads that body with the
/// including class as `self`, which is what Ruby runs.
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
            // The module rubydex files the `def` under: the extended module, the hand-written
            // `ClassMethods`, or, for a `def` in the block, the concern itself.
            let holder = match (via, method.spelled) {
                (Some(module), _) => module.to_owned(),
                (None, MODULE) => format!("{concern}::{CLASS_METHODS}"),
                (None, _) => concern.to_owned(),
            };
            facts.declare(Declared {
                owner: Owner::Singleton(includer.clone()),
                name: method.name.clone(),
                returns: format!("{DEFINED}[::{holder}]"),
                // A Rails method whose block runs against something else says so
                // (`super::blocks`); every other member keeps what its `def` implies.
                parameters: super::blocks::class_method(concern, &method.name)
                    .unwrap_or_else(|| method.parameters.clone()),
                because: match via {
                    None => format!(
                        "From `{file}`, `{}` in `{}` in `{concern}`, which `{includer}` \
                         includes.",
                        method.written, method.spelled
                    ),
                    Some(module) => format!(
                        "From `{file}`, `{}` in `{module}`, which `{concern}`'s `{}` puts on \
                         every including class — here `{includer}`.",
                        method.written, method.spelled
                    ),
                },
                at: Some((method.at, method.name_at)),
                from: Source::Convention,
                overloads: Vec::new(),
                private: false,
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
AnsweredByItsDef[::Tallyable]
  # From `app/models/concerns/tallyable.rb`, `def primary_key=` in `class_methods do` in \
`Tallyable`, which `Ledger` includes.
  def self.primary_key=: (untyped value) -> AnsweredByItsDef[::Tallyable]
  # From `app/models/concerns/tallyable.rb`, `def still_public` in `class_methods do` in \
`Tallyable`, which `Ledger` includes.
  def self.still_public: () -> AnsweredByItsDef[::Tallyable]
end
"
        );
    }

    /// An `attr_*` in the block is two methods, or one, installed like a `def`.
    ///
    /// `ActiveRecord::Inheritance::ClassMethods` writes `attr_accessor :abstract_class`, which is
    /// what every `self.abstract_class = true` calls. Each side keeps the call's line as its place
    /// and the symbol as its name, and the privacy rules are the `def`'s.
    #[test]
    fn an_attr_in_the_block_installs_its_reader_and_its_writer() {
        assert_eq!(
            rbs(
                "\
module Tallyable
  extend ActiveSupport::Concern

  module ClassMethods
    attr_accessor :abstract_class, 'tally_limit'
    attr_reader :counted
    attr_writer :scale
    attr_reader :ready?, :'not a name'
    attr_accessor
    self.attr_reader :on_the_module
    attr_reader :hidden
    private :hidden

    private

    attr_accessor :after_private
  end
end
",
                &["Tallyable", "Ledger"],
                INCLUDED,
            ),
            "\
class Ledger
  # From `app/models/concerns/tallyable.rb`, `attr_accessor :abstract_class` in `module \
ClassMethods` in `Tallyable`, which `Ledger` includes.
  def self.abstract_class: () -> AnsweredByItsDef[::Tallyable::ClassMethods]
  # From `app/models/concerns/tallyable.rb`, `attr_accessor :abstract_class` in `module \
ClassMethods` in `Tallyable`, which `Ledger` includes.
  def self.abstract_class=: (untyped value) -> AnsweredByItsDef[::Tallyable::ClassMethods]
  # From `app/models/concerns/tallyable.rb`, `attr_accessor :tally_limit` in `module \
ClassMethods` in `Tallyable`, which `Ledger` includes.
  def self.tally_limit: () -> AnsweredByItsDef[::Tallyable::ClassMethods]
  # From `app/models/concerns/tallyable.rb`, `attr_accessor :tally_limit` in `module \
ClassMethods` in `Tallyable`, which `Ledger` includes.
  def self.tally_limit=: (untyped value) -> AnsweredByItsDef[::Tallyable::ClassMethods]
  # From `app/models/concerns/tallyable.rb`, `attr_reader :counted` in `module ClassMethods` in \
`Tallyable`, which `Ledger` includes.
  def self.counted: () -> AnsweredByItsDef[::Tallyable::ClassMethods]
  # From `app/models/concerns/tallyable.rb`, `attr_writer :scale` in `module ClassMethods` in \
`Tallyable`, which `Ledger` includes.
  def self.scale=: (untyped value) -> AnsweredByItsDef[::Tallyable::ClassMethods]
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
  def self.tally_by: (untyped) -> AnsweredByItsDef[::Tallyable]
  # From `app/models/concerns/tallyable.rb`, `def counted_name` in `module ClassMethods` in \
`Tallyable`, which `Ledger` includes.
  def self.counted_name: (untyped) -> AnsweredByItsDef[::Tallyable::ClassMethods]
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
    /// Only a bare `included do` or `prepended do` in a module body runs on the includers: not a
    /// call with no block or a stored one, not somebody else's method, not a class's body.
    #[test]
    fn a_block_the_concern_evaluates_on_its_includers_is_said_to_run_elsewhere() {
        let starts = |source: &str| {
            let parsed = ruby_prism::parse(source.as_bytes());
            let program = parsed.node();
            let first = program
                .as_program_node()
                .and_then(|program| program.statements().body().iter().next())
                .expect("one body");
            let (body, module) = match first.as_module_node() {
                Some(module) => (module.body(), true),
                None => (first.as_class_node().expect("a class").body(), false),
            };
            let statements = body
                .and_then(|body| body.as_statements_node())
                .expect("statements");
            super::evaluated_elsewhere(&statements, module)
        };
        let source = "\
module M
  included do
  end
  prepended do
  end
  included
  Other.included do
  end
  included(&:stored)
  validates :title do
  end
end
";
        let at = |needle: &str| source.find(needle).unwrap() as u32;
        assert_eq!(starts(source), vec![at("included do"), at("prepended do")]);
        assert!(starts("class M\n  included do\n  end\nend\n").is_empty());
    }

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
