//! Every RSpec word in the crate: what a spec file's example groups are, and the RBS they imply.
//!
//! Text in, facts out, as `workspace/rails/` is: no graph, no I/O. The orchestration (which files,
//! which names the bundle declares) is `knowledge::rspec`'s.
//!
//! # What RSpec does, and what is written for it
//!
//! - **`describe` makes a class** (`Class.new(parent)`, `RSpec::Core::ExampleGroup` at the top) and
//!   evaluates its block with `self` that class. Ruby leaves the class anonymous until RSpec names
//!   it by load order, so this module names it instead: one module per spec file under
//!   `RSpec::ExampleGroups`, one class per group inside it, nested as the groups are
//!   ([`base_name`] is RSpec's own spelling of a description). The block's `self` is said with
//!   [`Facts::runs`], because two `describe` calls make two classes and no signature can say so.
//! - **`it`, the hooks and `let`'s block run on an instance of that class**, which a signature can
//!   say: [`dsl`] writes `[self: instance]` on `RSpec::Core::ExampleGroup`'s class methods, read
//!   against whichever group's class object the call is made on.
//! - **`let(:user) { … }` is a method whose body is the block** ([`generated::BLOCK`]), and so is
//!   `subject`. RSpec defines it on the group, so a nested group inherits it and may redefine it.
//! - **`described_class` is the innermost group's first argument that is not a String**, and the
//!   implicit `subject` is `described_class.new` for a class, or the module itself.
//! - **A shared group's block runs in whichever group includes it**, so its `self` is refused, not
//!   read as the class around it.
//!
//! [`generated::BLOCK`]: crate::generated::BLOCK

use std::collections::{BTreeMap, BTreeSet};

use ruby_prism::{CallNode, ClassNode, DefNode, ModuleNode, Node, Visit, parse};

use crate::generated::{At, BLOCK, Declared, Facts, Namespaces, OWN_DEF, Owner, Runs, Source};

/// The class every example group descends from.
pub const EXAMPLE_GROUP: &str = "RSpec::Core::ExampleGroup";

/// The module RSpec files its groups' constants under, and this module files its own names under.
pub const EXAMPLE_GROUPS: &str = "RSpec::ExampleGroups";

/// What an example's block is handed, and what `it` returns.
const EXAMPLE: &str = "RSpec::Core::Example";

/// What an `around` hook's block is handed: the example, wrapped so it can be run.
const PROCSY: &str = "RSpec::Core::Example::Procsy";

/// What `RSpec.configure` hands its block.
const CONFIGURATION: &str = "RSpec::Core::Configuration";

/// The module whose class methods start a group from anywhere.
const RSPEC: &str = "RSpec";

/// What `expect(value)` makes.
const VALUE_TARGET: &str = "RSpec::Expectations::ValueExpectationTarget";

/// What `expect { … }` makes.
const BLOCK_TARGET: &str = "RSpec::Expectations::BlockExpectationTarget";

/// What `receive(:name)` makes, and what each customization of it hands back.
const RECEIVE: &str = "RSpec::Mocks::Matchers::Receive";

/// rspec-mocks' `expect` syntax, which `Syntax.enable_expect` writes into `ExampleMethods` with
/// `class_exec`, so no `def` is a member of it: `(name, parameters, the class its body makes)`,
/// each parameter named as the gem's `def` names it.
const MOCK_SYNTAX: [(&str, &str, &str); 6] = [
    (
        "receive",
        "(untyped method_name) ?{ (*untyped) -> untyped }",
        RECEIVE,
    ),
    (
        "receive_messages",
        "(untyped message_return_value_hash)",
        "RSpec::Mocks::Matchers::ReceiveMessages",
    ),
    (
        "receive_message_chain",
        "(*untyped messages) ?{ (*untyped) -> untyped }",
        "RSpec::Mocks::Matchers::ReceiveMessageChain",
    ),
    ("allow", "(untyped target)", "RSpec::Mocks::AllowanceTarget"),
    (
        "expect_any_instance_of",
        "(untyped klass)",
        "RSpec::Mocks::AnyInstanceExpectationTarget",
    ),
    (
        "allow_any_instance_of",
        "(untyped klass)",
        "RSpec::Mocks::AnyInstanceAllowanceTarget",
    ),
];

/// rspec-mocks' targets, whose `to`, `not_to` and `to_not` `delegate_to` writes with
/// `define_method`.
const MOCK_TARGETS: [&str; 4] = [
    "RSpec::Mocks::ExpectationTarget",
    "RSpec::Mocks::AllowanceTarget",
    "RSpec::Mocks::AnyInstanceExpectationTarget",
    "RSpec::Mocks::AnyInstanceAllowanceTarget",
];

/// `MessageExpectation`'s own public methods, each of which `Receive` records and answers itself
/// for (`MessageExpectation.public_instance_methods(false)`, each a `define_method` returning
/// `self`), as rspec-mocks 3.13 has them; `and_invoke` since 3.10.3. `to_s` too: it is one of
/// them, so `receive(:x).to_s` is the matcher, not a `String`.
const CUSTOMIZATIONS: [&str; 18] = [
    "and_return",
    "and_invoke",
    "and_call_original",
    "and_wrap_original",
    "and_raise",
    "and_throw",
    "and_yield",
    "exactly",
    "at_least",
    "at_most",
    "times",
    "never",
    "once",
    "twice",
    "thrice",
    "with",
    "ordered",
    "to_s",
];

/// The classes the DSL's own signatures are written on ([`dsl`]), whose files host them.
pub const HOSTS: [&str; 10] = [
    EXAMPLE_GROUP,
    RSPEC,
    CONFIGURATION,
    MATCHERS,
    MOCKS,
    RECEIVE,
    MOCK_TARGETS[0],
    MOCK_TARGETS[1],
    MOCK_TARGETS[2],
    MOCK_TARGETS[3],
];

/// Every constant this module names, for the pass to ask the bundle about.
pub const CONSTANTS: [&str; 23] = [
    RSPEC,
    "TestProf",
    LET_IT_BE_MODULE,
    EXAMPLE_GROUPS,
    "RSpec::Core",
    EXAMPLE_GROUP,
    EXAMPLE,
    PROCSY,
    CONFIGURATION,
    MATCHERS,
    "RSpec::Expectations",
    VALUE_TARGET,
    BLOCK_TARGET,
    "RSpec::Mocks",
    MOCKS,
    "RSpec::Mocks::Matchers",
    RECEIVE,
    "RSpec::Mocks::Matchers::ReceiveMessages",
    "RSpec::Mocks::Matchers::ReceiveMessageChain",
    MOCK_TARGETS[0],
    MOCK_TARGETS[1],
    MOCK_TARGETS[2],
    MOCK_TARGETS[3],
];

/// The calls that make a group: `define_example_group_method`'s seven, and the `feature` alias
/// rspec-rails and Capybara add for feature specs.
const GROUPS: [&str; 10] = [
    "describe",
    "context",
    "example_group",
    "xdescribe",
    "xcontext",
    "fdescribe",
    "fcontext",
    "feature",
    "xfeature",
    "ffeature",
];

/// The calls that define a shared group, whose block runs wherever it is included.
const SHARED: [&str; 3] = ["shared_examples", "shared_context", "shared_examples_for"];

/// The calls that nest a group around a shared one: `it_behaves_like "x"` is a group described
/// `"behaves like x"`, and its block customises it.
const BEHAVES: [&str; 2] = ["it_behaves_like", "it_should_behave_like"];

/// The calls that bring a shared group into the group they are written in.
const INCLUDES: [&str; 2] = ["include_context", "include_examples"];

/// The calls that define a memoized helper from their block.
const LETS: [&str; 2] = ["let", "let!"];

/// The same, for the example's subject.
const SUBJECTS: [&str; 2] = ["subject", "subject!"];

/// test-prof's `let_it_be` and its two shorthands: a `let` whose block runs once, before all the
/// group's examples, on an instance of the group, and whose value the accessor hands back.
const LET_IT_BE: [&str; 3] = [
    "let_it_be",
    "let_it_be_with_reload",
    "let_it_be_with_refind",
];

/// The modifiers test-prof ships, none of which changes what the value is: `reload` and `refind`
/// read the same record again, `freeze` freezes it.
const MODIFIERS: [&str; 3] = ["reload", "refind", "freeze"];

/// The module test-prof extends `RSpec::Core::ExampleGroup` with, by a call rubydex does not read.
const LET_IT_BE_MODULE: &str = "TestProf::LetItBe";

/// The calls that make an example: `define_example_method`'s, and `scenario` for feature specs.
const EXAMPLES: [&str; 15] = [
    "it",
    "specify",
    "example",
    "focus",
    "fexample",
    "fit",
    "fspecify",
    "xexample",
    "xit",
    "xspecify",
    "skip",
    "pending",
    "scenario",
    "xscenario",
    "fscenario",
];

/// The hooks whose block is handed the example, or the group instance for `:context`.
const HOOKS: [&str; 6] = [
    "before",
    "after",
    "prepend_before",
    "append_before",
    "prepend_after",
    "append_after",
];

/// One spec file's example groups, and the shared groups it defines.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Spec {
    /// Every group, parents before their children.
    pub groups: Vec<Group>,
    /// Every shared group, in the order written.
    pub shared: Vec<Shared>,
}

/// One `shared_examples`, `shared_context` or `shared_examples_for`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shared {
    /// The name `include_context` finds it by: a String's or Symbol's text, or a Module's spelling.
    pub name: Option<String>,
    /// Where the call that defines it starts.
    pub call: u32,
    /// The group it is written in: only that group and those inside it find it by name.
    pub parent: Option<usize>,
    /// Its `let`s and `subject`s, which every group including it takes.
    pub lets: Vec<Let>,
    /// The `def`s its block writes, which every group including it takes.
    pub defs: Vec<Def>,
}

/// A `def` written in a group's block, which Ruby makes a method of the group's class and rubydex
/// files elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Def {
    pub name: String,
    /// The whole `def`, and its name.
    pub at: At,
    /// `def self.name`, a method of the group's class object.
    pub singleton: bool,
}

/// One example group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// The group it is written in, by index into [`Spec::groups`].
    pub parent: Option<usize>,
    /// Its constant's last segment, unique among its siblings.
    pub segment: String,
    /// Where the call that makes it starts, where a block was written: the key its block's `self`
    /// is said under.
    pub call: Option<u32>,
    /// The `module`s and `class`es around it, joined: where a constant it names is looked up.
    pub nesting: String,
    /// What it says `described_class` is.
    pub described: Described,
    /// Its `let`s and `subject`s, in the order written.
    pub lets: Vec<Let>,
    /// The `def`s its block writes.
    pub defs: Vec<Def>,
    /// The shared group it is written in, by index into [`Spec::shared`], whose module it includes:
    /// Ruby makes it inside each group that includes that one, which this cannot name. A group
    /// written inside it has it as its parent, and the same shared group.
    pub shared: Option<usize>,
    /// The metadata its own arguments write (`type: :request`, `:js`); its parent's is inherited.
    pub metadata: Metadata,
    /// The shared groups it brings in by name (`include_context`, `include_examples`, and the one
    /// an `it_behaves_like` group is made for), with where each call starts.
    pub includes: Vec<(u32, String)>,
}

/// Metadata as RSpec's filters compare it: each key's value as `to_s` spells it (`true` for a bare
/// symbol), or `None` for a value only running Ruby knows.
pub type Metadata = BTreeMap<String, Option<String>>;

/// What a group's arguments say `described_class` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Described {
    /// The parent's: no argument, or a String.
    Inherited,
    /// A constant, as written.
    Constant(String),
    /// Something else Ruby will use (a Symbol, an expression), which nothing here can type.
    Unknown,
}

/// One `let`, `let!`, `subject` or `subject!`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Let {
    /// The method it defines.
    pub name: String,
    /// The call, and the name in it.
    pub at: At,
    /// Whether it is a named `subject`, which also answers `subject`.
    pub subject: bool,
    /// Whether a block was written, which is the body. Not for a `let_it_be` a modifier test-prof
    /// does not ship changes: the modifier decides what it hands back.
    pub block: bool,
    /// Whether it is a `let_it_be`, whose value a project's own modifier may change
    /// ([`Configured::modifiers`]).
    pub it_be: bool,
}

/// A constant a group names, where the caller could find it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Its full name.
    pub name: String,
    /// Whether it is a module, whose implicit subject is itself.
    pub module: bool,
}

