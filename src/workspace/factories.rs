//! Which class each FactoryBot factory builds, and the RBS that says so: `create(:user)` is a
//! `User`.
//!
//! Text in, facts out, as `workspace/rails/` is. The orchestration (which files, which names the
//! bundle declares, which file hosts the rows) is `knowledge::factories`'.
//!
//! # What a factory builds
//!
//! - **`class:` where a factory names one**, else its parent's (a factory nested in another, or
//!   `parent:`), else the topmost factory's name camelized. An alias is the factory.
//! - **A constant is looked up from where it is written; a String or a Symbol from the top level**,
//!   as `constantize` reads it (`"spree/address"` is `Spree::Address`).
//! - **An `initialize_with` builds what its block returns.** Only `new(…)` is the class; any other,
//!   in the factory, a trait, an ancestor, or the global one, leaves the factory untyped.
//! - **`FactoryBot.register_strategy` replacing a strategy** leaves that strategy undeclared.
//! - **The call's first argument picks the arm** (`types::pick_by_literal`): each factory is one
//!   `(:name, *untyped, **untyped)` arm, tried in order, and a name nothing here reads falls to an
//!   `untyped` catch-all.

use std::collections::{BTreeMap, BTreeSet};

use ruby_prism::{BlockNode, CallNode, ClassNode, DefNode, ModuleNode, Node, Visit, parse};

use crate::generated::{At, Declared, Facts, Namespaces, Owner, Runs, Source, candidates};
use crate::workspace::rails::camelize;

/// The module FactoryBot's strategy methods are defined on, which a project includes.
pub const SYNTAX: &str = "FactoryBot::Syntax::Methods";

/// The class whose instance runs a factory's block, a trait's and a `transient`'s.
pub const PROXY: &str = "FactoryBot::DefinitionProxy";

/// The class whose instance runs a callback's block.
pub const RUNNER: &str = "FactoryBot::SyntaxRunner";

/// The class whose instance (a subclass of it, made per factory) runs an attribute's block.
pub const EVALUATOR: &str = "FactoryBot::Evaluator";

/// The namespace of the class made for each factory's block ([`proxies`]).
pub const FACTORIES: &str = "FactoryBot::Factories";

/// Every constant this module names, for the pass to ask the bundle about.
pub const CONSTANTS: [&str; 6] = [
    "FactoryBot",
    "FactoryBot::Syntax",
    SYNTAX,
    PROXY,
    RUNNER,
    EVALUATOR,
];

/// FactoryBot's strategies, `(name, the strategy it runs, what one built object is wrapped in)`:
/// `create_list` runs `create` and hands back an `Array` of what it built.
const STRATEGIES: [(&str, &str, Wrap); 9] = [
    ("create", "create", Wrap::One),
    ("build", "build", Wrap::One),
    ("build_stubbed", "build_stubbed", Wrap::One),
    ("create_list", "create", Wrap::Array),
    ("build_list", "build", Wrap::Array),
    ("build_stubbed_list", "build_stubbed", Wrap::Array),
    ("create_pair", "create", Wrap::Array),
    ("build_pair", "build", Wrap::Array),
    ("build_stubbed_pair", "build_stubbed", Wrap::Array),
];

/// What `attributes_for` and its list and pair build, whatever the factory: the attributes, by
/// name.
const ATTRIBUTES: [(&str, &str); 3] = [
    ("attributes_for", "::Hash[::Symbol, untyped]"),
    ("attributes_for_list", "::Array[::Hash[::Symbol, untyped]]"),
    ("attributes_for_pair", "::Array[::Hash[::Symbol, untyped]]"),
];

/// The strategy [`ATTRIBUTES`] run.
const ATTRIBUTES_FOR: &str = "attributes_for";

/// How a strategy hands back what it built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wrap {
    One,
    Array,
}

/// One file's factory definitions.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Read {
    /// Every `factory` inside a `FactoryBot.define`, parents before their children.
    pub factories: Vec<Factory>,
    /// Whether an `initialize_with` outside every factory builds anything but `new(…)`: FactoryBot's
    /// global constructor, a global trait's, or one in a `FactoryBot.modify`, which may be any
    /// factory's.
    pub otherwise: bool,
    /// The strategies `FactoryBot.register_strategy` replaces, which build what the project says.
    pub registered: BTreeSet<String>,
    /// For each factory whose name is not a literal, the factory it inherits from (the one it is
    /// written in, or its literal `parent:`), or `None` where a `parent:` names one only Ruby
    /// knows: its callbacks' objects may be any of that factory's.
    pub unnamed: Vec<Option<String>>,
    /// The file it was read from, which the caller fills in: where a factory is written
    /// ([`Classes::places`]).
    pub uri: String,
    /// Whether that file is a gem's, which the caller fills in: its factories answer only for a
    /// name the project does not define ([`classes`]).
    pub gem: bool,
}

/// One FactoryBot `factory`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Factory {
    pub name: String,
    /// `aliases:`, each another name for this factory.
    pub aliases: Vec<String>,
    /// What `class:` says, where it is written.
    pub class: Option<Named>,
    /// `parent:`, by name.
    pub parent: Option<String>,
    /// Whether a `parent:` is written that is not a literal: whose child it is, only Ruby knows.
    pub parent_unknown: bool,
    /// The factory it is written in, by index into [`Read::factories`]: its parent too.
    pub within: Option<usize>,
    /// Whether an `initialize_with` in its block, a trait's included, builds anything but
    /// `new(…)`.
    pub otherwise: bool,
    /// The `module`s and `class`es around the definition, joined.
    pub nesting: String,
    /// The whole `factory` call, and its name.
    pub at: At,
    /// Where each call starts whose block its definition runs (its own `trait`s and
    /// `transient`s included), and what runs it.
    pub blocks: Vec<(u32, Ran)>,
    /// Whether a block in it is handed to `send`, which may call a callback no [`Ran::Callback`]
    /// lists.
    pub sends: bool,
}

/// What runs a block written in a factory's definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ran {
    /// A `trait` or a `transient`: the definition's proxy, as the factory's own block.
    Proxy,
    /// An `after`, a `before` or a `callback`: an object of the syntax runner, handed what was
    /// built.
    Callback(Callback),
    /// An attribute's: the factory's evaluator.
    Attribute,
    /// Anything else (`sequence`, `initialize_with`, `to_create`, …): what runs it differs between
    /// FactoryBot's releases, or is not one class.
    Other,
}

/// One callback: which of the three it is written with, and what its block takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callback {
    /// `after`, `before` or `callback`.
    pub method: String,
    /// The names it is registered under (`after_create`), or `None` where one is not a literal.
    pub names: Option<Vec<String>>,
    /// Whether its block's parameters are at most two plain ones, which FactoryBot hands the object
    /// and the evaluator. Anything else (a third, an optional, a `*rest`) changes what FactoryBot
    /// hands it, by the block's arity.
    pub plain: bool,
}

/// A class an option names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Named {
    /// A constant, as written: Ruby looks it up from where it is written.
    Constant(String),
    /// A String or a Symbol, as `constantize` reads it: from the top level.
    Name(String),
    /// Something only running Ruby knows.
    Unknown,
}

/// Read one file's factory definitions.
#[must_use]
pub fn read_factories(source: &str) -> Read {
    let result = parse(source.as_bytes());
    let mut reader = Reader {
        source,
        read: Read::default(),
        nesting: Vec::new(),
        defining: None,
        within: None,
        proxied: None,
    };
    reader.visit(&result.node());
    reader.read
}

/// Which FactoryBot block the walk is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Defining {
    /// `FactoryBot.define`: each `factory` is a new one.
    Define,
    /// `FactoryBot.modify`: each `factory` names one defined elsewhere, and its options are
    /// ignored.
    Modify,
}

/// [`read_factories`]' walk.
struct Reader<'s> {
    source: &'s str,
    read: Read,
    nesting: Vec<String>,
    defining: Option<Defining>,
    /// The factory whose block the walk is in.
    within: Option<usize>,
    /// The factory whose definition's proxy is `self` where the walk is: in its block, a trait's or
    /// a `transient`'s, and not inside a block one of their calls is handed.
    proxied: Option<usize>,
}