/// Read one spec file.
#[must_use]
pub fn read_spec(source: &str) -> Spec {
    let result = parse(source.as_bytes());
    let mut reader = Reader {
        source,
        spec: Spec::default(),
        nesting: Vec::new(),
        group: None,
        shared: None,
        beneath: None,
        taken: BTreeMap::new(),
    };
    reader.visit(&result.node());
    reader.spec
}

/// [`read_spec`]'s walk.
struct Reader<'s> {
    source: &'s str,
    spec: Spec,
    /// The `module`s and `class`es around the walk, outside any group.
    nesting: Vec<String>,
    /// The group whose body the walk is in.
    group: Option<usize>,
    /// The shared group whose body the walk is in, by index into [`Spec::shared`].
    shared: Option<usize>,
    /// The shared group whose nested groups the walk is in ([`Group::shared`]).
    beneath: Option<usize>,
    /// The segments each parent's children have taken, by parent group and shared group.
    taken: BTreeMap<(Option<usize>, Option<usize>), BTreeSet<String>>,
}

impl<'pr> Visit<'pr> for Reader<'_> {
    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        let (path, body) = (node.constant_path(), node.body());
        self.namespace(&path, body.as_ref());
    }

    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        let (path, body) = (node.constant_path(), node.body());
        self.namespace(&path, body.as_ref());
    }

    /// A method's body is not the DSL; the `def` itself is the group's, or the shared group's.
    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        let singleton = match node.receiver() {
            None => false,
            Some(receiver) if receiver.as_self_node().is_some() => true,
            // `def other.name` is a method of another object.
            Some(_) => return,
        };
        let (whole, name) = (node.location(), node.name_loc());
        let written = Def {
            name: String::from_utf8_lossy(node.name().as_slice()).into_owned(),
            at: (
                (whole.start_offset() as u32, whole.end_offset() as u32),
                (name.start_offset() as u32, name.end_offset() as u32),
            ),
            singleton,
        };
        match (self.shared, self.group) {
            // A shared group's block runs in the group that includes it, where a `def self.`
            // is that group's own: nothing this module can hold.
            (Some(index), _) if !singleton => self.spec.shared[index].defs.push(written),
            (Some(_), _) => {}
            (None, Some(index)) => self.spec.groups[index].defs.push(written),
            (None, None) => {}
        }
    }

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        let name = String::from_utf8_lossy(node.name().as_slice()).into_owned();
        let receiver = node.receiver();
        let bare = receiver.is_none();
        let on_rspec = receiver
            .as_ref()
            .is_some_and(|receiver| self.is_rspec(receiver));
        let block = node.block().and_then(|block| block.as_block_node());
        let start = node.location().start_offset() as u32;
        let named = |list: &[&str]| list.contains(&name.as_str());
        let in_group = self.group.is_some();
        if self.shared.is_some() {
            // A shared group's body: its `let`s are its own, and the groups and examples it writes
            // are made in whichever group includes it, which this does not follow.
            if (named(&LETS) || named(&SUBJECTS) || named(&LET_IT_BE))
                && bare
                && let (Some(index), Some(written)) = (
                    self.shared,
                    self.memoized(node, named(&SUBJECTS), block.is_some()),
                )
            {
                self.spec.shared[index].lets.push(written);
            } else if bare && ((named(&GROUPS) && block.is_some()) || named(&BEHAVES)) {
                // A group it writes: one class for every group that includes it, which takes its
                // module and is walked as any group is.
                let outer = (self.group, self.shared, self.beneath);
                (self.group, self.shared, self.beneath) = (None, None, self.shared);
                self.nest(node, named(&BEHAVES), block, start);
                (self.group, self.shared, self.beneath) = outer;
            } else if !((named(&GROUPS)
                || named(&SHARED)
                || named(&EXAMPLES)
                || named(&HOOKS)
                || named(&INCLUDES)
                || name == "around")
                && bare)
            {
                ruby_prism::visit_call_node(self, node);
            }
            return;
        }
        if named(&GROUPS) && block.is_some() && (bare || (on_rspec && !in_group)) {
            self.nest(node, false, block, start);
        } else if named(&BEHAVES) && bare && in_group {
            self.nest(node, true, block, start);
        } else if named(&SHARED)
            && (bare || on_rspec)
            && let Some(block) = block
        {
            self.spec.shared.push(Shared {
                name: self.shared_name(node),
                call: start,
                parent: self.group,
                lets: Vec::new(),
                defs: Vec::new(),
            });
            self.shared = Some(self.spec.shared.len() - 1);
            if let Some(body) = block.body() {
                self.visit(&body);
            }
            self.shared = None;
        } else if named(&INCLUDES) && bare && in_group {
            if let (Some(index), Some(shared)) = (self.group, self.shared_name(node)) {
                self.spec.groups[index].includes.push((start, shared));
            }
        } else if (named(&LETS) || named(&SUBJECTS) || named(&LET_IT_BE)) && bare && in_group {
            if let (Some(index), Some(written)) = (
                self.group,
                self.memoized(node, named(&SUBJECTS), block.is_some()),
            ) {
                self.spec.groups[index].lets.push(written);
            }
        } else if !((named(&EXAMPLES) || named(&HOOKS) || name == "around") && bare && in_group) {
            // Anything else may hold a group in its block (`%w[a b].each do |x| context x do`), so
            // the walk goes on. An example's or a hook's body holds none.
            ruby_prism::visit_call_node(self, node);
        }
    }
}

impl Reader<'_> {
    /// A `module` or `class` body outside every group: a constant a group names is looked up in
    /// it. Inside a group, a `class` is a constant of the block's lexical scope, not a group.
    fn namespace(&mut self, path: &Node<'_>, body: Option<&Node<'_>>) {
        if self.group.is_some() {
            return;
        }
        self.nesting.push(self.spelling(path));
        if let Some(body) = body {
            self.visit(body);
        }
        self.nesting.pop();
    }

    /// A group written where the walk is, or the one `it_behaves_like` makes: minted, and its block
    /// walked.
    fn nest(
        &mut self,
        node: &CallNode<'_>,
        behaves: bool,
        block: Option<ruby_prism::BlockNode<'_>>,
        start: u32,
    ) {
        if behaves {
            let shared = self.shared_name(node);
            let index = self.mint(
                &format!("behaves like {}", shared.as_deref().unwrap_or_default()),
                block.as_ref().map(|_| start),
                Described::Inherited,
                Metadata::new(),
            );
            // The shared block runs first, then the customisation block, whose `let`s win.
            if let Some(shared) = shared {
                self.spec.groups[index].includes.push((start, shared));
            }
            self.within(index, block.and_then(|block| block.body()));
        } else {
            let (description, described) = self.description(node);
            let metadata = self.metadata(node);
            let index = self.mint(&description, Some(start), described, metadata);
            self.within(index, block.and_then(|block| block.body()));
        }
    }

    /// Walk a group's block with that group as the one being written.
    fn within(&mut self, index: usize, body: Option<Node<'_>>) {
        let outer = self.group.replace(index);
        if let Some(body) = body {
            self.visit(&body);
        }
        self.group = outer;
    }

    /// Record a group under the one being written, named as RSpec would, unique among its
    /// siblings.
    fn mint(
        &mut self,
        description: &str,
        call: Option<u32>,
        described: Described,
        metadata: Metadata,
    ) -> usize {
        let taken = self.taken.entry((self.group, self.beneath)).or_default();
        let base = base_name(description);
        let mut segment = base.clone();
        let mut next = 2;
        while taken.contains(&segment) {
            segment = format!("{base}_{next}");
            next += 1;
        }
        taken.insert(segment.clone());
        self.spec.groups.push(Group {
            parent: self.group,
            segment,
            call,
            nesting: self.nesting.join("::"),
            described,
            lets: Vec::new(),
            defs: Vec::new(),
            shared: self.beneath,
            metadata,
            includes: Vec::new(),
        });
        self.spec.groups.len() - 1
    }

    /// A `let`, `let!`, `subject` or `subject!`, where it names a method.
    fn memoized(&self, node: &CallNode<'_>, subject: bool, block: bool) -> Option<Let> {
        let first = node
            .arguments()
            .and_then(|arguments| arguments.arguments().iter().next());
        let location = node.location();
        let whole = (location.start_offset() as u32, location.end_offset() as u32);
        let (name, selection) = match first {
            Some(first) => self.named(&first)?,
            // An anonymous `subject` is `let(:subject)`; a `let` with no name raises.
            None if subject => {
                let message = node.message_loc().map_or(location, |message| message);
                (
                    "subject".to_owned(),
                    (message.start_offset() as u32, message.end_offset() as u32),
                )
            }
            None => return None,
        };
        let it_be = LET_IT_BE.contains(&String::from_utf8_lossy(node.name().as_slice()).as_ref());
        // A modifier test-prof does not ship, as an option: the project's own block decides.
        let modified = it_be
            && node.arguments().is_some_and(|arguments| {
                arguments.arguments().iter().skip(1).any(|argument| {
                    argument.as_keyword_hash_node().is_none_or(|hash| {
                        hash.elements().iter().any(|element| {
                            element
                                .as_assoc_node()
                                .and_then(|pair| pair.key().as_symbol_node())
                                .is_none_or(|key| {
                                    !MODIFIERS.contains(
                                        &String::from_utf8_lossy(key.unescaped()).as_ref(),
                                    )
                                })
                        })
                    })
                })
            });
        Some(Let {
            subject: subject && name != "subject",
            name,
            at: (whole, selection),
            block: block && !modified,
            it_be,
        })
    }

    /// The name a shared group is defined or included by: its first argument, a String's or a
    /// Symbol's text, or a Module's spelling.
    fn shared_name(&self, node: &CallNode<'_>) -> Option<String> {
        let first = node.arguments()?.arguments().iter().next()?;
        if is_constant(&first) {
            return Some(self.spelling(&first));
        }
        self.literal(&first)
    }

    /// A `let`'s name: a symbol or a plain string that spells a method.
    fn named(&self, node: &Node<'_>) -> Option<(String, (u32, u32))> {
        let (name, location) = if let Some(symbol) = node.as_symbol_node() {
            (
                String::from_utf8(symbol.unescaped().to_vec()).ok()?,
                symbol.value_loc()?,
            )
        } else {
            let string = node.as_string_node()?;
            (
                String::from_utf8(string.unescaped().to_vec()).ok()?,
                string.content_loc(),
            )
        };
        spellable(&name).then(|| {
            (
                name,
                (location.start_offset() as u32, location.end_offset() as u32),
            )
        })
    }

    /// What a group's arguments describe it as, and what they say `described_class` is.
    ///
    /// RSpec's `description_args`: the first argument, and a second where it is a String, joined
    /// with no space after a Module when the String starts `#`, `.` or `::` (`describe User,
    /// "#name"` is `User#name`). Only literals are read; anything else describes the group as
    /// `Group`, a name this module invents like every other.
    fn description(&self, node: &CallNode<'_>) -> (String, Described) {
        let positional: Vec<Node<'_>> = node
            .arguments()
            .map(|arguments| {
                arguments
                    .arguments()
                    .iter()
                    .filter(|argument| argument.as_keyword_hash_node().is_none())
                    .collect()
            })
            .unwrap_or_default();
        let Some(first) = positional.first() else {
            return (String::new(), Described::Inherited);
        };
        let (mut description, described, constant) = if is_constant(first) {
            let spelled = self.spelling(first);
            (spelled.clone(), Described::Constant(spelled), true)
        } else if first.as_string_node().is_some() || first.as_interpolated_string_node().is_some()
        {
            (
                self.literal(first).unwrap_or_else(|| "Group".to_owned()),
                Described::Inherited,
                false,
            )
        } else {
            (
                self.literal(first).unwrap_or_else(|| "Group".to_owned()),
                Described::Unknown,
                false,
            )
        };
        if let Some(second) = positional
            .get(1)
            .filter(|second| second.as_string_node().is_some())
            .and_then(|second| self.literal(second))
        {
            let joined = constant
                && (second.starts_with('#') || second.starts_with('.') || second.starts_with("::"));
            if !joined {
                description.push(' ');
            }
            description.push_str(&second);
        }
        (description, described)
    }

    /// The metadata a group's arguments write: every `key: value` of a keyword hash, and every
    /// Symbol after the first argument (`describe User, :js` is `js: true`).
    fn metadata(&self, node: &CallNode<'_>) -> Metadata {
        let mut metadata = Metadata::new();
        let Some(arguments) = node.arguments() else {
            return metadata;
        };
        for (at, argument) in arguments.arguments().iter().enumerate() {
            if let Some(hash) = argument.as_keyword_hash_node() {
                metadata.extend(pairs(&hash.elements().iter().collect::<Vec<_>>()));
            } else if at > 0
                && let Some(symbol) = argument.as_symbol_node()
            {
                metadata.insert(
                    String::from_utf8_lossy(symbol.unescaped()).into_owned(),
                    Some("true".to_owned()),
                );
            }
        }
        metadata
    }

    /// A plain string's or a symbol's text.
    fn literal(&self, node: &Node<'_>) -> Option<String> {
        let bytes = if let Some(string) = node.as_string_node() {
            string.unescaped().to_vec()
        } else {
            node.as_symbol_node()?.unescaped().to_vec()
        };
        String::from_utf8(bytes).ok()
    }

    /// A constant path as written, a leading `::` dropped.
    fn spelling(&self, node: &Node<'_>) -> String {
        let location = node.location();
        self.source
            .get(location.start_offset()..location.end_offset())
            .unwrap_or_default()
            .trim_start_matches("::")
            .to_owned()
    }

    /// Whether a receiver is the `RSpec` module, written either way.
    fn is_rspec(&self, node: &Node<'_>) -> bool {
        is_constant(node) && self.spelling(node) == RSPEC
    }
}

/// The `key: value` pairs of a hash, as [`Metadata`]: a Symbol key only, and a value read as its
/// `to_s` where it is a literal.
fn pairs(elements: &[Node<'_>]) -> Metadata {
    let mut metadata = Metadata::new();
    for element in elements {
        let Some(pair) = element.as_assoc_node() else {
            continue;
        };
        let Some(key) = pair.key().as_symbol_node() else {
            continue;
        };
        metadata.insert(
            String::from_utf8_lossy(key.unescaped()).into_owned(),
            spelled(&pair.value()),
        );
    }
    metadata
}

/// A metadata value as `to_s` spells it, where it is a literal.
fn spelled(value: &Node<'_>) -> Option<String> {
    if let Some(symbol) = value.as_symbol_node() {
        return Some(String::from_utf8_lossy(symbol.unescaped()).into_owned());
    }
    if let Some(string) = value.as_string_node() {
        return Some(String::from_utf8_lossy(string.unescaped()).into_owned());
    }
    if value.as_true_node().is_some() {
        return Some("true".to_owned());
    }
    if value.as_false_node().is_some() {
        return Some("false".to_owned());
    }
    value.as_nil_node().map(|_| "nil".to_owned())
}

/// Whether a node is a constant, bare or a path.
fn is_constant(node: &Node<'_>) -> bool {
    node.as_constant_read_node().is_some() || node.as_constant_path_node().is_some()
}

/// Whether a name can be written as an RBS method name.
fn spellable(name: &str) -> bool {
    let mut characters = name.strip_suffix(['?', '!']).unwrap_or(name).chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|rest| rest.is_ascii_alphanumeric() || rest == '_')
}

/// RSpec's own constant for a description (`ExampleGroups.base_name_for`): every run of
/// characters that are not ASCII letters or digits dropped and the next letter capitalised,
/// `Nested` in front of one that does not start with a capital, `Anonymous` for nothing.
#[must_use]
pub fn base_name(description: &str) -> String {
    let mut name = String::new();
    let mut capital = true;
    for character in description.chars() {
        if character.is_ascii_alphanumeric() {
            name.push(if capital {
                character.to_ascii_uppercase()
            } else {
                character
            });
            capital = false;
        } else {
            capital = true;
        }
    }
    if name.is_empty() {
        return "Anonymous".to_owned();
    }
    if !name.starts_with(|first: char| first.is_ascii_uppercase()) {
        name.insert_str(0, "Nested");
    }
    name
}

/// The module a spec file's groups are filed under: its path without `.rb`, spelled as a constant.
#[must_use]
pub fn file_module(caption: &str) -> String {
    let path = caption.strip_suffix(".rb").unwrap_or(caption);
    format!("{EXAMPLE_GROUPS}::{}", base_name(path))
}

/// The RBS one spec file implies.
///
/// - Each group a class under `file`, inheriting its parent's, and the `self` of its block.
/// - Its `let`s and `subject`s, each returning its block ([`BLOCK`]); the last of one name in a
///   group is the one Ruby keeps.
/// - `described_class` on the class side of every group, its own where its arguments say and the
///   one it inherits otherwise, and on the instance side where its own arguments say (a nested group
///   inherits that). **Every group has a class-side member because of this**, which is also what
///   makes rubydex hold the class object its block's `self` is: a class with nothing on its class
///   side has none. The instance side is not left to rspec-core's `self.class.described_class`:
///   that body is read from rspec-core's file, and a member a spec file implies is fenced from
///   there.
/// - The implicit `subject` where a group says what it describes and no `subject` is written in it
///   or above.
/// - The shared groups' blocks, refused.
///
/// `resolve` finds the constant a group names, looked up from the nesting around it.
#[must_use]
pub fn spec_facts(
    spec: &Spec,
    file: &str,
    caption: &str,
    setup: &Setup,
    resolve: &dyn Fn(&str, &str) -> Option<Resolved>,
) -> Facts {
    let mut facts = Facts::default();
    facts.whole();
    facts.namespace(Owner::Module(file.to_owned()), None);
    let modules = shared_facts(&mut facts, spec, file, caption, setup.modifiers);
    let mut names: Vec<String> = Vec::with_capacity(spec.groups.len());
    let mut explicit: Vec<bool> = Vec::with_capacity(spec.groups.len());
    // What each group's `described_class` is, own or inherited: `None` where nothing above says.
    let mut effective: Vec<Option<Resolved>> = Vec::with_capacity(spec.groups.len());
    // Each group's metadata, its own over its parent's.
    let mut tagged: Vec<Metadata> = Vec::with_capacity(spec.groups.len());
    for (index, group) in spec.groups.iter().enumerate() {
        let (parent, above, inherited) = match (group.parent, group.shared) {
            (Some(parent), _) => (
                names[parent].clone(),
                explicit[parent],
                effective[parent].clone(),
            ),
            // Written in a shared group: filed under its module, and what the including group
            // describes is not known here.
            (None, Some(shared)) => (modules[shared].clone(), false, None),
            (None, None) => (file.to_owned(), false, None),
        };
        let name = format!("{parent}::{}", group.segment);
        let owner = Owner::Instance(name.clone());
        let superclass = match group.parent {
            Some(_) => format!("::{parent}"),
            None => format!("::{EXAMPLE_GROUP}"),
        };
        facts.inherits(owner.clone(), superclass);
        let outer = group.parent.map(|parent| &tagged[parent]);
        let mut metadata = outer.cloned().unwrap_or_default();
        metadata.extend(group.metadata.clone());
        if group.parent.is_none() {
            // The including group's type is its own directory's, not necessarily this file's.
            if setup.infers_types
                && group.shared.is_none()
                && !metadata.contains_key("type")
                && let Some(kind) = inferred(caption)
            {
                metadata.insert("type".to_owned(), Some(kind.to_owned()));
            }
            for module in &setup.extended {
                facts.extension(owner.clone(), format!("::{module}"));
            }
            if let Some(shared) = group.shared {
                facts.mixin(owner.clone(), format!("::{}", modules[shared]));
            }
        }
        // A module a parent took is already this group's: RSpec includes it once.
        for (extend, module, filter) in &setup.filtered {
            if matches(filter, &metadata) && !outer.is_some_and(|outer| matches(filter, outer)) {
                if *extend {
                    facts.extension(owner.clone(), format!("::{module}"));
                } else {
                    facts.mixin(owner.clone(), format!("::{module}"));
                }
            }
        }
        if let Some(call) = group.call {
            facts.runs(owner.clone(), call, Runs::Made(name.clone()));
        }
        // The shared groups it brings in, found as RSpec finds them: written in this group or one
        // around it, innermost first, then at this file's top, then in a support file.
        let brought: Vec<(u32, &Shared, &str)> = group
            .includes
            .iter()
            .filter_map(|(at, wanted)| {
                let (shared, module) =
                    find_shared(spec, &modules, index, wanted).or_else(|| {
                        setup
                            .shared
                            .get(wanted)
                            .map(|(shared, module)| (shared, module.as_str()))
                    })?;
                Some((*at, shared, module))
            })
            .collect();
        for (_, _, module) in &brought {
            facts.mixin(owner.clone(), format!("::{module}"));
        }
        let written = group
            .lets
            .iter()
            .chain(brought.iter().flat_map(|(_, shared, _)| &shared.lets))
            .any(|written| written.name == "subject" || written.subject);
        let described = match &group.described {
            Described::Inherited => None,
            Described::Constant(constant) => Some(resolve(&group.nesting, constant)),
            Described::Unknown => Some(None),
        };
        let described_class = match &described {
            Some(own) => own.clone(),
            None => inherited,
        };
        described_row(
            &mut facts,
            &name,
            described_class.as_ref(),
            described.is_some().then(|| {
                format!(
                    "From `{caption}`: the innermost group's first argument that is not a String."
                )
            }),
        );
        if let Some(described) = &described
            && !above
            && !written
        {
            facts.declare(member(
                &owner,
                "subject",
                described.as_ref().map_or_else(
                    || "untyped".to_owned(),
                    |resolved| {
                        if resolved.module {
                            format!("singleton(::{})", resolved.name)
                        } else {
                            format!("::{}", resolved.name)
                        }
                    },
                ),
                format!(
                    "From `{caption}`: the implicit subject, `described_class.new` for a \
                         class and the module itself for a module."
                ),
                None,
            ));
        }
        // The last of one name is the method Ruby keeps: a group's own `let` written before an
        // `include_context` defining the same name is redefined by it.
        let mut kept = kept(&group.lets);
        kept.retain(|name, written| {
            !brought.iter().any(|(at, shared, _)| {
                *at > written.at.0.0 && kept_names(&shared.lets).contains(name)
            })
        });
        // A `def` in the block is the class's own method, which outranks a `let`'s (`let` defines
        // its method in a module the class includes): declared first, it keeps the name
        // ([`Facts::declare`]'s first speaker).
        for def in &group.defs {
            let on = if def.singleton {
                Owner::Singleton(name.clone())
            } else {
                owner.clone()
            };
            facts.declare(member(
                &on,
                &def.name,
                OWN_DEF.to_owned(),
                format!("From `{caption}`: a method of the group's class."),
                Some(def.at),
            ));
        }
        for (name, written) in kept {
            facts.declare(member(
                &owner,
                name,
                returns(written, setup.modifiers),
                format!("From `{caption}`."),
                Some(written.at),
            ));
        }
        names.push(name);
        explicit.push(above || written);
        effective.push(described_class);
        tagged.push(metadata);
    }
    facts
}

/// The RBS a support file's top-level shared groups imply: each one a module of its `let`s under
/// `file`, the way a spec file's are. Answers them with their modules, for the registry
/// [`setup`] takes.
#[must_use]
pub fn support_facts(
    spec: &Spec,
    file: &str,
    caption: &str,
    modifiers: bool,
) -> (Facts, Vec<(Shared, String)>) {
    let top = Spec {
        groups: Vec::new(),
        shared: spec
            .shared
            .iter()
            .filter(|shared| shared.parent.is_none())
            .cloned()
            .collect(),
    };
    let mut facts = Facts::default();
    facts.whole();
    facts.namespace(Owner::Module(file.to_owned()), None);
    let modules = shared_facts(&mut facts, &top, file, caption, modifiers);
    (facts, top.shared.into_iter().zip(modules).collect())
}

/// The last of each name a list of `let`s writes, which is the method Ruby keeps; a named
/// `subject` answers `subject` too.
/// What a `let` returns: its block ([`BLOCK`]), unless none was written, or it is a `let_it_be`
/// a project's own modifier may change.
fn returns(written: &Let, modifiers: bool) -> String {
    if written.block && !(written.it_be && modifiers) {
        BLOCK.to_owned()
    } else {
        "untyped".to_owned()
    }
}

fn kept(lets: &[Let]) -> BTreeMap<&str, &Let> {
    let mut kept: BTreeMap<&str, &Let> = BTreeMap::new();
    for written in lets {
        kept.insert(&written.name, written);
        if written.subject {
            kept.insert("subject", written);
        }
    }
    kept
}

/// The names [`kept`] keeps.
fn kept_names(lets: &[Let]) -> BTreeSet<&str> {
    kept(lets).into_keys().collect()
}