impl<'pr> Visit<'pr> for Reader<'_> {
    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        self.nested(&node.constant_path(), node.body().as_ref());
    }

    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        self.nested(&node.constant_path(), node.body().as_ref());
    }

    fn visit_def_node(&mut self, _node: &DefNode<'pr>) {}

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        let name = node.name().as_slice();
        let bare = node.receiver().is_none();
        let on_factory_bot = node.receiver().is_some_and(|receiver| {
            is_constant(&receiver) && self.spelling(&receiver) == "FactoryBot"
        });
        if on_factory_bot && matches!(name, b"define" | b"modify") {
            let mode = if name == b"define" {
                Defining::Define
            } else {
                Defining::Modify
            };
            self.walk(node, Some(mode), None);
            return;
        }
        if on_factory_bot && name == b"register_strategy" {
            self.registered(node);
            return;
        }
        if bare && name == b"initialize_with" {
            self.constructor(node);
            self.ran(node, Ran::Other);
            return;
        }
        if bare && name == b"factory" && self.defining.is_some() {
            self.factory(node);
            return;
        }
        if bare
            && let Some(index) = self.proxied
            && let Some(ran) = self.dsl(node, index)
        {
            let proxied = ran == Ran::Proxy;
            self.ran(node, ran);
            let (defining, within) = (self.defining, self.within);
            self.visit_arguments(node);
            // Inside a block another object runs, a `factory` or an `initialize_with` is not
            // FactoryBot's.
            let (inner, proxy) = if proxied {
                (defining, Some(index))
            } else {
                (None, None)
            };
            self.walk_as(node, inner, within, proxy);
            return;
        }
        ruby_prism::visit_call_node(self, node);
    }
}