/// Each shared group a module of its `let`s, named after it under the file's module, and its
/// block's `self` said: `RSpec::Core::ExampleGroup`, the class every group including it descends
/// from. What only the including group defines is not on it, and a guess answers that, never a
/// wrong type. Answers the module of each, by index.
fn shared_facts(
    facts: &mut Facts,
    spec: &Spec,
    file: &str,
    caption: &str,
    modifiers: bool,
) -> Vec<String> {
    let mut taken: BTreeSet<String> = BTreeSet::new();
    let mut modules = Vec::with_capacity(spec.shared.len());
    for shared in &spec.shared {
        let base = format!(
            "Shared{}",
            base_name(shared.name.as_deref().unwrap_or_default())
        );
        let mut segment = base.clone();
        let mut next = 2;
        while !taken.insert(segment.clone()) {
            segment = format!("{base}_{next}");
            next += 1;
        }
        let module = format!("{file}::{segment}");
        let owner = Owner::Module(module.clone());
        facts.namespace(owner.clone(), None);
        facts.runs(
            owner.clone(),
            shared.call,
            Runs::Made(EXAMPLE_GROUP.to_owned()),
        );
        // `def`s first, which keep a name a `let` also writes, as a group's do.
        for def in &shared.defs {
            facts.declare(member(
                &owner,
                &def.name,
                OWN_DEF.to_owned(),
                format!("From `{caption}`: a method of the groups that include it."),
                Some(def.at),
            ));
        }
        for (name, written) in kept(&shared.lets) {
            facts.declare(member(
                &owner,
                name,
                returns(written, modifiers),
                format!("From `{caption}`."),
                Some(written.at),
            ));
        }
        modules.push(module);
    }
    modules
}

/// The shared group `wanted` names as a group at `index` finds it in this file: written in it or a
/// group around it, innermost first and the last of one name, then at the top of the file. With
/// its module.
fn find_shared<'s>(
    spec: &'s Spec,
    modules: &'s [String],
    index: usize,
    wanted: &str,
) -> Option<(&'s Shared, &'s str)> {
    let mut scope = Some(index);
    loop {
        let found =
            spec.shared.iter().enumerate().rev().find(|(_, shared)| {
                shared.parent == scope && shared.name.as_deref() == Some(wanted)
            });
        if let Some((at, shared)) = found {
            return Some((shared, &modules[at]));
        }
        scope = spec.groups[scope?].parent;
    }
}

/// `described_class` on a group's class side, and on its instance side where `because` says its own
/// arguments decided it (an inherited one is the parent's word, already said there): `untyped`
/// where nothing names a class (a String describes nothing, and `described_class` is `nil` there).
fn described_row(
    facts: &mut Facts,
    group: &str,
    resolved: Option<&Resolved>,
    because: Option<String>,
) {
    let returns = resolved.map_or_else(
        || "untyped".to_owned(),
        |resolved| format!("singleton(::{})", resolved.name),
    );
    let mut owners = vec![Owner::Singleton(group.to_owned())];
    if because.is_some() {
        owners.push(Owner::Instance(group.to_owned()));
    }
    for owner in owners {
        facts.declare(member(
            &owner,
            "described_class",
            returns.clone(),
            because.clone().unwrap_or_default(),
            None,
        ));
    }
}

/// One member with no parameters.
fn member(owner: &Owner, name: &str, returns: String, because: String, at: Option<At>) -> Declared {
    Declared {
        owner: owner.clone(),
        name: name.to_owned(),
        returns,
        parameters: "()".to_owned(),
        because,
        at,
        from: Source::Convention,
        overloads: Vec::new(),
        private: false,
    }
}

/// What a project's `RSpec.configure` blocks say that decides a type.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Configured {
    /// Every `config.include` and `config.extend`, in the order written.
    pub mixins: Vec<Mixin>,
    /// Whether `infer_spec_type_from_file_location!` is called: rspec-rails' `type:` by directory.
    pub infers_types: bool,
    /// Whether `mock_with` names a framework other than RSpec's own, whose methods then are not
    /// an example's.
    pub mocks_elsewhere: bool,
    /// The same, for `expect_with`.
    pub expects_elsewhere: bool,
    /// Whether a file naming `TestProf` registers a `let_it_be` modifier of its own, which may
    /// change what every `let_it_be` hands back (as a default, or by metadata).
    pub modifiers: bool,
}

/// One `config.include` or `config.extend`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mixin {
    /// `extend`, whose module the group's class object takes, rather than `include`.
    pub extend: bool,
    /// `include_context`, whose name is a shared group's rather than a module's.
    pub shared: bool,
    /// The module as written, or the shared group's name.
    pub module: String,
    /// The `module`s and `class`es around the call, joined: where the constant is looked up.
    pub nesting: String,
    /// What a group must match one key of to take it, empty for every group.
    pub filter: Metadata,
}

/// Read every `RSpec.configure` block in one file.
///
/// The block's own parameter is the configuration (`|config|`, `|c|`, `|rspec|`), and only calls on
/// it count: a nested `expect_with :rspec do |expectations|` configures something else.
#[must_use]
pub fn read_configured(source: &str) -> Configured {
    let result = parse(source.as_bytes());
    let mut reader = ConfigReader {
        source,
        nesting: Vec::new(),
        config: None,
        configured: Configured::default(),
    };
    reader.visit(&result.node());
    reader.configured
}

/// [`read_configured`]'s walk.
struct ConfigReader<'s> {
    source: &'s str,
    nesting: Vec<String>,
    /// The name the `configure` block being walked calls its configuration.
    config: Option<String>,
    configured: Configured,
}

impl<'pr> Visit<'pr> for ConfigReader<'_> {
    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        self.nested(&node.constant_path(), node.body().as_ref());
    }

    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        self.nested(&node.constant_path(), node.body().as_ref());
    }

    /// A method's body runs when somebody calls it, which a configuration file's rarely does.
    fn visit_def_node(&mut self, _node: &DefNode<'pr>) {}

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        let name = String::from_utf8_lossy(node.name().as_slice()).into_owned();
        // test-prof's own configuration, on whatever it is called: `TestProf::LetItBe.configure`
        // hands its block a `config`, and `TestProf::LetItBe.config` is the same object.
        if name == "register_modifier" && self.source.contains("TestProf") {
            self.configured.modifiers = true;
        }
        let receiver = node.receiver();
        let on_rspec = receiver
            .as_ref()
            .is_some_and(|receiver| is_constant(receiver) && self.spelling(receiver) == RSPEC);
        if name == "configure"
            && on_rspec
            && let Some(block) = node.block().and_then(|block| block.as_block_node())
            && let Some(parameter) = first_parameter(&block)
        {
            let outer = self.config.replace(parameter);
            if let Some(body) = block.body() {
                self.visit(&body);
            }
            self.config = outer;
            return;
        }
        let on_config = receiver
            .as_ref()
            .and_then(|receiver| receiver.as_local_variable_read_node())
            .is_some_and(|local| {
                self.config.as_deref() == Some(&*String::from_utf8_lossy(local.name().as_slice()))
            });
        if !on_config {
            ruby_prism::visit_call_node(self, node);
            return;
        }
        let arguments: Vec<Node<'_>> = node
            .arguments()
            .map(|arguments| arguments.arguments().iter().collect())
            .unwrap_or_default();
        match name.as_str() {
            "include" | "extend" | "include_context" => {
                let shared = name == "include_context";
                let Some(first) = arguments.first() else {
                    return;
                };
                let module = if is_constant(first) {
                    self.spelling(first)
                } else if let Some(text) = shared.then(|| literal_of(first)).flatten() {
                    text
                } else {
                    return;
                };
                let mut filter = Metadata::new();
                for argument in &arguments[1..] {
                    if let Some(hash) = argument.as_keyword_hash_node() {
                        filter.extend(pairs(&hash.elements().iter().collect::<Vec<_>>()));
                    } else if let Some(symbol) = argument.as_symbol_node() {
                        filter.insert(
                            String::from_utf8_lossy(symbol.unescaped()).into_owned(),
                            Some("true".to_owned()),
                        );
                    } else {
                        // Something only running Ruby can match: no group is known to take it.
                        filter.insert(String::new(), None);
                    }
                }
                self.configured.mixins.push(Mixin {
                    extend: name == "extend",
                    shared,
                    module,
                    nesting: self.nesting.join("::"),
                    filter,
                });
            }
            "infer_spec_type_from_file_location!" => self.configured.infers_types = true,
            "mock_with" | "expect_with" => {
                let own = arguments
                    .first()
                    .and_then(|first| first.as_symbol_node())
                    .is_some_and(|symbol| symbol.unescaped() == b"rspec");
                if !own {
                    if name == "mock_with" {
                        self.configured.mocks_elsewhere = true;
                    } else {
                        self.configured.expects_elsewhere = true;
                    }
                }
            }
            _ => {}
        }
    }
}

impl ConfigReader<'_> {
    fn nested(&mut self, path: &Node<'_>, body: Option<&Node<'_>>) {
        self.nesting.push(self.spelling(path));
        if let Some(body) = body {
            self.visit(body);
        }
        self.nesting.pop();
    }

    fn spelling(&self, node: &Node<'_>) -> String {
        let location = node.location();
        self.source
            .get(location.start_offset()..location.end_offset())
            .unwrap_or_default()
            .trim_start_matches("::")
            .to_owned()
    }
}

/// A plain string's or a symbol's text.
fn literal_of(node: &Node<'_>) -> Option<String> {
    let bytes = if let Some(string) = node.as_string_node() {
        string.unescaped().to_vec()
    } else {
        node.as_symbol_node()?.unescaped().to_vec()
    };
    String::from_utf8(bytes).ok()
}

/// A block's first required parameter's name, where it writes one.
fn first_parameter(block: &ruby_prism::BlockNode<'_>) -> Option<String> {
    let parameters = block
        .parameters()?
        .as_block_parameters_node()?
        .parameters()?;
    let first = parameters.requireds().iter().next()?;
    Some(
        String::from_utf8_lossy(first.as_required_parameter_node()?.name().as_slice()).into_owned(),
    )
}

/// What rspec-core and rspec-rails include in example groups without a line of the project's:
/// `(module, the \`type:\` values that take it, none for every group)`.
///
/// - rspec-core includes its expectation and mock adapters in `ExampleGroup` itself, the first
///   time a group is defined (`configure_expectation_framework`, `configure_mock_framework`).
/// - rspec-rails includes one module per spec type (`add_test_type_configurations`), four for every
///   group, and Capybara's two where Capybara is loaded (`vendor/capybara.rb`).
///
/// Each only where the bundle declares it.
const BUILT_IN: [(&str, &[&str]); 18] = [
    (MATCHERS, &[]),
    (MOCKS, &[]),
    ("RSpec::Rails::Matchers", &[]),
    ("RSpec::Rails::FixtureSupport", &[]),
    ("RSpec::Rails::FileFixtureSupport", &[]),
    ("RSpec::Rails::FixtureFileUploadSupport", &[]),
    ("RSpec::Rails::ControllerExampleGroup", &["controller"]),
    ("RSpec::Rails::HelperExampleGroup", &["helper"]),
    ("RSpec::Rails::ModelExampleGroup", &["model"]),
    ("RSpec::Rails::RequestExampleGroup", &["request"]),
    ("RSpec::Rails::RoutingExampleGroup", &["routing"]),
    ("RSpec::Rails::ViewExampleGroup", &["view"]),
    ("RSpec::Rails::FeatureExampleGroup", &["feature"]),
    ("RSpec::Rails::SystemExampleGroup", &["system"]),
    ("RSpec::Rails::MailerExampleGroup", &["mailer"]),
    ("RSpec::Rails::JobExampleGroup", &["job"]),
    ("Capybara::DSL", &["feature", "system"]),
    (
        "Capybara::RSpecMatchers",
        &[
            "view",
            "helper",
            "mailer",
            "controller",
            "feature",
            "system",
        ],
    ),
];

/// rspec-core's expectation adapter.
const MATCHERS: &str = "RSpec::Matchers";

/// rspec-core's mock adapter's methods.
const MOCKS: &str = "RSpec::Mocks::ExampleMethods";