impl Reader<'_> {
    fn nested(&mut self, path: &Node<'_>, body: Option<&Node<'_>>) {
        self.nesting.push(self.spelling(path));
        if let Some(body) = body {
            self.visit(body);
        }
        self.nesting.pop();
    }

    /// Walk a call's block as the given definition's.
    fn walk(&mut self, node: &CallNode<'_>, defining: Option<Defining>, within: Option<usize>) {
        let proxied = within.filter(|_| defining == Some(Defining::Define));
        self.walk_as(node, defining, within, proxied);
    }

    /// Walk a call's block as the given definition's, with the proxy as `self` or not.
    fn walk_as(
        &mut self,
        node: &CallNode<'_>,
        defining: Option<Defining>,
        within: Option<usize>,
        proxied: Option<usize>,
    ) {
        let Some(body) = node
            .block()
            .and_then(|block| block.as_block_node())
            .and_then(|block| block.body())
        else {
            return;
        };
        let outer = (self.defining, self.within, self.proxied);
        (self.defining, self.within, self.proxied) = (defining, within, proxied);
        self.visit(&body);
        (self.defining, self.within, self.proxied) = outer;
    }

    /// A call's arguments, walked as where the call is written.
    fn visit_arguments(&mut self, node: &CallNode<'_>) {
        if let Some(arguments) = node.arguments() {
            self.visit(&arguments.as_node());
        }
    }

    /// What runs the block a call with no receiver hands, where the definition's proxy is `self`:
    /// `None` for a call with no block written.
    ///
    /// The proxy undefines every method but a dozen, so any other name is an attribute's
    /// (`method_missing`), whose block the evaluator runs.
    fn dsl(&mut self, node: &CallNode<'_>, index: usize) -> Option<Ran> {
        let block = node.block()?.as_block_node()?;
        let name = node.name();
        Some(match name.as_slice() {
            b"trait" | b"transient" => Ran::Proxy,
            b"after" | b"before" | b"callback" => Ran::Callback(Callback {
                method: String::from_utf8_lossy(name.as_slice()).into_owned(),
                names: self.callback_names(node),
                plain: plain_parameters(&block),
            }),
            b"add_attribute" => Ran::Attribute,
            b"send" | b"__send__" | b"public_send" => {
                self.read.factories[index].sends = true;
                Ran::Other
            }
            b"sequence"
            | b"to_create"
            | b"association"
            | b"skip_create"
            | b"traits_for_enum"
            | b"ignore"
            | b"method_missing"
            | b"singleton_method_added"
            | b"child_factories"
            | b"__id__"
            | b"nil?"
            | b"object_id"
            | b"extend"
            | b"instance_eval"
            | b"instance_exec"
            | b"initialize"
            | b"block_given?"
            | b"raise"
            | b"caller"
            | b"method" => Ran::Other,
            _ => Ran::Attribute,
        })
    }

    /// The names a callback registers its block under: `after(:create)` is `after_create`,
    /// `callback(:after_stub)` as written.
    fn callback_names(&self, node: &CallNode<'_>) -> Option<Vec<String>> {
        let prefix = match node.name().as_slice() {
            b"after" => "after_",
            b"before" => "before_",
            _ => "",
        };
        let arguments = arguments_of(node);
        if arguments.is_empty() {
            return None;
        }
        arguments
            .iter()
            .map(|argument| self.literal(argument).map(|name| format!("{prefix}{name}")))
            .collect()
    }

    /// Record where a call whose block the definition runs starts.
    fn ran(&mut self, node: &CallNode<'_>, ran: Ran) {
        let Some(index) = self.proxied else {
            return;
        };
        if node
            .block()
            .and_then(|block| block.as_block_node())
            .is_some()
        {
            self.read.factories[index]
                .blocks
                .push((node.location().start_offset() as u32, ran));
        }
    }

    /// One `factory :name, …` and the factories written in its block. In a `modify` it defines
    /// nothing, and a constructor written there counts for every factory.
    fn factory(&mut self, node: &CallNode<'_>) {
        if self.defining == Some(Defining::Modify) {
            self.walk(node, self.defining, None);
            return;
        }
        let arguments = arguments_of(node);
        let options = self.options(&arguments);
        let Some(name) = arguments.first().and_then(|first| self.literal(first)) else {
            // A factory only Ruby names is no answer of its own, but it may inherit callbacks, so
            // the factory above it must not be read as having every child it has.
            let parent = match options.get("parent") {
                Some(value) => self.literal(value),
                None => self
                    .within
                    .map(|index| self.read.factories[index].name.clone()),
            };
            if options.contains_key("parent") || self.within.is_some() {
                self.read.unnamed.push(parent);
            }
            self.walk_as(node, self.defining, self.within, self.proxied);
            return;
        };
        let whole = node.location();
        let named = arguments[0].location();
        // A Symbol's name without its colon, a String's without its quotes.
        let named = arguments[0]
            .as_symbol_node()
            .and_then(|symbol| symbol.value_loc())
            .or_else(|| {
                arguments[0]
                    .as_string_node()
                    .map(|string| string.content_loc())
            })
            .unwrap_or(named);
        self.read.factories.push(Factory {
            at: (
                (whole.start_offset() as u32, whole.end_offset() as u32),
                (named.start_offset() as u32, named.end_offset() as u32),
            ),
            name,
            aliases: self.aliases(&options),
            class: options.get("class").map(|value| self.named(value)),
            parent: options.get("parent").and_then(|value| self.literal(value)),
            parent_unknown: options
                .get("parent")
                .is_some_and(|value| self.literal(value).is_none()),
            within: self.within,
            otherwise: false,
            nesting: self.nesting.join("::"),
            blocks: Vec::new(),
            sends: false,
        });
        let index = self.read.factories.len() - 1;
        self.walk(node, self.defining, Some(index));
    }

    /// An `initialize_with`: whether its block builds the factory's class (FactoryBot runs it with
    /// `new` meaning the class), and whose it is.
    fn constructor(&mut self, node: &CallNode<'_>) {
        let Some(block) = node.block().and_then(|block| block.as_block_node()) else {
            return;
        };
        if self.defining.is_none() || returns_new(&block) {
            return;
        }
        match (self.defining, self.within) {
            (Some(Defining::Define), Some(index)) => self.read.factories[index].otherwise = true,
            _ => self.read.otherwise = true,
        }
    }

    /// `FactoryBot.register_strategy(:name, …)`: a name not written as a literal may be any.
    fn registered(&mut self, node: &CallNode<'_>) {
        let arguments = arguments_of(node);
        match arguments.first().and_then(|first| self.literal(first)) {
            Some(name) => {
                self.read.registered.insert(name);
            }
            None => self.read.registered.extend(
                STRATEGIES
                    .iter()
                    .map(|(_, strategy, _)| (*strategy).to_owned())
                    .chain([ATTRIBUTES_FOR.to_owned()]),
            ),
        }
    }

    /// The `key: value` options after the first argument.
    fn options<'n>(&self, arguments: &[Node<'n>]) -> BTreeMap<String, Node<'n>> {
        let mut options = BTreeMap::new();
        for argument in arguments.iter().skip(1) {
            let Some(hash) = argument.as_keyword_hash_node() else {
                continue;
            };
            for element in hash.elements().iter() {
                let Some(pair) = element.as_assoc_node() else {
                    continue;
                };
                if let Some(key) = pair.key().as_symbol_node() {
                    options.insert(
                        String::from_utf8_lossy(key.unescaped()).into_owned(),
                        pair.value(),
                    );
                }
            }
        }
        options
    }

    /// `aliases:`, each written as a literal.
    fn aliases(&self, options: &BTreeMap<String, Node<'_>>) -> Vec<String> {
        options
            .get("aliases")
            .and_then(Node::as_array_node)
            .map(|array| {
                array
                    .elements()
                    .iter()
                    .filter_map(|element| self.literal(&element))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The class an option names: a constant as written, a String or a Symbol as `constantize`
    /// reads it (`"spree/address"` is `Spree::Address`).
    fn named(&self, value: &Node<'_>) -> Named {
        if is_constant(value) {
            return Named::Constant(self.spelling(value));
        }
        match self.literal(value).and_then(|text| constantized(&text)) {
            Some(name) => Named::Name(name),
            None => Named::Unknown,
        }
    }

    fn literal(&self, node: &Node<'_>) -> Option<String> {
        let bytes = if let Some(string) = node.as_string_node() {
            string.unescaped().to_vec()
        } else {
            node.as_symbol_node()?.unescaped().to_vec()
        };
        String::from_utf8(bytes).ok()
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

/// A call's arguments.
fn arguments_of<'pr>(node: &CallNode<'pr>) -> Vec<Node<'pr>> {
    node.arguments()
        .map(|arguments| arguments.arguments().iter().collect())
        .unwrap_or_default()
}

/// Whether a node is a constant, bare or a path.
fn is_constant(node: &Node<'_>) -> bool {
    node.as_constant_read_node().is_some() || node.as_constant_path_node().is_some()
}

/// Whether a callback's block takes at most two plain parameters, which FactoryBot hands the object
/// and the evaluator by the block's arity (`Callback#run`): one is the object, two the object and
/// the evaluator. `_1`, `_2` and `it` count as written.
fn plain_parameters(block: &BlockNode<'_>) -> bool {
    let Some(parameters) = block.parameters() else {
        return true;
    };
    if let Some(numbered) = parameters.as_numbered_parameters_node() {
        return numbered.maximum() <= 2;
    }
    if parameters.as_it_parameters_node().is_some() {
        return true;
    }
    let Some(written) = parameters
        .as_block_parameters_node()
        .and_then(|written| written.parameters())
    else {
        return true;
    };
    written.requireds().iter().count() <= 2
        && written
            .requireds()
            .iter()
            .all(|required| required.as_required_parameter_node().is_some())
        && written.optionals().iter().next().is_none()
        // A parameter after a rest or an optional is past one of them already.
        && written.rest().is_none()
        && written.keywords().iter().next().is_none()
        && written.keyword_rest().is_none()
        && written.block().is_none()
}

/// Whether a constructor block ends in `new(…)`, sent to nothing: the factory's class.
fn returns_new(block: &BlockNode<'_>) -> bool {
    let last = block
        .body()
        .and_then(|body| body.as_statements_node())
        .and_then(|statements| statements.body().iter().last());
    last.as_ref()
        .and_then(Node::as_call_node)
        .is_some_and(|call| call.receiver().is_none() && call.name().as_slice() == b"new")
}

/// A name as ActiveSupport's `camelize` then `constantize` read it: each `/` a `::`, each segment
/// camelized. `None` where a segment spells no constant.
fn constantized(text: &str) -> Option<String> {
    let segments: Option<Vec<String>> = text
        .trim_start_matches("::")
        .split('/')
        .map(|segment| {
            if segment.contains("::") {
                Some(segment.to_owned())
            } else {
                camelize(segment)
            }
        })
        .collect();
    Some(segments?.join("::"))
}

/// Which class each factory builds, where it can be said.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Classes {
    /// FactoryBot factories (aliases included), by name: the class, or `None` where it cannot be
    /// said.
    pub factories: BTreeMap<String, Option<String>>,
    /// The strategies a project replaces ([`Read::registered`]).
    pub registered: BTreeSet<String>,
    /// Where each factory (aliases included) is written: the file and the `factory` call. Not a
    /// name two definitions write.
    pub places: BTreeMap<String, (String, At)>,
    /// Each factory's parent (aliases included), by name: the one it is written in, or its
    /// `parent:`.
    pub parents: BTreeMap<String, String>,
    /// Which read and which of its factories each name is (aliases included). Not a name two
    /// definitions write, nor a gem's the project writes too.
    pub defined: BTreeMap<String, (usize, usize)>,
}

/// What every file's definitions build. `resolve` looks a constant written inside a nesting up,
/// answering the name the project or the bundle declares.
///
/// - **A name two definitions write builds nothing here**: FactoryBot refuses the second, and which
///   file loads first is not known.
/// - **A gem's factory answers only where the project defines no factory of that name.** A gem's
///   factories load only when the project requires them, and a project that loaded one beside its
///   own of the same name would not start, so a working project that writes the name is not
///   loading the gem's.
/// - **A parent chain is followed to its end**, a loop or a missing parent answering nothing.
/// - **A constructor that builds anything but the class, anywhere in the chain, answers
///   nothing**: a trait's may be the one a call asks for, and a child inherits its parent's.
#[must_use]
pub fn classes(reads: &[Read], resolve: &dyn Fn(&str, &str) -> Option<String>) -> Classes {
    let otherwise = reads.iter().any(|read| read.otherwise);
    let mut factories: BTreeMap<String, &Factory> = BTreeMap::new();
    let mut defined: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut twice: BTreeSet<String> = BTreeSet::new();
    let mut places: BTreeMap<String, (String, At)> = BTreeMap::new();
    // Each factory's parent, by every name it has.
    let mut parents: BTreeMap<String, String> = BTreeMap::new();
    let own: BTreeSet<&String> = reads
        .iter()
        .filter(|read| !read.gem)
        .flat_map(|read| {
            read.factories
                .iter()
                .flat_map(|factory| std::iter::once(&factory.name).chain(&factory.aliases))
        })
        .collect();
    for (read_at, read) in reads.iter().enumerate() {
        for (factory_at, factory) in read.factories.iter().enumerate() {
            let parent = factory.parent.clone().or_else(|| {
                factory
                    .within
                    .map(|index| read.factories[index].name.clone())
            });
            for name in std::iter::once(&factory.name).chain(&factory.aliases) {
                if read.gem && own.contains(name) {
                    continue;
                }
                if factories.insert(name.clone(), factory).is_some() {
                    twice.insert(name.clone());
                }
                defined.insert(name.clone(), (read_at, factory_at));
                places.insert(name.clone(), (read.uri.clone(), factory.at));
                if let Some(parent) = &parent {
                    parents.insert(name.clone(), parent.clone());
                }
            }
        }
    }
    let mut classes = Classes {
        registered: reads
            .iter()
            .flat_map(|read| read.registered.iter().cloned())
            .collect(),
        ..Classes::default()
    };
    classes.places = places;
    classes.places.retain(|name, _| !twice.contains(name));
    defined.retain(|name, _| !twice.contains(name));
    classes.defined = defined;
    classes.parents = parents.clone();
    for name in factories.keys() {
        let answer = if otherwise {
            None
        } else {
            factory_class(name, &factories, &parents, &twice, resolve)
        };
        classes.factories.insert(name.clone(), answer);
    }
    classes
}

/// How far a parent chain is followed: a guard against a loop, far above any real chain.
const PARENTS: usize = 16;

/// FactoryBot's `class_name`: the nearest `class:` up the chain, else the topmost factory's name.
fn factory_class(
    name: &str,
    factories: &BTreeMap<String, &Factory>,
    parents: &BTreeMap<String, String>,
    twice: &BTreeSet<String>,
    resolve: &dyn Fn(&str, &str) -> Option<String>,
) -> Option<String> {
    let mut chain: Vec<&Factory> = Vec::new();
    let mut at = Some(name);
    while let Some(current) = at {
        if chain.len() > PARENTS || twice.contains(current) {
            return None;
        }
        let factory = *factories.get(current)?;
        // A `parent:` only Ruby names may hold the `class:` that decides.
        if factory.otherwise || factory.parent_unknown {
            return None;
        }
        chain.push(factory);
        at = parents.get(current).map(String::as_str);
    }
    for factory in &chain {
        if let Some(named) = &factory.class {
            return class_of(&factory.nesting, named, resolve);
        }
    }
    resolve("", &constantized(&chain.last()?.name)?)
}

/// The class an option names, looked up as Ruby would: a constant from where it is written, a
/// name from the top level.
fn class_of(
    nesting: &str,
    named: &Named,
    resolve: &dyn Fn(&str, &str) -> Option<String>,
) -> Option<String> {
    match named {
        Named::Constant(written) => resolve(nesting, written),
        Named::Name(name) => resolve("", name),
        Named::Unknown => None,
    }
}

/// Every name a definition may mean, for the one question the caller asks the graph: each
/// constant an option writes, looked up from its nesting, and each name camelized.
#[must_use]
pub fn wanted_names(reads: &[Read]) -> BTreeSet<String> {
    let mut wanted = BTreeSet::new();
    let mut add = |nesting: &str, named: Option<&Named>| match named {
        Some(Named::Constant(written)) => wanted.extend(candidates_of(nesting, written)),
        Some(Named::Name(name)) => {
            wanted.insert(name.clone());
        }
        Some(Named::Unknown) | None => {}
    };
    let spelled = |name: &str| constantized(name).map(Named::Name);
    for read in reads {
        for factory in &read.factories {
            add(&factory.nesting, factory.class.as_ref());
            add("", spelled(&factory.name).as_ref());
        }
    }
    wanted
}

/// The names a constant written inside `nesting` could mean, innermost first.
#[must_use]
pub fn candidates_of(nesting: &str, written: &str) -> Vec<String> {
    if nesting.is_empty() {
        vec![written.to_owned()]
    } else {
        candidates(nesting, written)
    }
}

/// The strategies' rows, on [`SYNTAX`] where the bundle declares it.
#[must_use]
pub fn rows(built: &Classes, namespaces: &Namespaces) -> Vec<(&'static str, Facts)> {
    let mut hosted = Vec::new();
    if namespaces.declares(SYNTAX) && !built.factories.is_empty() {
        let owner = if namespaces.opens(SYNTAX) {
            Owner::Module(SYNTAX.to_owned())
        } else {
            Owner::Instance(SYNTAX.to_owned())
        };
        let mut facts = Facts::default();
        for (name, strategy, wrap) in STRATEGIES {
            if built.registered.contains(strategy) {
                continue;
            }
            facts.declare(picked(&owner, name, &built.factories, wrap));
        }
        for (name, returns) in ATTRIBUTES {
            if built.registered.contains(ATTRIBUTES_FOR) {
                continue;
            }
            facts.declare(Declared {
                owner: owner.clone(),
                name: name.to_owned(),
                returns: returns.to_owned(),
                parameters: format!(
                    "({}) ?{{ (untyped) -> untyped }}",
                    parameters(name, "untyped")
                ),
                because: "FactoryBot's `attributes_for`: the factory's attributes, by name."
                    .to_owned(),
                at: None,
                from: Source::Interface,
                overloads: Vec::new(),
                private: false,
            });
        }
        hosted.push((SYNTAX, facts));
    }
    hosted
}

/// What each factory's blocks run as, and what its callbacks are handed: per definition file, by
/// its index in `reads`, the facts written beside it.
///
/// - **A factory's block, its `trait`s' and its `transient`s' run on its definition's proxy**:
///   one class per factory, `FactoryBot::Factories::<Name>`, below FactoryBot's
///   `DefinitionProxy`, so a callback there can say what that factory builds.
/// - **A callback's block runs on a `SyntaxRunner`, an attribute's on the evaluator**; any other
///   block (`sequence`, `initialize_with`, `to_create`) is refused: what runs it differs between
///   releases. A factory this pass does not answer for (a name two definitions write, a gem's the
///   project writes too) is left as it was.
/// - **`after`, `before` and `callback` hand their block what was built** ([`callback_row`]):
///   every class the factory and each factory inheriting from it build, since a child runs its
///   parent's callbacks and traits. Refused where any of those is not known, where a factory only
///   Ruby names may inherit it, where a block is handed to `send`, or where a project registers a
///   strategy of its own, which may hand a callback anything.
#[must_use]
pub fn proxies(reads: &[Read], built: &Classes, namespaces: &Namespaces) -> Vec<(usize, Facts)> {
    if !namespaces.declares(PROXY) {
        return Vec::new();
    }
    // Any factory may be the parent of one whose `parent:` only Ruby names.
    let anyone_s = reads.iter().any(|read| {
        read.unnamed.iter().any(Option::is_none)
            || read.factories.iter().any(|factory| factory.parent_unknown)
    }) || !built.registered.is_empty();
    // A factory only Ruby names inherits from these, and from everything above them.
    let mut adopting: BTreeSet<&str> = BTreeSet::new();
    for parent in reads.iter().flat_map(|read| read.unnamed.iter().flatten()) {
        let mut at = Some(parent.as_str());
        while let Some(name) = at {
            if !adopting.insert(name) {
                break;
            }
            at = built.parents.get(name).map(String::as_str);
        }
    }
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (child, parent) in &built.parents {
        children
            .entry(parent.as_str())
            .or_default()
            .push(child.as_str());
    }
    let ran = |declared: &str| {
        if namespaces.declares(declared) {
            Runs::Instance(declared.to_owned())
        } else {
            Runs::Refused
        }
    };
    let mut taken: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for (read_at, read) in reads.iter().enumerate() {
        let mut facts = Facts::default();
        facts.whole();
        for (factory_at, factory) in read.factories.iter().enumerate() {
            if built.defined.get(&factory.name) != Some(&(read_at, factory_at)) {
                continue;
            }
            let Some(segment) = segment_of(&factory.name, &mut taken) else {
                continue;
            };
            let class = format!("{FACTORIES}::{segment}");
            let owner = Owner::Instance(class.clone());
            facts.namespace(Owner::Module(FACTORIES.to_owned()), None);
            facts.inherits(owner.clone(), format!("::{PROXY}"));
            facts.runs(owner.clone(), factory.at.0.0, Runs::Instance(class.clone()));
            for (call, block) in &factory.blocks {
                let runs = match block {
                    Ran::Proxy => Runs::Instance(class.clone()),
                    Ran::Callback(_) => ran(RUNNER),
                    Ran::Attribute => ran(EVALUATOR),
                    Ran::Other => Runs::Refused,
                };
                facts.runs(owner.clone(), *call, runs);
            }
            let refused = anyone_s
                || factory.sends
                || std::iter::once(&factory.name)
                    .chain(&factory.aliases)
                    .any(|name| adopting.contains(name.as_str()));
            let objects = if refused {
                None
            } else {
                objects_of(factory, built, &children)
            };
            for method in ["after", "before", "callback"] {
                if let Some(row) = objects
                    .as_ref()
                    .and_then(|objects| callback_row(&owner, method, objects, &factory.blocks))
                {
                    facts.declare(row);
                }
            }
        }
        if !facts.is_empty() {
            out.push((read_at, facts));
        }
    }
    out
}

/// The constant a factory's class is named by below [`FACTORIES`]: its name camelized, suffixed
/// as RSpec suffixes a group where two names camelize alike. `None` for a name that is no bare
/// RBS symbol.
fn segment_of(name: &str, taken: &mut BTreeSet<String>) -> Option<String> {
    if !symbol_spelled(name) {
        return None;
    }
    let base =
        camelize(name).filter(|base| base.starts_with(|first: char| first.is_ascii_uppercase()))?;
    let mut segment = base.clone();
    let mut next = 2;
    while !taken.insert(segment.clone()) {
        segment = format!("{base}_{next}");
        next += 1;
    }
    Some(segment)
}

/// Every class a factory's callbacks may be handed an object of: its own and that of each
/// factory inheriting from it, by `parent:` or by being written in it, sorted. `None` where any
/// of them is not known.
fn objects_of(
    factory: &Factory,
    built: &Classes,
    children: &BTreeMap<&str, Vec<&str>>,
) -> Option<Vec<String>> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut next: Vec<&str> = std::iter::once(&factory.name)
        .chain(&factory.aliases)
        .map(String::as_str)
        .collect();
    let mut objects: BTreeSet<String> = BTreeSet::new();
    while let Some(name) = next.pop() {
        if !seen.insert(name) {
            continue;
        }
        objects.insert(built.factories.get(name)?.clone()?);
        next.extend(children.get(name).into_iter().flatten());
    }
    Some(objects.into_iter().collect())
}