/// rspec-rails' `DIRECTORY_MAPPINGS`, in its order: the `type:` a spec under each directory is
/// given where the project calls `infer_spec_type_from_file_location!`. The pattern is
/// `spec/<directory>/` anywhere in the path.
const INFERRED: [(&str, &[&str]); 13] = [
    ("channel", &["channels"]),
    ("controller", &["controllers"]),
    ("generator", &["generator"]),
    ("helper", &["helpers"]),
    ("job", &["jobs"]),
    ("mailer", &["mailers"]),
    ("model", &["models"]),
    ("request", &["requests", "integration", "api"]),
    ("routing", &["routing"]),
    ("view", &["views"]),
    ("feature", &["features"]),
    ("system", &["system"]),
    ("mailbox", &["mailboxes"]),
];

/// The type rspec-rails infers for a spec at `path`, the first mapping that matches.
fn inferred(path: &str) -> Option<&'static str> {
    INFERRED.iter().find_map(|(kind, directories)| {
        directories
            .iter()
            .any(|directory| path.contains(&format!("spec/{directory}/")))
            .then_some(*kind)
    })
}

/// Every module a project's groups take, resolved: what [`spec_facts`] and [`dsl`] write.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Setup {
    /// Modules every group includes: written on `RSpec::Core::ExampleGroup` itself.
    pub everywhere: Vec<String>,
    /// Modules every group's class object extends: written on each outermost group, because an
    /// `extend` written onto rspec-core's own class is never linked.
    pub extended: Vec<String>,
    /// `(extend, module, filter)` for a module only the groups matching its filter take.
    pub filtered: Vec<(bool, String, Metadata)>,
    /// Whether a spec's directory gives its groups a `type:`.
    pub infers_types: bool,
    /// Whether a project's own modifier may change what a `let_it_be` hands back
    /// ([`Configured::modifiers`]).
    pub modifiers: bool,
    /// The shared groups a support file defines at its top, and each one's module, by name: what
    /// a group including one takes. A name two files define is in neither: which one loads last is
    /// not known here.
    pub shared: BTreeMap<String, (Shared, String)>,
}

/// Every name [`setup`] may ask the bundle about, for the one question the caller batches.
#[must_use]
pub fn wanted_modules(configured: &Configured) -> Vec<String> {
    let mut wanted: Vec<String> = BUILT_IN
        .iter()
        .map(|(module, _)| (*module).to_owned())
        .collect();
    for mixin in configured.mixins.iter().filter(|mixin| !mixin.shared) {
        wanted.extend(candidates_of(&mixin.nesting, &mixin.module));
    }
    wanted
}

/// The names a constant written inside `nesting` could mean, innermost first.
fn candidates_of(nesting: &str, written: &str) -> Vec<String> {
    if nesting.is_empty() {
        vec![written.to_owned()]
    } else {
        crate::generated::candidates(nesting, written)
    }
}

/// What the project's configuration and the bundle make every group take. `declared` says whether
/// the bundle or the project declares a module; a mixin naming nothing it declares, or filtering on
/// a value only running Ruby knows, is left out. `shared` is every support file's top-level shared
/// groups, with each one's module, by name: a name two files define is dropped.
#[must_use]
pub fn setup(
    configured: &Configured,
    declared: &dyn Fn(&str) -> bool,
    shared: Vec<(Shared, String)>,
) -> Setup {
    let mut setup = Setup {
        infers_types: configured.infers_types,
        modifiers: configured.modifiers,
        ..Setup::default()
    };
    let mut twice: BTreeSet<String> = BTreeSet::new();
    for (found, module) in shared {
        let Some(name) = found.name.clone() else {
            continue;
        };
        if setup.shared.insert(name.clone(), (found, module)).is_some() {
            twice.insert(name);
        }
    }
    for name in twice {
        setup.shared.remove(&name);
    }
    for (module, types) in BUILT_IN {
        let elsewhere = (module == MATCHERS && configured.expects_elsewhere)
            || (module == MOCKS && configured.mocks_elsewhere);
        if elsewhere || !declared(module) {
            continue;
        }
        if types.is_empty() {
            setup.everywhere.push(module.to_owned());
        }
        for kind in types {
            setup.filtered.push((
                false,
                module.to_owned(),
                Metadata::from([("type".to_owned(), Some((*kind).to_owned()))]),
            ));
        }
    }
    for mixin in &configured.mixins {
        let module = if mixin.shared {
            setup
                .shared
                .get(&mixin.module)
                .map(|(_, module)| module.clone())
        } else {
            candidates_of(&mixin.nesting, &mixin.module)
                .into_iter()
                .find(|name| declared(name))
        };
        let Some(module) = module else {
            continue;
        };
        if mixin.filter.values().any(Option::is_none) {
            continue;
        }
        match (mixin.filter.is_empty(), mixin.extend) {
            (true, false) => setup.everywhere.push(module),
            (true, true) => setup.extended.push(module),
            (false, extend) => setup.filtered.push((extend, module, mixin.filter.clone())),
        }
    }
    setup
}

/// Whether a group's metadata matches one key of a filter, as RSpec's `any?` does: a `true`
/// filter takes any value that is not `false` or `nil`, anything else the same `to_s`.
fn matches(filter: &Metadata, metadata: &Metadata) -> bool {
    filter.iter().any(
        |(key, wanted)| match (wanted.as_deref(), metadata.get(key)) {
            (Some("true"), Some(Some(value))) => value != "false" && value != "nil",
            (Some(wanted), Some(Some(value))) => wanted == value,
            _ => false,
        },
    )
}

/// The DSL's own signatures, by the owner each is written on: what `self` is in each block, and
/// what `it` returns. Only where the bundle declares the owner; a type it does not declare is
/// written `untyped`. Each parameter is named as rspec-core's `def` names it (`let(name)`,
/// `config.before(scope, *meta)`), since a card prints the names and RBS would otherwise leave
/// `arg0`.
#[must_use]
pub fn dsl(namespaces: &Namespaces, setup: &Setup) -> Vec<(&'static str, Facts)> {
    let named = |name: &str| {
        if namespaces.declares(name) {
            format!("::{name}")
        } else {
            "untyped".to_owned()
        }
    };
    let (example, procsy, group) = (named(EXAMPLE), named(PROCSY), named(EXAMPLE_GROUP));
    let mut hosted = Vec::new();
    let group_calls = || GROUPS.iter().chain(&SHARED);
    if namespaces.declares(EXAMPLE_GROUP) {
        let owner = Owner::Singleton(EXAMPLE_GROUP.to_owned());
        let mut facts = Facts::default();
        for name in group_calls().chain(&BEHAVES).chain(&INCLUDES) {
            facts.declare(dsl_member(
                &owner,
                name,
                &format!("({}) ?{{ (*untyped) -> untyped }}", group_parameters(name)),
                "untyped",
            ));
        }
        for name in EXAMPLES {
            facts.declare(dsl_member(
                &owner,
                name,
                &format!("(*untyped args) ?{{ ({example}) [self: instance] -> untyped }}"),
                &example,
            ));
        }
        for name in HOOKS {
            facts.declare(dsl_member(
                &owner,
                name,
                "(*untyped args) ?{ (untyped) [self: instance] -> untyped }",
                "untyped",
            ));
        }
        facts.declare(dsl_member(
            &owner,
            "around",
            &format!("(*untyped args) ?{{ ({procsy}) [self: instance] -> untyped }}"),
            "untyped",
        ));
        for name in LETS {
            facts.declare(dsl_member(
                &owner,
                name,
                &format!("(untyped name) ?{{ (?{example}) [self: instance] -> untyped }}"),
                "untyped",
            ));
        }
        for name in SUBJECTS {
            facts.declare(dsl_member(
                &owner,
                name,
                &format!("(?untyped name) ?{{ (?{example}) [self: instance] -> untyped }}"),
                "untyped",
            ));
        }
        // test-prof's, where the bundle has it: its block runs on an instance, once.
        if namespaces.declares(LET_IT_BE_MODULE) {
            for name in LET_IT_BE {
                facts.declare(dsl_member(
                    &owner,
                    name,
                    "(untyped identifier, **untyped options) ?{ () [self: instance] -> untyped }",
                    "untyped",
                ));
            }
        }
        for module in &setup.everywhere {
            facts.mixin(
                Owner::Instance(EXAMPLE_GROUP.to_owned()),
                format!("::{module}"),
            );
        }
        hosted.push((EXAMPLE_GROUP, facts));
    }
    if namespaces.declares(RSPEC) {
        let owner = if namespaces.opens(RSPEC) {
            Owner::ModuleSingleton(RSPEC.to_owned())
        } else {
            Owner::Singleton(RSPEC.to_owned())
        };
        let mut facts = Facts::default();
        for name in group_calls() {
            facts.declare(dsl_member(
                &owner,
                name,
                &format!("({}) ?{{ (*untyped) -> untyped }}", group_parameters(name)),
                "untyped",
            ));
        }
        // `yield configuration if block_given?`: the block's value, or `nil` without one.
        facts.declare(Declared {
            overloads: vec![("()".to_owned(), "nil".to_owned())],
            ..dsl_member(
                &owner,
                "configure",
                &format!("[T] () {{ ({}) -> T }}", named(CONFIGURATION)),
                "T",
            )
        });
        hosted.push((RSPEC, facts));
    }
    if namespaces.declares(CONFIGURATION) {
        let owner = Owner::Instance(CONFIGURATION.to_owned());
        let mut facts = Facts::default();
        for name in HOOKS {
            facts.declare(dsl_member(
                &owner,
                name,
                &format!(
                    "(?untyped scope, *untyped meta) ?{{ (untyped) [self: {group}] -> untyped }}"
                ),
                "untyped",
            ));
        }
        facts.declare(dsl_member(
            &owner,
            "around",
            &format!(
                "(?untyped scope, *untyped meta) ?{{ ({procsy}) [self: {group}] -> untyped }}"
            ),
            "untyped",
        ));
        hosted.push((CONFIGURATION, facts));
    }
    hosted.extend(syntax(namespaces));
    hosted
}

/// rspec-expectations' and rspec-mocks' syntax, which both write with `module_exec`,
/// `class_exec` and `define_method`, so no `def` of it is a member of anything: what each call
/// makes, as the gems' own bodies return it. Only where the bundle declares the owner.
fn syntax(namespaces: &Namespaces) -> Vec<(&'static str, Facts)> {
    let named = |name: &str| {
        if namespaces.declares(name) {
            format!("::{name}")
        } else {
            "untyped".to_owned()
        }
    };
    let owner = |name: &str| {
        if namespaces.opens(name) {
            Owner::Module(name.to_owned())
        } else {
            Owner::Instance(name.to_owned())
        }
    };
    let mut hosted = Vec::new();
    if namespaces.declares(MATCHERS) {
        let mut facts = Facts::default();
        let mut expect = syntax_member(
            &owner(MATCHERS),
            "expect",
            "(untyped value)",
            &named(VALUE_TARGET),
        );
        expect.overloads = vec![("() { () -> untyped }".to_owned(), named(BLOCK_TARGET))];
        facts.declare(expect);
        hosted.push((MATCHERS, facts));
    }
    if namespaces.declares(MOCKS) {
        let mut facts = Facts::default();
        for (name, parameters, makes) in MOCK_SYNTAX {
            facts.declare(syntax_member(
                &owner(MOCKS),
                name,
                parameters,
                &named(makes),
            ));
        }
        hosted.push((MOCKS, facts));
    }
    if namespaces.declares(RECEIVE) {
        let mut facts = Facts::default();
        for name in CUSTOMIZATIONS {
            facts.declare(syntax_member(
                &owner(RECEIVE),
                name,
                "(*untyped args) ?{ (*untyped) -> untyped }",
                &format!("::{RECEIVE}"),
            ));
        }
        hosted.push((RECEIVE, facts));
    }
    for target in MOCK_TARGETS {
        if !namespaces.declares(target) {
            continue;
        }
        let mut facts = Facts::default();
        for name in ["to", "not_to", "to_not"] {
            facts.declare(syntax_member(
                &owner(target),
                name,
                "(untyped matcher) ?{ (*untyped) -> untyped }",
                "untyped",
            ));
        }
        hosted.push((target, facts));
    }
    hosted
}

/// How rubydex spells the class object a group's DSL is declared on.
const GROUP_SINGLETON: &str = "RSpec::Core::ExampleGroup::<ExampleGroup>";

/// The declaration whose `def` is the code a member [`dsl`] or [`syntax`] declared without a place
/// really runs, as rubydex files it: where a jump on the member goes. `owner` and `method` spell
/// the member's own declaration.
///
/// - **rspec-core's DSL**, which `ExampleGroup` extends: `Hooks`, `MemoizedHelpers::ClassMethods`
///   and `SharedExampleGroup` write `def before`, `def let`, `def shared_examples` (the rest are
///   `alias`es of them), and test-prof's `LetItBe` writes `def let_it_be`. `describe`, `it` and
///   `it_behaves_like` are `define_method`s, and `let_it_be`'s shorthands too: nothing.
/// - **The gems' syntax**: `Syntax.enable_expect` writes each `def` inside a `module_exec` or
///   `class_exec` on the member's owner, which rubydex files under the `Syntax` module the body is
///   written in. The mock targets' `to` is `delegate_to`'s `define_method`: nothing.
/// - **`Receive`'s customizations** are `MessageExpectation`'s own `def`s, which it records and
///   replays on the expectation it sets up.
///
/// The ancestors are not walked instead: past a `define_method` on the class object they reach
/// whatever else the bundle names alike, minitest's `Kernel#describe` among them.
#[must_use]
pub fn written_in(owner: &str, method: &str) -> Option<String> {
    let module = match owner {
        GROUP_SINGLETON if HOOKS.contains(&method) || method == "around" => "RSpec::Core::Hooks",
        GROUP_SINGLETON if LETS.contains(&method) || SUBJECTS.contains(&method) => {
            "RSpec::Core::MemoizedHelpers::ClassMethods"
        }
        GROUP_SINGLETON if SHARED.contains(&method) => "RSpec::Core::SharedExampleGroup",
        GROUP_SINGLETON if method == LET_IT_BE[0] => LET_IT_BE_MODULE,
        MATCHERS if method == "expect" => "RSpec::Expectations::Syntax",
        MOCKS if MOCK_SYNTAX.iter().any(|(name, ..)| *name == method) => "RSpec::Mocks::Syntax",
        RECEIVE if CUSTOMIZATIONS.contains(&method) => "RSpec::Mocks::MessageExpectation",
        _ => return None,
    };
    Some(format!("{module}#{method}()"))
}

/// A group call's parameters, as rspec-core names them: `describe(*args)`, but a shared group,
/// `it_behaves_like` and `include_context` take the shared group's name first.
fn group_parameters(name: &str) -> &'static str {
    if GROUPS.contains(&name) {
        "*untyped args"
    } else {
        "untyped name, *untyped args"
    }
}

/// One of [`syntax`]'s members: unplaced, as `define_method` leaves it. A jump goes to the gem's
/// own `def` where one is written ([`written_in`]).
fn syntax_member(owner: &Owner, name: &str, parameters: &str, returns: &str) -> Declared {
    Declared {
        because: format!(
            "RSpec's `{name}`, which the gem defines at run time; ya-lsp writes what its body makes."
        ),
        ..dsl_member(owner, name, parameters, returns)
    }
}