/// The row that says what a callback's block is handed, from every callback of that method the
/// factory's definition writes: `after(:build)`, `after(:create)`, `after(:stub)` and
/// `before(:create)` the object built, `before(:build)` and `before(:all)` a `nil` (FactoryBot
/// 6.6 notifies them before there is one; earlier releases never run them). `None` where one is
/// registered under another name (`after(:all)` is handed what the strategy returns, a `Hash` for
/// `attributes_for`), its names are not literals, or its block's parameters are not plain.
fn callback_row(
    owner: &Owner,
    method: &str,
    objects: &[String],
    blocks: &[(u32, Ran)],
) -> Option<Declared> {
    let (mut object, mut nil) = (false, false);
    let mut written = false;
    for (_, block) in blocks {
        let Ran::Callback(callback) = block else {
            continue;
        };
        if callback.method != method {
            continue;
        }
        written = true;
        if !callback.plain {
            return None;
        }
        for name in callback.names.as_ref()? {
            match name.as_str() {
                "after_build" | "after_create" | "after_stub" | "before_create" => object = true,
                "before_build" | "before_all" => nil = true,
                _ => return None,
            }
        }
    }
    if !written {
        return None;
    }
    let union = objects
        .iter()
        .map(|class| format!("::{class}"))
        .collect::<Vec<_>>()
        .join(" | ");
    let handed = match (object, nil) {
        (true, false) => union,
        (true, true) if objects.len() == 1 => format!("{union}?"),
        (true, true) => format!("({union})?"),
        // A block handed only `nil` says nothing a reader needs.
        (false, _) => return None,
    };
    Some(Declared {
        owner: owner.clone(),
        name: method.to_owned(),
        returns: "untyped".to_owned(),
        parameters: format!("(*untyped names) {{ ({handed} object, untyped evaluator) -> void }}"),
        because: format!(
            "FactoryBot's `{method}`: its block is handed what this factory builds, then the \
             evaluator."
        ),
        at: None,
        from: Source::Interface,
        overloads: Vec::new(),
        private: false,
    })
}

/// One strategy with an arm per factory that says its class, in name order, then a catch-all.
fn picked(
    owner: &Owner,
    method: &str,
    classes: &BTreeMap<String, Option<String>>,
    wrap: Wrap,
) -> Declared {
    let mut arms: Vec<(String, String)> = classes
        .iter()
        .filter(|(name, _)| symbol_spelled(name))
        .filter_map(|(name, class)| {
            let class = class.as_ref()?;
            let returns = match wrap {
                Wrap::One => format!("::{class}"),
                Wrap::Array => format!("::Array[::{class}]"),
            };
            Some((
                format!(
                    "({}) ?{{ (untyped) -> untyped }}",
                    parameters(method, &format!(":{name}"))
                ),
                returns,
            ))
        })
        .collect();
    // `untyped` for a list too, though it is an `Array`: arms that all say `Array` agree on the head
    // before the call's symbol is read, and `create_list(:user, 3)` would lose its `User`.
    arms.push((
        format!(
            "({}) ?{{ (untyped) -> untyped }}",
            parameters(method, "untyped")
        ),
        "untyped".to_owned(),
    ));
    let (parameters, returns) = arms.remove(0);
    Declared {
        owner: owner.clone(),
        name: method.to_owned(),
        returns,
        parameters,
        because: format!(
            "FactoryBot's `{method}`: the class the factory its first argument names builds."
        ),
        at: None,
        from: Source::Interface,
        overloads: arms,
        private: false,
    }
}

/// A strategy's parameters, named for a card, the factory's written `factory`: `create(factory,
/// *args, **kwargs)`, and a list's count after it, `create_list(factory, amount, *args,
/// **kwargs)`. FactoryBot's own `define_method` blocks take `|name, amount, *traits_and_overrides|`.
///
/// `amount` is `untyped`, so an arm still names its symbol where every later positional takes
/// anything (`types::pick_by_literal`), and `create_list(:user, 3)` keeps its `User`.
fn parameters(method: &str, factory: &str) -> String {
    let amount = if method.ends_with("_list") {
        "untyped amount, "
    } else {
        ""
    };
    format!("{factory} factory, {amount}*untyped args, **untyped kwargs")
}