/// One of [`dsl`]'s members: unplaced, so a jump goes to rspec-core's own `def` where it writes
/// one, on the member's declaration itself (`config.before`, `RSpec.configure`) or on a module the
/// group extends ([`written_in`]).
fn dsl_member(owner: &Owner, name: &str, parameters: &str, returns: &str) -> Declared {
    Declared {
        owner: owner.clone(),
        name: name.to_owned(),
        returns: returns.to_owned(),
        parameters: parameters.to_owned(),
        because: format!("RSpec's `{name}`; ya-lsp writes what its block runs against."),
        at: None,
        from: Source::Interface,
        overloads: Vec::new(),
        private: false,
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::{declaring, declaring_kinds};

    /// What a group was read as, compactly: its segment, its parent's, what it describes, its
    /// `let`s, its metadata and what it includes.
    fn outline(spec: &Spec) -> Vec<String> {
        spec.groups
            .iter()
            .map(|group| {
                let parent = group
                    .parent
                    .map_or("-".to_owned(), |parent| spec.groups[parent].segment.clone());
                let lets: Vec<String> = group
                    .lets
                    .iter()
                    .map(|written| {
                        format!(
                            "{}{}{}",
                            written.name,
                            if written.subject { "=subject" } else { "" },
                            if written.block { "" } else { "!block" }
                        )
                    })
                    .collect();
                let includes: Vec<&str> = group
                    .includes
                    .iter()
                    .map(|(_, name)| name.as_str())
                    .collect();
                format!(
                    "{} < {parent} [{}] {:?} lets={lets:?} meta={:?} includes={includes:?}{}{}",
                    group.segment,
                    group.nesting,
                    group.described,
                    group.metadata,
                    if group.call.is_some() {
                        ""
                    } else {
                        " no-block"
                    },
                    group
                        .shared
                        .map_or(String::new(), |shared| format!(" in shared {shared}"))
                )
            })
            .collect()
    }

    #[test]
    fn a_description_is_spelled_as_rspec_spells_it() {
        assert_eq!(base_name("User"), "User");
        assert_eq!(base_name("#call"), "Call");
        assert_eq!(base_name("User#name"), "UserName");
        assert_eq!(base_name("when it's valid"), "WhenItSValid");
        assert_eq!(base_name("123 go"), "Nested123Go");
        assert_eq!(base_name(""), "Anonymous");
        assert_eq!(base_name(" ?! "), "Anonymous");
        assert_eq!(
            file_module("spec/models/user_spec.rb"),
            "RSpec::ExampleGroups::SpecModelsUserSpec"
        );
        assert_eq!(file_module("Rakefile"), "RSpec::ExampleGroups::Rakefile");
        assert_eq!(
            inferred("plugins/chat/spec/models/x_spec.rb"),
            Some("model")
        );
        assert_eq!(inferred("spec/api/v1/x_spec.rb"), Some("request"));
        assert_eq!(inferred("spec/lib/x_spec.rb"), None);
    }

    /// Every shape the reader takes and every one it passes over, in one file.
    #[test]
    fn what_a_spec_file_is_read_as() {
        let source = "\
module Spree
  RSpec.describe Order, \"#total\", :js, type: :model, id: 1, on: false, off: nil do
    let(:order) { Order.new }
    let!(:line) { 1 }
    subject { order }
    subject(:named) { 2 }
    let(\"quoted\") { 3 }
    let(:\"not a name\") { 4 }
    let(:no_block)
    let { 5 }
    let(some_name) { 6 }
    subject!
    context \"when empty\", :slow do
      it { describe \"inside an example\" do end }
      around { |example| example.run }
      let(:inner) { 7 }
    end
    context \"when empty\" do
    end
    describe :symbol do
    end
    describe some_call, \"plain\" do
    end
    describe \"#{interpolated}\" do
    end
    describe Order, \"words\" do
    end
    %w[a b].each do |name|
      context name do
      end
    end
    it_behaves_like \"a thing\" do
      let(:custom) { 8 }
    end
    it_should_behave_like \"another\"
    it_behaves_like
    include_context \"with setup\"
    include_examples Shared::Examples
    include_context
    shared_examples \"local\" do
      let(:shared_let) { 9 }
      describe \"nested in shared\" do end
      it { }
      something.each { let(:inside_each) { 10 } }
    end
    def helper
      describe \"in a def\" do end
    end
    class Inner
      describe \"in a class in a group\" do end
    end
    RSpec.describe \"isolated\" do end
  end
end
RSpec.shared_context :top do
end
::RSpec.describe do
end
foo.describe \"not a group\" do
end
describe \"no block\"
let(:outside) { 1 }
";
        let spec = read_spec(source);
        assert_eq!(
            outline(&spec),
            [
                "OrderTotal < - [Spree] Constant(\"Order\") lets=[\"order\", \"line\", \"subject\", \
                 \"named=subject\", \"quoted\", \"no_block!block\", \"subject!block\"] \
                 meta={\"id\": None, \"js\": Some(\"true\"), \"off\": Some(\"nil\"), \"on\": \
                 Some(\"false\"), \"type\": Some(\"model\")} includes=[\"with setup\", \
                 \"Shared::Examples\"]",
                "WhenEmpty < OrderTotal [Spree] Inherited lets=[\"inner\"] meta={\"slow\": \
                 Some(\"true\")} includes=[]",
                "WhenEmpty_2 < OrderTotal [Spree] Inherited lets=[] meta={} includes=[]",
                "Symbol < OrderTotal [Spree] Unknown lets=[] meta={} includes=[]",
                "GroupPlain < OrderTotal [Spree] Unknown lets=[] meta={} includes=[]",
                "Group < OrderTotal [Spree] Inherited lets=[] meta={} includes=[]",
                "OrderWords < OrderTotal [Spree] Constant(\"Order\") lets=[] meta={} includes=[]",
                "Group_2 < OrderTotal [Spree] Unknown lets=[] meta={} includes=[]",
                "BehavesLikeAThing < OrderTotal [Spree] Inherited lets=[\"custom\"] meta={} \
                 includes=[\"a thing\"]",
                "BehavesLikeAnother < OrderTotal [Spree] Inherited lets=[] meta={} \
                 includes=[\"another\"] no-block",
                "BehavesLike < OrderTotal [Spree] Inherited lets=[] meta={} includes=[] no-block",
                "NestedInShared < - [Spree] Inherited lets=[] meta={} includes=[] in shared 0",
                "Anonymous < - [] Inherited lets=[] meta={} includes=[]",
            ]
        );
        let shared: Vec<(Option<&str>, Option<usize>, Vec<&str>)> = spec
            .shared
            .iter()
            .map(|shared| {
                (
                    shared.name.as_deref(),
                    shared.parent,
                    shared
                        .lets
                        .iter()
                        .map(|written| written.name.as_str())
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            shared,
            [
                (Some("local"), Some(0), vec!["shared_let", "inside_each"]),
                (Some("top"), None, vec![]),
            ]
        );

        // The rest of what the walk passes over, and the shapes a name or a description takes.
        let edges = read_spec(
            "\
let(:top) { 1 }
it { }
include_context \"at the top\"
it_behaves_like \"at the top\"
module Empty; end
shared_examples \"no block\"
foo.shared_examples \"other\" do
end
shared_context \"busy\" do
  subject { 1 }
  let(:\"not a name\") { 0 }
  foo.let(:not_mine) { 2 }
  let(:no_block)
  it_behaves_like \"x\"
  shared_examples \"y\" do end
  before { }
  include_context \"z\"
  around { }
  foo.describe \"w\" do end
end
describe Order, \".where\", **options, \"key\" => 1, type: \"request\" do
  before { }
  foo.it { }
  foo.let(:not_mine) { 0 }
  foo.it_behaves_like \"x\"
  let(\"\") { 1 }
  let(:\"9lives\") { 2 }
  let(:valid?) { 3 }
end
describe Order, \"::Nested\" do
end
",
        );
        assert_eq!(
            outline(&edges),
            [
                // A shared group's `it_behaves_like` makes a group of its own, filed under it.
                "BehavesLikeX < - [] Inherited lets=[] meta={} includes=[\"x\"] no-block in shared 0",
                "OrderWhere < - [] Constant(\"Order\") lets=[\"valid?\"] meta={\"type\": \
                 Some(\"request\")} includes=[]",
                "OrderNested < - [] Constant(\"Order\") lets=[] meta={} includes=[]",
            ]
        );
        let lets: Vec<Vec<&str>> = edges
            .shared
            .iter()
            .map(|shared| {
                shared
                    .lets
                    .iter()
                    .map(|written| written.name.as_str())
                    .collect()
            })
            .collect();
        assert_eq!(lets, [vec!["subject", "no_block"]]);
        assert!(!edges.shared[0].lets[1].block);
    }

    /// Every `RSpec.configure` shape, and every call the reader passes over.
    #[test]
    fn what_a_configure_block_is_read_as() {
        let source = "\
module Support
  RSpec.configure do |config|
    config.include Helpers
    config.include Requests, :js, type: :request
    config.extend ::Tagged, :slow, some_value
    config.include_context \"with api\", api: true
    config.include_context some_name
    config.include helpers_for(:x)
    config.include
    config.infer_spec_type_from_file_location!
    config.mock_with :rspec do |mocks|
      mocks.include Ignored
    end
    config.expect_with :minitest
    config.mock_with :mocha
    config.order = :random
    other.include Ignored
    if ENV[\"CI\"]
      config.include Conditional
    end
  end
end
RSpec.configure { |c| c.expect_with :rspec }
RSpec.configure do
  include Ignored
end
Other.configure do |config|
  config.include Ignored
end
def later
  RSpec.configure { |config| config.include Ignored }
end
class Kept
  RSpec.configure { |config| config.include InClass }
end
module Empty; end
RSpec.configure
RSpec.configure { |config| }
RSpec.configure { _1.include Ignored }
RSpec.configure { |; local| local }
";
        let configured = read_configured(source);
        let mixins: Vec<String> = configured
            .mixins
            .iter()
            .map(|mixin| {
                format!(
                    "{}{} {} [{}] {:?}",
                    if mixin.extend { "extend" } else { "include" },
                    if mixin.shared { "_context" } else { "" },
                    mixin.module,
                    mixin.nesting,
                    mixin.filter
                )
            })
            .collect();
        assert_eq!(
            mixins,
            [
                "include Helpers [Support] {}",
                "include Requests [Support] {\"js\": Some(\"true\"), \"type\": Some(\"request\")}",
                "extend Tagged [Support] {\"\": None, \"slow\": Some(\"true\")}",
                "include_context with api [Support] {\"api\": Some(\"true\")}",
                "include Conditional [Support] {}",
                "include InClass [Kept] {}",
            ]
        );
        assert!(configured.infers_types);
        assert!(configured.mocks_elsewhere);
        assert!(configured.expects_elsewhere);
        let plain = read_configured("RSpec.configure { |c| c.mock_with :rspec }\n");
        assert!(!plain.mocks_elsewhere && !plain.expects_elsewhere && !plain.infers_types);
    }

    /// `(extend, shared, module, filter)`, one per mixin.
    type Written<'a> = (bool, bool, &'a str, &'a [(&'a str, Option<&'a str>)]);

    fn configured(mixins: &[Written<'_>]) -> Configured {
        Configured {
            mixins: mixins
                .iter()
                .map(|(extend, shared, module, filter)| Mixin {
                    extend: *extend,
                    shared: *shared,
                    module: (*module).to_owned(),
                    nesting: String::new(),
                    filter: filter
                        .iter()
                        .map(|(key, value)| ((*key).to_owned(), value.map(str::to_owned)))
                        .collect(),
                })
                .collect(),
            ..Configured::default()
        }
    }

    fn shared(name: Option<&str>, lets: &[&str]) -> Shared {
        Shared {
            name: name.map(str::to_owned),
            call: 0,
            parent: None,
            lets: lets
                .iter()
                .map(|name| Let {
                    name: (*name).to_owned(),
                    at: ((0, 1), (0, 1)),
                    subject: false,
                    block: true,
                    it_be: false,
                })
                .collect(),
            defs: Vec::new(),
        }
    }

    /// Which modules every group takes, which only a tag's or a type's, and which none.
    #[test]
    fn the_setup_a_configuration_and_the_bundle_make() {
        let declared = |name: &str| {
            [
                "RSpec::Matchers",
                "RSpec::Mocks::ExampleMethods",
                "RSpec::Rails::RequestExampleGroup",
                "Capybara::DSL",
                "Helpers",
                "Support::Nested",
                "Tagged",
            ]
            .contains(&name)
        };
        let mut read = configured(&[
            (false, false, "Helpers", &[]),
            (true, false, "Helpers", &[]),
            (false, false, "Tagged", &[("slow", Some("true"))]),
            (false, false, "Missing", &[]),
            (false, false, "Helpers", &[("", None)]),
            (false, true, "with api", &[]),
            (false, true, "tagged api", &[("api", Some("true"))]),
            (false, true, "twice", &[]),
            (false, true, "nowhere", &[]),
        ]);
        read.mixins.push(Mixin {
            extend: false,
            shared: false,
            module: "Nested".to_owned(),
            nesting: "Support".to_owned(),
            filter: Metadata::new(),
        });
        read.mocks_elsewhere = true;
        let setup = setup(
            &read,
            &declared,
            vec![
                (
                    shared(Some("with api"), &["token"]),
                    "M::SharedWithApi".to_owned(),
                ),
                (
                    shared(Some("tagged api"), &[]),
                    "M::SharedTaggedApi".to_owned(),
                ),
                (shared(Some("twice"), &[]), "M::A".to_owned()),
                (shared(Some("twice"), &[]), "M::B".to_owned()),
                (shared(None, &[]), "M::Nameless".to_owned()),
            ],
        );
        assert_eq!(
            setup.everywhere,
            [
                "RSpec::Matchers",
                "Helpers",
                "M::SharedWithApi",
                "Support::Nested"
            ]
        );
        assert_eq!(setup.extended, ["Helpers"]);
        let filtered: Vec<(bool, &str, Vec<&str>)> = setup
            .filtered
            .iter()
            .map(|(extend, module, filter)| {
                (
                    *extend,
                    module.as_str(),
                    filter.keys().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            filtered,
            [
                (false, "RSpec::Rails::RequestExampleGroup", vec!["type"]),
                (false, "Capybara::DSL", vec!["type"]),
                (false, "Capybara::DSL", vec!["type"]),
                (false, "Tagged", vec!["slow"]),
                (false, "M::SharedTaggedApi", vec!["api"]),
            ]
        );
        assert!(!setup.shared.contains_key("twice"), "two files define it");
        assert!(setup.shared.contains_key("with api"));
        assert!(
            wanted_modules(&read)
                .iter()
                .any(|name| name == "Support::Nested")
        );
        assert!(!wanted_modules(&read).iter().any(|name| name == "with api"));
        let unmocked = configured(&[]);
        let quiet = super::setup(
            &Configured {
                expects_elsewhere: true,
                ..unmocked
            },
            &declared,
            Vec::new(),
        );
        assert_eq!(quiet.everywhere, ["RSpec::Mocks::ExampleMethods"]);
    }

    #[test]
    fn a_filter_matches_one_key_as_rspec_does() {
        let meta = |pairs: &[(&str, Option<&str>)]| -> Metadata {
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.map(str::to_owned)))
                .collect()
        };
        let group = meta(&[
            ("type", Some("request")),
            ("js", Some("true")),
            ("off", Some("false")),
            ("none", Some("nil")),
            ("unread", None),
        ]);
        assert!(matches(&meta(&[("type", Some("request"))]), &group));
        assert!(!matches(&meta(&[("type", Some("model"))]), &group));
        assert!(matches(&meta(&[("js", Some("true"))]), &group));
        assert!(!matches(&meta(&[("off", Some("true"))]), &group));
        assert!(!matches(&meta(&[("none", Some("true"))]), &group));
        assert!(!matches(&meta(&[("unread", Some("x"))]), &group));
        assert!(!matches(&meta(&[("absent", Some("x"))]), &group));
        assert!(matches(
            &meta(&[("type", Some("model")), ("js", Some("true"))]),
            &group
        ));
        assert!(!matches(&Metadata::new(), &group));
    }

    /// The RBS a spec file implies, whole.
    #[test]
    fn the_rbs_a_spec_file_implies() {
        let source = "\
describe Order, type: :request do
  let(:early) { 1 }
  include_context \"local\"
  include_context \"from support\"
  include_context \"unknown\"
  let(:late) { 2 }
  shared_context \"local\" do
    let(:early) { 3 }
    let(:named) { 4 }
  end
  shared_context \"local\" do
    let(:early) { 5 }
    let(:bare)
  end
  it_should_behave_like \"local\"
  context \"nested\", :slow do
    subject(:thing) { 6 }
  end
  describe Checkout do
    let(:no_block)
  end
  describe Mixin do
  end
  describe :unknown do
  end
  describe Unresolved do
  end
end
shared_examples do
end
";
        let spec = read_spec(source);
        let setup = Setup {
            extended: vec!["Everywhere".to_owned()],
            filtered: vec![
                (
                    false,
                    "Requests".to_owned(),
                    Metadata::from([("type".to_owned(), Some("request".to_owned()))]),
                ),
                (
                    true,
                    "Slow".to_owned(),
                    Metadata::from([("slow".to_owned(), Some("true".to_owned()))]),
                ),
            ],
            infers_types: true,
            shared: BTreeMap::from([(
                "from support".to_owned(),
                (
                    shared(Some("from support"), &["subject"]),
                    "Support::SharedFromSupport".to_owned(),
                ),
            )]),
            ..Setup::default()
        };
        let resolve = |_: &str, constant: &str| match constant {
            "Order" | "Checkout" => Some(Resolved {
                name: constant.to_owned(),
                module: false,
            }),
            "Mixin" => Some(Resolved {
                name: constant.to_owned(),
                module: true,
            }),
            _ => None,
        };
        let facts = spec_facts(&spec, "F", "spec/requests/order_spec.rb", &setup, &resolve);
        let rendered = facts.render(&declaring(&["F"])).rbs;
        // The `let` above `include_context "local"` is its: RSpec redefines it. The second "local"
        // is the one found; "unknown" names nothing; a support file's shared group defines
        // `subject`, so no group here has an implicit one.
        assert_eq!(
            rendered
                .lines()
                .filter(|line| !line.trim_start().starts_with('#'))
                .collect::<Vec<_>>(),
            [
                "module F",
                "module SharedLocal",
                "  def early: () -> ReturnedByItsBlock",
                "  def named: () -> ReturnedByItsBlock",
                "end",
                "end",
                "module F",
                "module SharedLocal_2",
                "  def bare: () -> untyped",
                "  def early: () -> ReturnedByItsBlock",
                "end",
                "end",
                "module F",
                "class Order < ::RSpec::Core::ExampleGroup",
                "  include ::Requests",
                "  include ::F::SharedLocal_2",
                "  include ::Support::SharedFromSupport",
                "  extend ::Everywhere",
                "  def self.described_class: () -> singleton(::Order)",
                "  def described_class: () -> singleton(::Order)",
                "  def late: () -> ReturnedByItsBlock",
                "end",
                "end",
                "class F::Order::BehavesLikeLocal < ::F::Order",
                "  include ::F::SharedLocal_2",
                "  def self.described_class: () -> singleton(::Order)",
                "end",
                "class F::Order::Nested < ::F::Order",
                "  extend ::Slow",
                "  def self.described_class: () -> singleton(::Order)",
                "  def subject: () -> ReturnedByItsBlock",
                "  def thing: () -> ReturnedByItsBlock",
                "end",
                "class F::Order::Checkout < ::F::Order",
                "  def self.described_class: () -> singleton(::Checkout)",
                "  def described_class: () -> singleton(::Checkout)",
                "  def no_block: () -> untyped",
                "end",
                "class F::Order::Mixin < ::F::Order",
                "  def self.described_class: () -> singleton(::Mixin)",
                "  def described_class: () -> singleton(::Mixin)",
                "end",
                "class F::Order::Unknown < ::F::Order",
                "  def self.described_class: () -> untyped",
                "  def described_class: () -> untyped",
                "end",
                "class F::Order::Unresolved < ::F::Order",
                "  def self.described_class: () -> untyped",
                "  def described_class: () -> untyped",
                "end",
                "module F",
                "end",
                "module F",
                "module SharedAnonymous",
                "end",
                "end",
            ]
        );
        let ran = facts.render(&declaring(&["F"])).ran;
        assert!(ran.contains(&(0, Runs::Made("F::Order".to_owned()))));
        assert!(
            ran.contains(&(162, Runs::Made(EXAMPLE_GROUP.to_owned()))),
            "{ran:?}"
        );

        // The implicit subject: a class's instance, a module itself, and nothing Ruby cannot say.
        let implicit = read_spec(
            "describe Mixin do\nend\ndescribe :sym do\nend\ndescribe Order do\n  describe \"x\" do\n  end\nend\n",
        );
        let rbs = spec_facts(
            &implicit,
            "F",
            "spec/x_spec.rb",
            &Setup {
                infers_types: true,
                ..Setup::default()
            },
            &resolve,
        )
        .render(&declaring(&["F"]))
        .rbs;
        assert!(
            rbs.contains("def subject: () -> singleton(::Mixin)"),
            "{rbs}"
        );
        assert!(rbs.contains("def subject: () -> untyped"), "{rbs}");
        assert!(rbs.contains("def subject: () -> ::Order"), "{rbs}");
        assert_eq!(
            rbs.matches("def subject").count(),
            3,
            "a nested group inherits it: {rbs}"
        );
    }

    /// A support file's top-level shared groups, as modules, and nothing else it writes.
    #[test]
    fn a_support_files_shared_groups_are_modules() {
        let spec = read_spec(
            "RSpec.shared_context \"api\" do\n  let(:token) { 1 }\nend\n\ndescribe \"not here\" do\n  \
             shared_examples \"inner\" do\n  end\nend\n",
        );
        let (facts, shared) = support_facts(&spec, "S", "spec/support/api.rb", false);
        let rbs = facts.render(&declaring(&["S"])).rbs;
        assert!(rbs.contains("module SharedApi"), "{rbs}");
        assert!(rbs.contains("def token: () -> ReturnedByItsBlock"), "{rbs}");
        assert!(!rbs.contains("NotHere") && !rbs.contains("Inner"), "{rbs}");
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].1, "S::SharedApi");
    }

    /// The DSL's own rows, only on what the bundle declares, and a type it does not declare
    /// written `untyped`.
    #[test]
    fn a_shared_group_s_own_group_takes_its_module_and_no_directory_type() {
        let spec = read_spec(
            "shared_context \"s\" do\n  context \"inner\" do\n  end\n  context \"blockless\"\n  setup_things\nend\n\n\
             describe Order do\nend\n",
        );
        let setup = Setup {
            infers_types: true,
            filtered: vec![(
                false,
                "RequestHelpers".to_owned(),
                Metadata::from([("type".to_owned(), Some("request".to_owned()))]),
            )],
            ..Setup::default()
        };
        let resolve = |_: &str, _: &str| None;
        let rbs = spec_facts(&spec, "F", "spec/requests/order_spec.rb", &setup, &resolve)
            .render(&declaring(&["F"]))
            .rbs;
        // The file's own group takes its directory's type; the shared group's does not, since the
        // group including it may be anywhere.
        assert_eq!(rbs.matches("include ::RequestHelpers").count(), 1, "{rbs}");
        assert!(rbs.contains("include ::F::SharedS"), "{rbs}");
    }

    #[test]
    fn a_def_in_a_block_is_its_group_s_or_its_shared_group_s() {
        let source = "def top; end\n\ndescribe Story do\n  def a; end\n  def self.b; end\n  def other.c; end\n\n  \
             it do\n    def in_example; end\n  end\n\n  shared_context \"s\" do\n    def d; end\n    \
             def self.e; end\n  end\nend\n";
        let spec = read_spec(source);
        let named = |defs: &[Def]| -> Vec<(String, bool)> {
            defs.iter()
                .map(|def| (def.name.clone(), def.singleton))
                .collect()
        };
        assert_eq!(
            named(&spec.groups[0].defs),
            [("a".to_owned(), false), ("b".to_owned(), true)]
        );
        assert_eq!(named(&spec.shared[0].defs), [("d".to_owned(), false)]);
        let ((start, end), (name, name_end)) = spec.groups[0].defs[0].at;
        assert_eq!(&source[start as usize..end as usize], "def a; end");
        assert_eq!(&source[name as usize..name_end as usize], "a");
    }

    #[test]
    fn a_let_it_be_is_a_let_unless_a_modifier_of_the_project_s_own_may_change_it() {
        let spec = read_spec(
            "describe Story do\n  let_it_be(:a) { 1 }\n  let_it_be(:b, reload: true, freeze: false) { 1 }\n  \
             let_it_be_with_refind(:c) { 1 }\n  let_it_be(:d, touched: true) { 1 }\n  \
             let_it_be(:e, :positional) { 1 }\n  let(:f) { 1 }\n\n  shared_context \"s\" do\n    \
             let_it_be(:g) { 1 }\n  end\nend\n",
        );
        let read: Vec<(&str, bool, bool)> = spec.groups[0]
            .lets
            .iter()
            .chain(&spec.shared[0].lets)
            .map(|written| (written.name.as_str(), written.block, written.it_be))
            .collect();
        assert_eq!(
            read,
            [
                ("a", true, true),
                ("b", true, true),
                ("c", true, true),
                ("d", false, true),
                ("e", false, true),
                ("f", true, false),
                ("g", true, true),
            ]
        );
        let plain = &spec.groups[0].lets[0];
        let ordinary = &spec.groups[0].lets[5];
        assert_eq!(returns(plain, false), BLOCK);
        assert_eq!(returns(plain, true), "untyped");
        assert_eq!(returns(ordinary, true), BLOCK);
        // Registered in a file that names test-prof, on whatever `config` it was handed.
        assert!(
            read_configured(
                "TestProf::LetItBe.configure do |config|\n  config.register_modifier :touch do |r, _|\n    r\n  end\nend\n"
            )
            .modifiers
        );
        // A plugin API may have a `register_modifier` of its own.
        assert!(!read_configured("register_modifier(:topic_args) { |args| args }\n").modifiers);
    }

    #[test]
    fn the_gems_run_time_syntax_is_written_where_the_bundle_declares_it() {
        let rbs = |bundle: &Namespaces| -> String {
            syntax(bundle)
                .iter()
                .map(|(_, facts)| facts.render(bundle).rbs)
                .collect()
        };
        assert!(syntax(&declaring(&[])).is_empty());
        let all = declaring_kinds(
            &[
                VALUE_TARGET,
                BLOCK_TARGET,
                RECEIVE,
                MOCK_TARGETS[1],
                "RSpec::Mocks::AnyInstanceAllowanceTarget",
            ],
            &[
                MATCHERS,
                MOCKS,
                "RSpec",
                "RSpec::Expectations",
                "RSpec::Mocks",
                "RSpec::Mocks::Matchers",
            ],
        );
        let written = rbs(&all);
        for expected in [
            "def expect: (untyped value) -> ::RSpec::Expectations::ValueExpectationTarget \
             | () { () -> untyped } -> ::RSpec::Expectations::BlockExpectationTarget",
            "def receive: (untyped method_name) ?{ (*untyped) -> untyped } \
             -> ::RSpec::Mocks::Matchers::Receive",
            "def receive_message_chain: (*untyped messages) ?{ (*untyped) -> untyped } -> untyped",
            "def allow: (untyped target) -> ::RSpec::Mocks::AllowanceTarget",
            "def expect_any_instance_of: (untyped klass) -> untyped",
            // A class the bundle does not declare is `untyped`.
            "def receive_messages: (untyped message_return_value_hash) -> untyped",
            "def and_return: (*untyped args) ?{ (*untyped) -> untyped } \
             -> ::RSpec::Mocks::Matchers::Receive",
            "def to_s: (*untyped args) ?{ (*untyped) -> untyped } -> ::RSpec::Mocks::Matchers::Receive",
            "def not_to: (untyped matcher) ?{ (*untyped) -> untyped } -> untyped",
        ] {
            assert!(written.contains(expected), "{expected}\n---\n{written}");
        }
        assert!(
            written.contains("module RSpec\nmodule Matchers\n"),
            "{written}"
        );
        assert!(
            written.contains("module RSpec::Mocks::Matchers\nclass Receive\n"),
            "{written}"
        );
        // Only the targets the bundle declares get `to`.
        assert_eq!(written.matches("def to_not:").count(), 2, "{written}");
        // Declared as classes, `RSpec::Matchers` and `ExampleMethods` are opened as classes.
        let classes = declaring_kinds(&[MATCHERS, MOCKS], &[]);
        let written = rbs(&classes);
        assert!(written.contains("class RSpec::Matchers\n"), "{written}");
        assert!(
            written.contains(
                "def expect: (untyped value) -> untyped | () { () -> untyped } -> untyped"
            ),
            "{written}"
        );
    }

    #[test]
    fn the_dsl_is_written_on_what_the_bundle_declares() {
        let setup = Setup {
            everywhere: vec!["Helpers".to_owned()],
            ..Setup::default()
        };
        assert!(dsl(&declaring(&[]), &setup).is_empty());
        let bundle = declaring_kinds(
            &[EXAMPLE_GROUP, EXAMPLE, PROCSY, CONFIGURATION],
            &[RSPEC, "RSpec::Core"],
        );
        let hosted = dsl(&bundle, &setup);
        let owners: Vec<&str> = hosted.iter().map(|(owner, _)| *owner).collect();
        assert_eq!(owners, [EXAMPLE_GROUP, RSPEC, CONFIGURATION]);
        let rbs: String = hosted
            .iter()
            .map(|(_, facts)| facts.render(&bundle).rbs)
            .collect();
        assert!(rbs.contains("include ::Helpers"), "{rbs}");
        assert!(
            rbs.contains(
                "def self.it: (*untyped args) ?{ (::RSpec::Core::Example) [self: instance] \
                 -> untyped } -> ::RSpec::Core::Example"
            ),
            "{rbs}"
        );
        // Each parameter named as rspec-core's `def` names it.
        for expected in [
            "def self.describe: (*untyped args) ?{ (*untyped) -> untyped } -> untyped",
            "def self.shared_examples: (untyped name, *untyped args) ?{ (*untyped) -> untyped }",
            "def self.it_behaves_like: (untyped name, *untyped args) ?{ (*untyped) -> untyped }",
            "def self.include_context: (untyped name, *untyped args) ?{ (*untyped) -> untyped }",
            "def self.before: (*untyped args) ?{ (untyped) [self: instance] -> untyped }",
            "def self.let: (untyped name) ?{ (?::RSpec::Core::Example) [self: instance] -> untyped }",
            "def self.subject!: (?untyped name) ?{ (?::RSpec::Core::Example) [self: instance] \
             -> untyped }",
            "def self.shared_context: (untyped name, *untyped args)",
            "def before: (?untyped scope, *untyped meta) ?{ (untyped) [self: \
             ::RSpec::Core::ExampleGroup] -> untyped }",
            "def around: (?untyped scope, *untyped meta) ?{ (::RSpec::Core::Example::Procsy) \
             [self: ::RSpec::Core::ExampleGroup] -> untyped }",
        ] {
            assert!(rbs.contains(expected), "{expected}\n---\n{rbs}");
        }
        assert!(rbs.contains("[self: ::RSpec::Core::ExampleGroup]"), "{rbs}");
        assert!(
            rbs.contains("module RSpec"),
            "RSpec opens as the module it is: {rbs}"
        );
        assert!(
            rbs.contains(
                "def self.configure: [T] () { (::RSpec::Core::Configuration) -> T } -> T | () -> nil"
            ),
            "{rbs}"
        );
        // A class `RSpec`, and no `Example` or `Procsy` declared: the types are `untyped`.
        let bare = declaring_kinds(&[EXAMPLE_GROUP, RSPEC], &[]);
        let rbs: String = dsl(&bare, &Setup::default())
            .iter()
            .map(|(_, facts)| facts.render(&bare).rbs)
            .collect();
        assert!(rbs.contains("class RSpec"), "{rbs}");
        assert!(
            rbs.contains("?{ (untyped) [self: instance] -> untyped } -> untyped"),
            "{rbs}"
        );
        assert!(!rbs.contains("Configuration"), "{rbs}");
    }

    #[test]
    fn a_member_the_gems_write_in_a_def_names_that_def_and_one_they_define_names_nothing() {
        let group = "RSpec::Core::ExampleGroup::<ExampleGroup>";
        let written = |owner: &str, method: &str| written_in(owner, method);
        for (owner, method, def) in [
            (group, "before", "RSpec::Core::Hooks#before()"),
            (group, "append_after", "RSpec::Core::Hooks#append_after()"),
            (group, "around", "RSpec::Core::Hooks#around()"),
            (
                group,
                "let!",
                "RSpec::Core::MemoizedHelpers::ClassMethods#let!()",
            ),
            (
                group,
                "subject",
                "RSpec::Core::MemoizedHelpers::ClassMethods#subject()",
            ),
            (
                group,
                "shared_context",
                "RSpec::Core::SharedExampleGroup#shared_context()",
            ),
            (group, "let_it_be", "TestProf::LetItBe#let_it_be()"),
            (
                "RSpec::Matchers",
                "expect",
                "RSpec::Expectations::Syntax#expect()",
            ),
            (
                "RSpec::Mocks::ExampleMethods",
                "allow_any_instance_of",
                "RSpec::Mocks::Syntax#allow_any_instance_of()",
            ),
            (
                "RSpec::Mocks::Matchers::Receive",
                "with",
                "RSpec::Mocks::MessageExpectation#with()",
            ),
        ] {
            assert_eq!(
                written(owner, method).as_deref(),
                Some(def),
                "{owner}#{method}"
            );
        }
        // `define_method`s, and names the owner does not have.
        for (owner, method) in [
            (group, "describe"),
            (group, "it"),
            (group, "it_behaves_like"),
            (group, "let_it_be_with_reload"),
            (group, "expect"),
            ("RSpec::Mocks::AllowanceTarget", "to"),
            ("RSpec::Matchers", "allow"),
            ("RSpec::Mocks::ExampleMethods", "expect"),
            ("RSpec::Mocks::Matchers::Receive", "expect"),
            ("RSpec::Core::Configuration", "before"),
        ] {
            assert_eq!(written(owner, method), None, "{owner}#{method}");
        }
    }
}