/// Whether a name can be written as a bare RBS symbol literal.
fn symbol_spelled(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|rest| rest.is_ascii_alphanumeric() || rest == '_')
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::{declaring, declaring_kinds};

    /// A resolver over the names a fixture's project declares.
    fn declared(names: &'static [&'static str]) -> impl Fn(&str, &str) -> Option<String> {
        move |nesting, written| {
            candidates_of(nesting, written)
                .into_iter()
                .find(|name| names.contains(&name.as_str()))
        }
    }

    /// Every factory's class, as `name=Class` or `name=-`.
    fn built(reads: &[Read], names: &'static [&'static str]) -> Vec<String> {
        classes(reads, &declared(names))
            .factories
            .iter()
            .map(|(name, class)| format!("{name}={}", class.as_deref().unwrap_or("-")))
            .collect()
    }

    #[test]
    fn a_definition_file_is_read_as_factory_bot_evaluates_it() {
        let read = read_factories(
            r#"
module Shop
  FactoryBot.define do
    factory :user, class: Account, aliases: [:author, "writer", other] do
      initialize_with { new(**attributes) }
      factory :admin do
        trait :odd do
          initialize_with { Account.find_or_create_by(name: name) }
        end
      end
    end
    factory "order", class: "shop/order", parent: :user
    factory :line, class: :line_item
    factory :dynamic, class: klass
    factory name_of(:x)
    factory :empty do
      initialize_with {}
    end
    factory :bare do
      initialize_with
    end
    factory :splat, **options
    factory :hashed, "class" => Account
    factory :positional, "extra"
    factory :received do
      initialize_with { Account.new }
    end
  end
  FactoryBot.lint
  def helper = factory(:ignored)
  class Empty; end
end
factory :outside
initialize_with { outside }
"#,
        );
        let shapes: Vec<String> = read
            .factories
            .iter()
            .map(|factory| {
                format!(
                    "{} {:?} {:?} {:?} {:?} {} [{}]",
                    factory.name,
                    factory.aliases,
                    factory.class,
                    factory.parent,
                    factory.within,
                    factory.otherwise,
                    factory.nesting
                )
            })
            .collect();
        assert_eq!(
            shapes,
            [
                r#"user ["author", "writer"] Some(Constant("Account")) None None false [Shop]"#,
                "admin [] None None Some(0) true [Shop]",
                r#"order [] Some(Name("Shop::Order")) Some("user") None false [Shop]"#,
                r#"line [] Some(Name("LineItem")) None None false [Shop]"#,
                "dynamic [] Some(Unknown) None None false [Shop]",
                "empty [] None None None true [Shop]",
                "bare [] None None None false [Shop]",
                "splat [] None None None false [Shop]",
                "hashed [] None None None false [Shop]",
                "positional [] None None None false [Shop]",
                "received [] None None None true [Shop]",
            ]
        );
        assert!(
            !read.otherwise,
            "an `initialize_with` outside FactoryBot is none of its"
        );
        assert!(read.registered.is_empty());
    }

    #[test]
    fn a_definition_s_blocks_are_filed_with_what_runs_them() {
        let source = r#"
FactoryBot.define do
  trait :global do
    after(:create) { |x| x }
  end
  factory :user do
    name { "x" }
    add_attribute(:email) { "e" }
    loop do end
    sequence(:n) { |i| i }
    initialize_with { new }
    to_create(&:save!)
    association :team
    send(:after, :create) { |x| x }
    [1].each { trait :inner do end }
    trait :admin do
      after(:build, :create) { |user, evaluator| user }
      transient do
        count { 1 }
      end
    end
    before(name) { |user| user }
    callback(:after_stub) { _1 }
    after(:create) { it }
    after(:create) { |a, b, c| a }
    after(:create) { |(a, b)| a }
    after(:create) { |a = 1| a }
    after(:create) { |*a| a }
    after(:create) { |a, *b, c| a }
    after(:create) { |a, k:| a }
    after(:create) { |a, **k| a }
    after(:create) { |a, &b| a }
    after(:create) { |;local| local }
    after(:create) { _3 }
    after { |x| x }
    after(:create) do
      factory :not_one
      initialize_with { other }
    end
  end
end
"#;
        let read = read_factories(source);
        let names: Vec<&str> = read
            .factories
            .iter()
            .map(|factory| factory.name.as_str())
            .collect();
        assert_eq!(names, ["user"], "a callback's block defines no factory");
        assert!(
            !read.otherwise,
            "a callback's `initialize_with` is not FactoryBot's"
        );
        let user = &read.factories[0];
        assert!(user.sends);
        let filed: Vec<String> = user
            .blocks
            .iter()
            .map(|(at, ran)| {
                let line = source[*at as usize..].lines().next().unwrap_or_default();
                let ran = match ran {
                    Ran::Callback(callback) => format!(
                        "{} {:?} {}",
                        callback.method,
                        callback.names,
                        if callback.plain { "plain" } else { "-" }
                    ),
                    other => format!("{other:?}"),
                };
                format!("{line} => {ran}")
            })
            .collect();
        assert_eq!(
            filed,
            [
                r#"name { "x" } => Attribute"#,
                r#"add_attribute(:email) { "e" } => Attribute"#,
                "loop do end => Attribute",
                "sequence(:n) { |i| i } => Other",
                "initialize_with { new } => Other",
                "send(:after, :create) { |x| x } => Other",
                "trait :inner do end } => Proxy",
                "trait :admin do => Proxy",
                r#"after(:build, :create) { |user, evaluator| user } => after Some(["after_build", "after_create"]) plain"#,
                "transient do => Proxy",
                "count { 1 } => Attribute",
                "before(name) { |user| user } => before None plain",
                r#"callback(:after_stub) { _1 } => callback Some(["after_stub"]) plain"#,
                r#"after(:create) { it } => after Some(["after_create"]) plain"#,
                r#"after(:create) { |a, b, c| a } => after Some(["after_create"]) -"#,
                r#"after(:create) { |(a, b)| a } => after Some(["after_create"]) -"#,
                r#"after(:create) { |a = 1| a } => after Some(["after_create"]) -"#,
                r#"after(:create) { |*a| a } => after Some(["after_create"]) -"#,
                r#"after(:create) { |a, *b, c| a } => after Some(["after_create"]) -"#,
                r#"after(:create) { |a, k:| a } => after Some(["after_create"]) -"#,
                r#"after(:create) { |a, **k| a } => after Some(["after_create"]) -"#,
                r#"after(:create) { |a, &b| a } => after Some(["after_create"]) -"#,
                r#"after(:create) { |;local| local } => after Some(["after_create"]) plain"#,
                r#"after(:create) { _3 } => after Some(["after_create"]) -"#,
                "after { |x| x } => after None plain",
                r#"after(:create) do => after Some(["after_create"]) plain"#,
            ]
        );
    }

    #[test]
    fn a_factory_only_ruby_names_is_filed_under_the_one_it_inherits_from() {
        let read = read_factories(
            "FactoryBot.define do\n  factory :user do\n    factory name_of(:x) do\n      \
             after(:create) { |x| x }\n    end\n  end\n  factory dynamic, parent: :post\n  \
             factory dynamic, parent: other\n  factory dynamic\n  factory :child, parent: someone\nend\n",
        );
        assert_eq!(
            read.unnamed,
            [Some("user".to_owned()), Some("post".to_owned()), None]
        );
        let user = &read.factories[0];
        assert_eq!(user.blocks.len(), 1, "its callback is filed under `user`");
        let child = &read.factories[1];
        assert!(child.parent_unknown);
        assert_eq!(child.parent, None);
        // Whose child it is decides its class, so none is said.
        assert_eq!(built(&[read], &["Child"]), ["child=-", "user=-"]);
    }

    /// What [`proxies`] writes for `reads`, as RBS and the blocks' runs by their call's first line.
    fn proxied(reads: &[Read], names: &'static [&'static str], bundle: &Namespaces) -> String {
        let built = classes(reads, &declared(names));
        proxies(reads, &built, bundle)
            .iter()
            .map(|(index, facts)| {
                let rendered = facts.render(bundle);
                let source = &reads[*index].uri;
                let runs: Vec<String> = rendered
                    .ran
                    .iter()
                    .map(|(at, runs)| {
                        let line = source[*at as usize..].lines().next().unwrap_or_default();
                        format!("{line} => {runs:?}\n")
                    })
                    .collect();
                format!("{}{}", rendered.rbs, runs.concat())
            })
            .collect()
    }

    /// A definition file read with its text kept where its URI would be, for [`proxied`].
    fn kept(source: &str) -> Read {
        let mut read = read_factories(source);
        read.uri = source.to_owned();
        read
    }

    #[test]
    fn each_factory_s_blocks_run_on_its_own_proxy_and_its_callbacks_are_handed_what_it_builds() {
        let source = "\
FactoryBot.define do
  factory :user do
    name { 1 }
    sequence(:n) { |i| i }
    after(:create) { |user, evaluator| user }
    trait :admin do
      after(:build) { |user| user }
    end
    before(:build, :create) { |user| user }
    callback(:after_stub) { |user| user }
    factory :admin, class: Admin
  end
  factory :post do
    before(:create) { |post| post }
  end
  factory :a_b, class: Post
  factory :aB, class: Post
  factory :\"odd name\", class: Post
end
";
        let bundle = declaring(&["FactoryBot", PROXY, RUNNER, EVALUATOR]);
        let rbs = proxied(&[kept(source)], &["User", "Admin", "Post"], &bundle);
        for expected in [
            "module FactoryBot\nmodule Factories\nend\nend\n",
            "class FactoryBot::Factories::User < ::FactoryBot::DefinitionProxy\n",
            "def after: (*untyped names) { (::Admin | ::User object, untyped evaluator) -> void } \
             -> untyped\n",
            "def before: (*untyped names) { ((::Admin | ::User)? object, untyped evaluator) -> void } \
             -> untyped\n",
            "def callback: (*untyped names) { (::Admin | ::User object, untyped evaluator) -> void } \
             -> untyped\n",
            "class FactoryBot::Factories::Admin < ::FactoryBot::DefinitionProxy\n",
            "def before: (*untyped names) { (::Post object, untyped evaluator) -> void } -> untyped\n",
            // Two names that camelize alike, the second suffixed.
            "class FactoryBot::Factories::AB < ::FactoryBot::DefinitionProxy\n",
            "class FactoryBot::Factories::AB_2 < ::FactoryBot::DefinitionProxy\n",
            // The factory's own block, a trait's, a callback's, an attribute's, and the rest.
            "factory :user do => Instance(\"FactoryBot::Factories::User\")\n",
            "trait :admin do => Instance(\"FactoryBot::Factories::User\")\n",
            "after(:create) { |user, evaluator| user } => Instance(\"FactoryBot::SyntaxRunner\")\n",
            "name { 1 } => Instance(\"FactoryBot::Evaluator\")\n",
            "sequence(:n) { |i| i } => Refused\n",
        ] {
            assert!(rbs.contains(expected), "{expected}\n---\n{rbs}");
        }
        assert!(!rbs.contains("odd name") && !rbs.contains("Odd"), "{rbs}");
        // `admin` writes no callback of its own.
        let admin = rbs
            .split("class FactoryBot::Factories::Admin")
            .nth(1)
            .and_then(|rest| rest.split("\nend\n").next())
            .unwrap_or_default();
        assert!(!admin.contains("def "), "{admin}");
        // Without the runner and the evaluator, their blocks are refused.
        let bare = declaring(&["FactoryBot", PROXY]);
        let rbs = proxied(&[kept(source)], &["User", "Admin", "Post"], &bare);
        assert!(rbs.contains("name { 1 } => Refused\n"), "{rbs}");
        assert!(
            rbs.contains("after(:create) { |user, evaluator| user } => Refused\n"),
            "{rbs}"
        );
        // Without the proxy, nothing.
        assert_eq!(
            proxied(
                &[kept(source)],
                &["User"],
                &declaring(&["FactoryBot", RUNNER])
            ),
            ""
        );
    }

    #[test]
    fn a_callback_is_handed_nothing_said_where_what_runs_it_may_be_anything() {
        let bundle = declaring(&["FactoryBot", PROXY, RUNNER, EVALUATOR]);
        let typed = |sources: &[&str]| {
            let reads: Vec<Read> = sources.iter().map(|source| kept(source)).collect();
            let rbs = proxied(&reads, &["User", "Admin", "Post"], &bundle);
            ["after", "before", "callback"]
                .into_iter()
                .filter(|method| rbs.contains(&format!("def {method}:")))
                .collect::<Vec<_>>()
        };
        let user = "FactoryBot.define do\n  factory :user do\n    after(:create) { |user| user }\n  end\nend\n";
        assert_eq!(typed(&[user]), ["after"]);
        // A child only Ruby names, below `user` or anywhere.
        for other in [
            "FactoryBot.define do\n  factory dynamic, parent: :user\n  factory again, parent: :user\nend\n",
            "FactoryBot.define do\n  factory dynamic, parent: anyone\nend\n",
            "FactoryBot.define do\n  factory :admin, parent: anyone\nend\n",
            "FactoryBot.register_strategy(:json, Json)\n",
        ] {
            assert!(typed(&[user, other]).is_empty(), "{other}");
        }
        // A child whose class is not known, and one below a sibling: only the first refuses.
        assert!(
            typed(&[
                user,
                "FactoryBot.define do\n  factory :admin, parent: :user, class: klass\nend\n"
            ])
            .is_empty()
        );
        assert_eq!(
            typed(&[
                user,
                "FactoryBot.define do\n  factory :post\n  factory dynamic, parent: :post\nend\n"
            ]),
            ["after"]
        );
        // A block handed to `send`, parameters FactoryBot reads by arity, a name it registers the
        // block under that hands something else, or none written as a literal.
        for written in [
            "send(:after, :create) { |user| user }",
            "after(:create) { |a, b, c| a }",
            "after(:all) { |result| result }",
            "after(name) { |user| user }",
        ] {
            let source = format!(
                "FactoryBot.define do\n  factory :user do\n    after(:create) {{ |user| user }}\n    \
                 {written}\n  end\nend\n"
            );
            assert!(typed(&[&source]).is_empty(), "{written}");
        }
        // One class or `nil`.
        let rbs = proxied(
            &[kept(
                "FactoryBot.define do\n  factory :post do\n    before(:build, :create) { |x| x }\n  \
                 end\nend\n",
            )],
            &["Post"],
            &bundle,
        );
        assert!(
            rbs.contains("{ (::Post? object, untyped evaluator) -> void }"),
            "{rbs}"
        );
        // Only ever handed `nil`, which says nothing.
        assert!(
            typed(&["FactoryBot.define do\n  factory :user do\n    before(:build) { |x| x }\n  end\nend\n"])
                .is_empty()
        );
        // A name two definitions write, and a gem's the project writes too, are left as they were.
        let twin =
            "FactoryBot.define do\n  factory :user do\n    after(:create) { |u| u }\n  end\nend\n";
        let reads = [kept(twin), kept(&format!("{twin}\n"))];
        assert_eq!(proxied(&reads, &["User"], &bundle), "");
        let mut gem = kept(&format!("{twin}\n\n"));
        gem.gem = true;
        let rbs = proxied(&[kept(twin), gem], &["User"], &bundle);
        assert_eq!(
            rbs.matches("class FactoryBot::Factories::User ").count(),
            1,
            "{rbs}"
        );
    }

    #[test]
    fn a_loop_of_parents_is_walked_once() {
        // `classes` answers no class anywhere in a loop, so this never meets one; the walk still
        // ends.
        let read = read_factories("FactoryBot.define do\n  factory :a, parent: :b\nend\n");
        let built = Classes {
            factories: BTreeMap::from([
                ("a".to_owned(), Some("A".to_owned())),
                ("b".to_owned(), Some("B".to_owned())),
            ]),
            ..Classes::default()
        };
        let children = BTreeMap::from([("a", vec!["b"]), ("b", vec!["a"])]);
        assert_eq!(
            objects_of(&read.factories[0], &built, &children),
            Some(vec!["A".to_owned(), "B".to_owned()])
        );
    }

    #[test]
    fn a_constructor_outside_every_factory_or_in_a_modify_counts_for_all() {
        let global = read_factories(
            "FactoryBot.define do\n  initialize_with { new }\n  trait :x do\n    \
             initialize_with { find_or_create_by(id: 1) }\n  end\nend\n",
        );
        assert!(
            global.otherwise,
            "a global trait's constructor may be any factory's"
        );
        let fine =
            read_factories("FactoryBot.define do\n  initialize_with { new(attributes) }\nend\n");
        assert!(!fine.otherwise);
        let modified = read_factories(
            "FactoryBot.modify do\n  factory :user, class: Other do\n    \
             initialize_with { attributes }\n    factory :child\n  end\nend\n",
        );
        assert!(modified.factories.is_empty(), "`modify` defines nothing");
        assert!(modified.otherwise);
        let empty =
            read_factories("FactoryBot.modify do\n  factory :user\nend\nFactoryBot.define\n");
        assert_eq!(empty, Read::default());
        // A `FactoryBot` that is not the library's, and `define` sent to something else.
        let other = read_factories(
            "Other::FactoryBot.define do\n  factory :x\nend\nbot.define do\n  factory :y\nend\n",
        );
        assert!(other.factories.is_empty());
    }

    #[test]
    fn a_replaced_strategy_is_read_by_name_or_as_any() {
        let read = read_factories(
            "FactoryBot.register_strategy(:create, Custom)\nFactoryBot.register_strategy(\"json\", J)\n",
        );
        assert_eq!(
            read.registered,
            BTreeSet::from(["create".to_owned(), "json".to_owned()])
        );
        let any = read_factories("FactoryBot.register_strategy(name, Custom)\n");
        assert_eq!(
            any.registered,
            BTreeSet::from(
                ["attributes_for", "build", "build_stubbed", "create"].map(str::to_owned)
            )
        );
    }

    #[test]
    fn a_factory_builds_its_class_its_parents_or_its_topmost_name() {
        let reads = [
            read_factories(
                r#"
module Shop
  FactoryBot.define do
    factory :user, aliases: [:author] do
      factory :admin
      factory :staff, class: "Shop::Staff"
    end
    factory :order, class: Order do
      factory :special, class: "Line"
    end
    factory :line, class: "Line"
    factory :dynamic, class: klass
    factory :lost, parent: :nobody
    factory :loop_a, parent: :loop_b
    factory :loop_b, parent: :loop_a
    factory :unknown
    factory :__
  end
end
"#,
            ),
            read_factories(
                r#"
FactoryBot.define do
  factory :odd, class: User do
    initialize_with { attributes }
  end
  factory :odd_child, parent: :odd, class: User
  factory :twin
  factory :twin_child, parent: :twin
end
"#,
            ),
            read_factories("FactoryBot.define do\n  factory :twin\nend\n"),
        ];
        assert_eq!(
            built(
                &reads,
                &[
                    "User",
                    "Shop::Order",
                    "Order",
                    "Line",
                    "Shop::Line",
                    "Shop::Staff",
                    "Twin",
                    // Declared, so a parent nothing defines is what leaves `lost` untyped.
                    "Lost"
                ]
            ),
            [
                "__=-",
                "admin=User",
                "author=User",
                "dynamic=-",
                "line=Line",
                "loop_a=-",
                "loop_b=-",
                "lost=-",
                "odd=-",
                "odd_child=-",
                "order=Shop::Order",
                "special=Line",
                "staff=Shop::Staff",
                "twin=-",
                "twin_child=-",
                "unknown=-",
                "user=User",
            ]
        );
        // One global constructor that builds something else leaves every factory untyped.
        let mut global = reads[0].clone();
        global.otherwise = true;
        assert!(
            built(&[global], &["User"])
                .iter()
                .all(|answer| answer.ends_with("=-"))
        );
    }

    #[test]
    fn a_factory_is_placed_where_its_call_is_written_and_a_doubled_name_nowhere() {
        let source = "FactoryBot.define do\n  factory :user, aliases: [:author]\n  factory \"order\"\n  factory :twin\nend\n";
        let mut read = read_factories(source);
        read.uri = "file:///p/spec/factories.rb".to_owned();
        let mut other = read_factories("FactoryBot.define do\n  factory :twin\nend\n");
        other.uri = "file:///p/spec/other.rb".to_owned();
        let placed = classes(&[read, other], &declared(&[])).places;
        let spelled = |name: &str| {
            placed
                .get(name)
                .map(|(uri, ((start, end), (name, name_end)))| {
                    (
                        uri.as_str(),
                        &source[*start as usize..*end as usize],
                        &source[*name as usize..*name_end as usize],
                    )
                })
        };
        assert_eq!(
            spelled("user"),
            Some((
                "file:///p/spec/factories.rb",
                "factory :user, aliases: [:author]",
                "user"
            ))
        );
        assert_eq!(spelled("author"), spelled("user"));
        assert_eq!(
            spelled("order"),
            Some(("file:///p/spec/factories.rb", "factory \"order\"", "order"))
        );
        assert_eq!(spelled("twin"), None);
    }

    #[test]
    fn a_gem_s_factory_answers_only_for_a_name_the_project_does_not_write() {
        let own = read_factories(
            "FactoryBot.define do\n  factory :user, class: Account\n  factory :admin, parent: :order\nend\n",
        );
        let mut gem = read_factories(
            "FactoryBot.define do\n  factory :user, class: Order\n  factory :order, class: Order\n  \
             factory :line, class: Order\nend\n",
        );
        gem.gem = true;
        let mut other =
            read_factories("FactoryBot.define do\n  factory :line, class: Order\nend\n");
        other.gem = true;
        assert_eq!(
            built(&[own, gem, other], &["Account", "Order"]),
            ["admin=Order", "line=-", "order=Order", "user=Account"]
        );
    }

    #[test]
    fn the_names_asked_about_are_every_one_a_definition_may_mean() {
        let reads = [read_factories(
            r#"
module Shop
  FactoryBot.define do
    factory :user, class: Account
    factory :order, class: "shop/order"
    factory :odd, class: klass
    factory :"9"
  end
end
"#,
        )];
        assert_eq!(
            wanted_names(&reads),
            BTreeSet::from(
                [
                    "Account",
                    "Odd",
                    "Order",
                    "Shop::Account",
                    "Shop::Order",
                    "User",
                ]
                .map(str::to_owned)
            )
        );
    }

    #[test]
    fn a_name_is_constantized_as_active_support_reads_it() {
        assert_eq!(
            constantized("spree/address").as_deref(),
            Some("Spree::Address")
        );
        assert_eq!(
            constantized("::Admin::User").as_deref(),
            Some("Admin::User")
        );
        assert_eq!(constantized("admin_user").as_deref(), Some("AdminUser"));
        assert_eq!(constantized("admin/9"), None);
        assert_eq!(candidates_of("", "User"), ["User"]);
        assert_eq!(
            candidates_of("A::B", "User"),
            ["A::B::User", "A::User", "User"]
        );
    }

    #[test]
    fn the_rows_are_written_on_what_the_bundle_declares() {
        let reads = [read_factories(
            r#"
FactoryBot.define do
  factory :user
  factory :admin, class: Admin
  factory :"odd name", class: User
  factory :"9lives", class: User
  factory :_hidden, class: User
  factory :dynamic, class: klass
end
"#,
        )];
        let classes = classes(&reads, &declared(&["User", "Admin"]));
        assert!(rows(&classes, &declaring(&[])).is_empty());
        let bundle = declaring(&["FactoryBot", SYNTAX]);
        let hosted = rows(&classes, &bundle);
        let owners: Vec<&str> = hosted.iter().map(|(owner, _)| *owner).collect();
        assert_eq!(owners, [SYNTAX]);
        let rbs: String = hosted
            .iter()
            .map(|(_, facts)| facts.render(&bundle).rbs)
            .collect();
        for expected in [
            "module FactoryBot",
            "def create: (:_hidden factory, *untyped args, **untyped kwargs) ?{ (untyped) -> untyped } -> ::User \
             | (:admin factory, *untyped args, **untyped kwargs) ?{ (untyped) -> untyped } -> ::Admin \
             | (:user factory, *untyped args, **untyped kwargs) ?{ (untyped) -> untyped } -> ::User \
             | (untyped factory, *untyped args, **untyped kwargs) ?{ (untyped) -> untyped } -> untyped\n",
            "def build_stubbed_pair: (:_hidden factory, *untyped args, **untyped kwargs) \
             ?{ (untyped) -> untyped } -> ::Array[::User]",
            // A list takes its count after the factory.
            "def create_list: (:_hidden factory, untyped amount, *untyped args, **untyped kwargs) \
             ?{ (untyped) -> untyped } -> ::Array[::User]",
            "| (untyped factory, untyped amount, *untyped args, **untyped kwargs) \
             ?{ (untyped) -> untyped } -> untyped\n",
            "def attributes_for: (untyped factory, *untyped args, **untyped kwargs) \
             ?{ (untyped) -> untyped } -> ::Hash[::Symbol, untyped]",
            "def attributes_for_list: (untyped factory, untyped amount, *untyped args, **untyped kwargs) \
             ?{ (untyped) -> untyped } -> ::Array[::Hash[::Symbol, untyped]]",
        ] {
            assert!(rbs.contains(expected), "{expected}\n---\n{rbs}");
        }
        assert!(
            !rbs.contains("odd name") && !rbs.contains(":9lives") && !rbs.contains(":dynamic"),
            "{rbs}"
        );
        // `Syntax::Methods` as a class, and the strategies the project replaces left out.
        let mut replaced = classes.clone();
        replaced.registered = BTreeSet::from(["create".to_owned(), ATTRIBUTES_FOR.to_owned()]);
        let classy = declaring_kinds(&[SYNTAX], &[]);
        let rbs: String = rows(&replaced, &classy)
            .iter()
            .map(|(_, facts)| facts.render(&classy).rbs)
            .collect();
        assert!(rbs.contains("def build:"), "{rbs}");
        assert!(
            !rbs.contains("def create") && !rbs.contains("attributes_for"),
            "{rbs}"
        );
        // No definitions: no rows, even where the bundle declares the owner.
        assert!(rows(&Classes::default(), &bundle).is_empty());
    }
}
