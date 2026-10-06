//! Which controller action each route reaches, and the path segments it requires.
//!
//! [`super::routes`] reads the same DSL for the helpers it names; this reads it for where a request
//! lands: `resources :posts` is `posts#show` at `/posts/:id(.:format)`, so `params[:id]` there is
//! a `String`. Rails' own rules, in the order they mattered on real routes files:
//!
//! 1. **Route blocks wherever they are written**: a routes file whole, each file it `draw`s, and
//!    every `routes.draw`/`append`/`prepend` block elsewhere (an engine's, a plugin's).
//! 2. **Engines**: a block drawn on any constant but an application is that engine's, read under
//!    each prefix it is `mount`ed at (or none) and its `isolate_namespace` module. A mounted engine
//!    whose routes are not in the text reaches every controller in its namespace.
//! 3. **Scopes**: `namespace`, `scope` (path, `module:`, `controller:`), `controller`,
//!    `with_options`, `defaults`, `constraints`, `concern`/`concerns`, a namespace inside a
//!    `resources` nesting under its member.
//! 4. **Resources**: `only`/`except`, `param:`, `path:`, `controller:`, `module:`, `to:`, `shallow`
//!    on the parent and the child, `member`/`collection`/`new` and `on:`.
//! 5. **A loop over a literal array** is read once per element, with the variable bound.
//! 6. **A target that is no controller** (`redirect`, a lambda, a Rack constant, a project method
//!    that builds an object) reaches none.
//! 7. **Everything else is unreadable** ([`Unreadable`]): bounded to the controller a `"c#a"`
//!    literal in its text names, or to the resource a project macro hands `resources`, and
//!    otherwise to any controller at all.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use ruby_prism::{CallNode, Node, Visit};

use super::inflect::{pluralize, singularize, underscore};

/// One route: the controller (as a path, `admin/posts`) and action it reaches, its path, the
/// segments the path requires, and the values its defaults give a key, by class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub controller: String,
    pub action: String,
    pub spec: String,
    pub required: Vec<String>,
    pub defaults: BTreeMap<String, &'static str>,
}

/// What a route the text cannot read may reach.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reach {
    /// Any controller at all.
    Any,
    /// This controller, as a path.
    Controller(String),
    /// Every controller under this path (`blazer`, for `blazer/*`).
    Namespace(String),
}

/// A route the text cannot read, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    pub reach: Reach,
    /// The action, where the text names it.
    pub action: Option<String>,
    /// A call the DSL does not have, by name: a gem's routing macro (`devise_for`'s kin) or a
    /// project method.
    pub call: Option<String>,
    pub why: &'static str,
}

/// Every route the text reads, and every one it cannot.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RouteTable {
    pub targets: Vec<Target>,
    pub unreadable: Vec<Unreadable>,
}

/// A file to read routes from: whole (a routes file, or one it draws), or only its route blocks.
pub struct RouteSource<'a> {
    pub text: &'a str,
    pub whole: bool,
}

/// What a project method a routes file calls does, read from its `def` ([`read_route_macros`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Macro {
    /// It hands its first parameter to `resources` (or to `resource`, `singular`).
    Forwards { singular: bool },
    /// Its last expression builds an object: a Rack application, which reaches no controller.
    RackApp,
}

/// What the reader needs beyond the text.
pub struct RouteContext<'a> {
    /// The text of the file `draw :name` reads, or `None` where there is none.
    pub drawn: &'a dyn Fn(&str) -> Option<String>,
    /// Each engine class that writes `isolate_namespace`, and the module it names.
    pub isolated: &'a BTreeMap<String, String>,
    /// The project methods a routes file may call, by name.
    pub macros: &'a BTreeMap<String, Macro>,
}

/// The options Rails reads itself, which are never a default.
const RESERVED: [&str; 22] = [
    "to",
    "controller",
    "action",
    "via",
    "as",
    "on",
    "format",
    "constraints",
    "defaults",
    "anchor",
    "path",
    "module",
    "only",
    "except",
    "param",
    "shallow",
    "shallow_path",
    "shallow_prefix",
    "concerns",
    "path_names",
    "internal",
    "trailing_slash",
];

/// The verbs that draw one route.
const VERBS: [&str; 8] = [
    "get", "post", "put", "patch", "delete", "options", "head", "match",
];

/// What a resource's seven actions are.
const PLURAL_ACTIONS: [&str; 7] = [
    "index", "create", "new", "show", "update", "destroy", "edit",
];
const SINGULAR_ACTIONS: [&str; 6] = ["show", "create", "update", "destroy", "new", "edit"];

/// Calls a routes file may make that draw nothing.
const QUIET: [&str; 17] = [
    "require",
    "require_relative",
    "extend",
    "include",
    "puts",
    "warn",
    "p",
    "raise",
    "lambda",
    "use",
    "proc",
    "freeze",
    "private",
    "attr_reader",
    "direct",
    "resolve",
    "redirect",
];

/// Scopes that only filter or wrap: their block's routes are the block's.
const PASSING: [&str; 6] = [
    "constraints",
    "authenticate",
    "authenticated",
    "unauthenticated",
    "devise_scope",
    "shallow",
];

/// An option as written, read once where it is written.
#[derive(Debug, Clone, Default)]
struct Opt {
    /// Its text, where it is a literal the scope can spell.
    text: Option<String>,
    /// Its texts, where it is a list of them (or one).
    list: Option<Vec<String>>,
    /// The class of a literal value, for a default.
    class: Option<&'static str>,
    /// A hash literal's pairs, each a literal key and its value's class.
    pairs: Option<Vec<(String, Option<&'static str>)>>,
    /// `true` written.
    yes: bool,
    /// A `Regexp`: a constraint, never a default.
    pattern: bool,
    /// A target that reaches no controller: `redirect`, a lambda, a Rack constant, `Proc.new`, a
    /// project method that builds an object.
    rack: bool,
    /// An interpolated string whose first part names a controller (`"c##{x}"`).
    controller_head: Option<String>,
}

/// A `"path" => "c#a"` pair: the path's text, where the scope can spell it, and the target.
type Rocket = (Option<String>, Opt);

/// A resource the walk is inside.
#[derive(Debug, Clone)]
struct Res {
    controller: String,
    collection: String,
    member: String,
    nested: String,
    shallow_member: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Root,
    Namespace,
    Resources,
    Member,
}

#[derive(Debug, Clone)]
struct Scope {
    path: String,
    module: Option<String>,
    controller: Option<String>,
    defaults: BTreeMap<String, &'static str>,
    level: Level,
    res: Option<Rc<Res>>,
    shallow: bool,
    shallow_path: String,
    locals: Rc<HashMap<String, String>>,
    with: Rc<BTreeMap<String, Opt>>,
}

impl Scope {
    fn root(path: &str, module: Option<String>) -> Self {
        Self {
            path: path.to_owned(),
            module,
            controller: None,
            defaults: BTreeMap::new(),
            level: Level::Root,
            res: None,
            shallow: false,
            shallow_path: path.to_owned(),
            locals: Rc::default(),
            with: Rc::default(),
        }
    }
}

/// Path parts joined as Rails joins a scope's: one `/` between, a leading one, none doubled.
fn join(parts: &[&str]) -> String {
    let joined = parts
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    let mut path = if joined.starts_with('/') || joined.starts_with("(/") {
        joined
    } else {
        format!("/{joined}")
    };
    // `(//` is gone with every `//`.
    while path.contains("//") {
        path = path.replace("//", "/");
    }
    path.replace("/(/", "(/")
}

/// The segments a path requires: `:name` and `*name` outside every `( … )`.
#[must_use]
pub fn required_of(spec: &str) -> Vec<String> {
    let mut depth = 0u32;
    let mut out = Vec::new();
    let bytes = spec.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b':' | b'*'
                if bytes
                    .get(at + 1)
                    .is_some_and(|next| next.is_ascii_alphabetic() || *next == b'_') =>
            {
                let start = at + 1;
                let mut end = start;
                while bytes
                    .get(end)
                    .is_some_and(|next| next.is_ascii_alphanumeric() || *next == b'_')
                {
                    end += 1;
                }
                if depth == 0 {
                    out.push(spec[start..end].to_owned());
                }
                at = end;
                continue;
            }
            _ => {}
        }
        at += 1;
    }
    out
}

fn name_of(node: &CallNode<'_>) -> String {
    String::from_utf8_lossy(node.name().as_slice()).into_owned()
}

/// A literal's text, as the scope spells it: a symbol, a string, an interpolation of those, a loop
/// variable, an integer.
fn lit(node: &Node<'_>, scope: &Scope) -> Option<String> {
    if let Some(symbol) = node.as_symbol_node() {
        return Some(String::from_utf8_lossy(symbol.unescaped()).into_owned());
    }
    if let Some(string) = node.as_string_node() {
        return Some(String::from_utf8_lossy(string.unescaped()).into_owned());
    }
    if let Some(local) = node.as_local_variable_read_node() {
        return scope
            .locals
            .get(String::from_utf8_lossy(local.name().as_slice()).as_ref())
            .cloned();
    }
    if let Some(integer) = node.as_integer_node() {
        return Some(String::from_utf8_lossy(integer.location().as_slice()).into_owned());
    }
    let parts = if let Some(string) = node.as_interpolated_string_node() {
        string.parts()
    } else {
        node.as_interpolated_symbol_node()?.parts()
    };
    let mut text = String::new();
    for part in parts.iter() {
        if let Some(string) = part.as_string_node() {
            text.push_str(&String::from_utf8_lossy(string.unescaped()));
        } else {
            let statements = part.as_embedded_statements_node()?.statements()?;
            let mut body = statements.body().iter();
            let only = body.next()?;
            if body.next().is_some() {
                return None;
            }
            text.push_str(&lit(&only, scope)?);
        }
    }
    Some(text)
}

/// The class a literal default holds.
fn class_of(node: &Node<'_>, scope: &Scope) -> Option<&'static str> {
    if node.as_symbol_node().is_some() {
        Some("Symbol")
    } else if node.as_string_node().is_some()
        || (node.as_interpolated_string_node().is_some() && lit(node, scope).is_some())
    {
        Some("String")
    } else if node.as_integer_node().is_some() {
        Some("Integer")
    } else if node.as_true_node().is_some() || node.as_false_node().is_some() {
        Some("bool")
    } else if node.as_nil_node().is_some() {
        Some("nil")
    } else {
        None
    }
}

struct Walk<'c, 'pr> {
    context: &'c RouteContext<'c>,
    /// Every parsed file a `draw` names, by name.
    drawn: &'pr HashMap<String, ruby_prism::ParseResult<'pr>>,
    concerns: HashMap<String, Node<'pr>>,
    /// Blocks drawn on an engine, by the engine's name.
    engines: BTreeMap<String, Vec<Node<'pr>>>,
    /// Where each engine is mounted.
    mounts: BTreeMap<String, Vec<String>>,
    /// Engines mounted, in order.
    mounted: Vec<String>,
    table: RouteTable,
}

impl<'pr> Walk<'_, 'pr> {
    fn opt(&self, node: &Node<'pr>, scope: &Scope) -> Opt {
        let mut opt = Opt {
            text: lit(node, scope),
            class: class_of(node, scope),
            yes: node.as_true_node().is_some(),
            pattern: node.as_regular_expression_node().is_some()
                || node.as_interpolated_regular_expression_node().is_some(),
            ..Opt::default()
        };
        opt.list = match node.as_array_node() {
            Some(array) => array
                .elements()
                .iter()
                .map(|element| lit(&element, scope))
                .collect(),
            None => opt.text.clone().map(|text| vec![text]),
        };
        if let Some(hash) = node.as_hash_node() {
            opt.pairs = Some(
                hash.elements()
                    .iter()
                    .filter_map(|element| {
                        let assoc = element.as_assoc_node()?;
                        Some((lit(&assoc.key(), scope)?, class_of(&assoc.value(), scope)))
                    })
                    .collect(),
            );
        }
        if let Some(call) = node.as_call_node() {
            let name = name_of(&call);
            let receiver = call.receiver();
            opt.rack = matches!(name.as_str(), "redirect" | "proc" | "lambda")
                || (receiver.is_none() && self.context.macros.get(&name) == Some(&Macro::RackApp))
                || (name == "new"
                    && receiver.as_ref().is_some_and(|receiver| {
                        receiver
                            .as_constant_read_node()
                            .is_some_and(|constant| constant.name().as_slice() == b"Proc")
                    }));
        }
        opt.rack |= node.as_constant_read_node().is_some()
            || node.as_constant_path_node().is_some()
            || node.as_lambda_node().is_some();
        if let Some(head) = node.as_interpolated_string_node().and_then(|string| {
            string
                .parts()
                .iter()
                .next()
                .and_then(|head| head.as_string_node())
        }) {
            let head = String::from_utf8_lossy(head.unescaped()).into_owned();
            if let Some((controller, _)) = head.split_once('#') {
                opt.controller_head = Some(controller.to_owned());
            }
        }
        opt
    }

    /// A call's keyword options, and its `"path" => "c#a"` pair.
    fn options(
        &self,
        call: &CallNode<'pr>,
        scope: &Scope,
    ) -> (BTreeMap<String, Opt>, Option<Rocket>) {
        let mut opts = BTreeMap::new();
        let mut rocket = None;
        for argument in arguments(call) {
            let elements: Vec<Node<'pr>> = if let Some(hash) = argument.as_keyword_hash_node() {
                hash.elements().iter().collect()
            } else if let Some(hash) = argument.as_hash_node() {
                hash.elements().iter().collect()
            } else {
                continue;
            };
            for element in elements {
                let Some(assoc) = element.as_assoc_node() else {
                    continue;
                };
                let key = assoc.key();
                let value = self.opt(&assoc.value(), scope);
                if let Some(symbol) = key.as_symbol_node() {
                    opts.insert(
                        String::from_utf8_lossy(symbol.unescaped()).into_owned(),
                        value,
                    );
                } else if key.as_string_node().is_some()
                    || key.as_interpolated_string_node().is_some()
                    || key.as_local_variable_read_node().is_some()
                {
                    rocket = Some((lit(&key, scope), value));
                }
            }
        }
        (opts, rocket)
    }

    /// A route the text cannot read. Reaching any controller, it is bounded to the `"c#a"`
    /// literals its call writes, where it writes any.
    fn unknown(
        &mut self,
        scope: &Scope,
        reach: Reach,
        action: Option<String>,
        call: Option<(&CallNode<'pr>, bool)>,
        why: &'static str,
    ) {
        if reach == Reach::Any
            && let Some((node, false)) = call
        {
            let mut found = Targets::default();
            found.visit(&node.as_node());
            if !found.targets.is_empty() {
                for (controller, action) in found.targets {
                    self.table.unreadable.push(Unreadable {
                        reach: Reach::Controller(controller_path(&controller, scope)),
                        action: (!action.is_empty()).then_some(action),
                        call: None,
                        why,
                    });
                }
                return;
            }
        }
        let name = call
            .filter(|(_, unknown)| *unknown)
            .map(|(node, _)| name_of(node));
        self.table.unreadable.push(Unreadable {
            reach,
            action,
            call: name,
            why,
        });
    }

    fn emit(
        &mut self,
        scope: &Scope,
        spec: &str,
        controller: Option<String>,
        action: Option<String>,
        extra: BTreeMap<String, &'static str>,
        call: &CallNode<'pr>,
    ) {
        let (controller, action) = match (controller, action) {
            (Some(controller), Some(action)) => (controller, action),
            (controller, action) => {
                let reach = controller.map_or(Reach::Any, Reach::Controller);
                return self.unknown(scope, reach, action, Some((call, false)), "no target");
            }
        };
        let mut defaults = scope.defaults.clone();
        defaults.extend(extra);
        let spec = if spec.ends_with("(.:format)") || spec.ends_with(".:format") {
            spec.to_owned()
        } else {
            format!("{spec}(.:format)")
        };
        self.table.targets.push(Target {
            controller,
            action,
            required: required_of(&spec),
            spec,
            defaults,
        });
    }

    fn walk(&mut self, node: Option<Node<'pr>>, scope: &Scope) {
        let Some(node) = node else {
            return;
        };
        if let Some(statements) = node.as_statements_node() {
            let mut scope = scope.clone();
            for statement in statements.body().iter() {
                // A local written between routes binds for the routes after it.
                if let Some(write) = statement.as_local_variable_write_node() {
                    let mut locals = (*scope.locals).clone();
                    let name = String::from_utf8_lossy(write.name().as_slice()).into_owned();
                    match lit(&write.value(), &scope) {
                        Some(value) => locals.insert(name, value),
                        None => locals.remove(&name),
                    };
                    scope.locals = Rc::new(locals);
                    continue;
                }
                self.walk(Some(statement), &scope);
            }
        } else if let Some(begin) = node.as_begin_node() {
            self.walk(
                begin.statements().map(|statements| statements.as_node()),
                scope,
            );
        } else if let Some(group) = node.as_parentheses_node() {
            self.walk(group.body(), scope);
        } else if let Some(conditional) = node.as_if_node() {
            self.walk(conditional.statements().map(|s| s.as_node()), scope);
            self.walk(conditional.subsequent(), scope);
        } else if let Some(conditional) = node.as_unless_node() {
            self.walk(conditional.statements().map(|s| s.as_node()), scope);
            self.walk(conditional.else_clause().map(|e| e.as_node()), scope);
        } else if let Some(other) = node.as_else_node() {
            self.walk(other.statements().map(|s| s.as_node()), scope);
        } else if let Some(case) = node.as_case_node() {
            for when in case
                .conditions()
                .iter()
                .filter_map(|condition| condition.as_when_node())
            {
                self.walk(when.statements().map(|s| s.as_node()), scope);
            }
            self.walk(case.else_clause().map(|e| e.as_node()), scope);
        } else if let Some(call) = node.as_call_node() {
            self.call(&call, scope);
        }
    }

    /// A block's routes read under a name the text cannot read: each becomes an unreadable route
    /// of its controller.
    fn walk_unknown(
        &mut self,
        body: Option<Node<'pr>>,
        scope: &Scope,
        call: &CallNode<'pr>,
        why: &'static str,
    ) {
        if body.is_none() {
            return self.unknown(scope, Reach::Any, None, Some((call, false)), why);
        }
        let before = self.table.targets.len();
        self.walk(body, scope);
        let read: Vec<Target> = self.table.targets.drain(before..).collect();
        for target in read {
            self.table.unreadable.push(Unreadable {
                reach: Reach::Controller(target.controller),
                action: None,
                call: None,
                why,
            });
        }
    }

    #[allow(clippy::too_many_lines)]
    fn call(&mut self, node: &CallNode<'pr>, scope: &Scope) {
        let name = name_of(node);
        let args = positional(node);
        let (mut opts, rocket) = self.options(node, scope);
        if matches!(name.as_str(), "resources" | "resource") || VERBS.contains(&name.as_str()) {
            let mut merged = (*scope.with).clone();
            merged.extend(opts);
            opts = merged;
        }
        let body = node
            .block()
            .and_then(|block| block.as_block_node())
            .and_then(|block| block.body());
        let receiver = node.receiver();
        // `X.routes.draw do`, `routes.append do`: an engine's or the application's.
        if matches!(name.as_str(), "draw" | "append" | "prepend")
            && let Some(routes) = receiver.as_ref().and_then(Node::as_call_node)
            && routes.name().as_slice() == b"routes"
        {
            let owner = routes.receiver().and_then(|owner| constant_name(&owner));
            match owner {
                Some(owner) if !owner.ends_with("Application") && owner != "Rails" => {
                    self.engines.entry(owner).or_default().extend(body);
                }
                _ => self.walk(body, scope),
            }
            return;
        }
        // `%w[a b].each do |x|`: once per element, with `x` bound.
        if matches!(name.as_str(), "each" | "each_with_index")
            && let Some(on) = receiver.as_ref()
            && body.is_some()
        {
            let items: Option<Vec<String>> = on.as_array_node().and_then(|array| {
                array
                    .elements()
                    .iter()
                    .map(|element| lit(&element, scope))
                    .collect()
            });
            let parameter = node
                .block()
                .and_then(|block| block.as_block_node())
                .and_then(|block| block.parameters())
                .and_then(|parameters| parameters.as_block_parameters_node())
                .and_then(|parameters| parameters.parameters())
                .and_then(|list| list.requireds().iter().next())
                .and_then(|first| first.as_required_parameter_node())
                .map(|first| String::from_utf8_lossy(first.name().as_slice()).into_owned());
            match (items, parameter) {
                (Some(items), Some(parameter)) => {
                    for item in items {
                        let mut bound = scope.clone();
                        let mut locals = (*scope.locals).clone();
                        locals.insert(parameter.clone(), item);
                        bound.locals = Rc::new(locals);
                        let body = node
                            .block()
                            .and_then(|block| block.as_block_node())
                            .and_then(|block| block.body());
                        self.walk(body, &bound);
                    }
                }
                _ => self.walk_unknown(body, scope, node, "a loop over a value"),
            }
            return;
        }
        if receiver.is_some() {
            // `some_object.something do … end`
            if body.is_some() {
                self.walk(body, scope);
            }
            return;
        }
        let text = |key: &str| opts.get(key).and_then(|opt| opt.text.clone());
        let defaults_of = |opts: &BTreeMap<String, Opt>| {
            let mut out = scope.defaults.clone();
            if let Some(pairs) = opts.get("defaults").and_then(|opt| opt.pairs.as_ref()) {
                out.extend(
                    pairs
                        .iter()
                        .filter_map(|(key, class)| Some((key.clone(), (*class)?))),
                );
            }
            out
        };
        match name.as_str() {
            "namespace" => {
                let Some(namespace) = args.first().and_then(|first| lit(first, scope)) else {
                    return self.walk_unknown(body, scope, node, "a namespace's name");
                };
                let path = text("path").unwrap_or_else(|| namespace.clone());
                let module = text("module").unwrap_or(namespace);
                let base = match (&scope.res, scope.level) {
                    (Some(res), Level::Resources) => res.nested.clone(),
                    _ => scope.path.clone(),
                };
                let inner = Scope {
                    path: join(&[&base, &path]),
                    module: Some(join_module(scope.module.as_deref(), &module)),
                    level: Level::Namespace,
                    res: None,
                    shallow_path: join(&[&scope.shallow_path, &path]),
                    defaults: defaults_of(&opts),
                    ..scope.clone()
                };
                self.walk(body, &inner);
            }
            "scope" => {
                let path = match (args.first(), opts.get("path")) {
                    (Some(first), _) if first.as_nil_node().is_none() => match lit(first, scope) {
                        Some(path) => Some(path),
                        None => return self.walk_unknown(body, scope, node, "a scope's path"),
                    },
                    (Some(_), _) => None,
                    (None, Some(opt)) => match &opt.text {
                        Some(path) => Some(path.clone()),
                        None if opt.class == Some("nil") => None,
                        None => return self.walk_unknown(body, scope, node, "a scope's path"),
                    },
                    (None, None) => None,
                };
                let inner = Scope {
                    path: path
                        .as_ref()
                        .map_or_else(|| scope.path.clone(), |path| join(&[&scope.path, path])),
                    module: match text("module") {
                        Some(module) => Some(join_module(scope.module.as_deref(), &module)),
                        None => scope.module.clone(),
                    },
                    controller: text("controller").or_else(|| scope.controller.clone()),
                    shallow_path: path.as_ref().map_or_else(
                        || scope.shallow_path.clone(),
                        |path| join(&[&scope.shallow_path, path]),
                    ),
                    defaults: defaults_of(&opts),
                    ..scope.clone()
                };
                self.walk(body, &inner);
            }
            "controller" => {
                let Some(controller) = args.first().and_then(|first| lit(first, scope)) else {
                    return self.walk_unknown(body, scope, node, "a controller's name");
                };
                let inner = Scope {
                    controller: Some(controller),
                    ..scope.clone()
                };
                self.walk(body, &inner);
            }
            "defaults" => {
                let mut inner = scope.clone();
                for (key, opt) in &opts {
                    if let Some(class) = opt.class {
                        inner.defaults.insert(key.clone(), class);
                    }
                }
                self.walk(body, &inner);
            }
            "with_options" => {
                let mut with = (*scope.with).clone();
                with.extend(opts);
                let inner = Scope {
                    with: Rc::new(with),
                    ..scope.clone()
                };
                self.walk(body, &inner);
            }
            "concern" => {
                if let Some(concern) = args.first().and_then(|first| lit(first, scope))
                    && let Some(body) = body
                {
                    self.concerns.insert(concern, body);
                }
            }
            "concerns" => {
                for argument in &args {
                    self.concerned(lit(argument, scope), scope, node);
                }
            }
            "draw" => {
                let Some(drawn) = args.first().and_then(|first| lit(first, scope)) else {
                    return self.unknown(
                        scope,
                        Reach::Any,
                        None,
                        Some((node, false)),
                        "a drawn file's name",
                    );
                };
                match self.drawn.get(&drawn) {
                    Some(parsed) => {
                        let statements = parsed
                            .node()
                            .as_program_node()
                            .map(|program| program.statements().as_node());
                        self.walk(statements, scope);
                    }
                    None => self.unknown(
                        scope,
                        Reach::Any,
                        None,
                        Some((node, false)),
                        "a drawn file missing",
                    ),
                }
            }
            "resources" | "resource" => {
                self.resources(node, name == "resource", &args, &opts, body, scope);
            }
            "member" | "collection" | "new" => {
                let Some(res) = scope.res.clone() else {
                    return self.walk(body, scope);
                };
                let path = match name.as_str() {
                    "member" => res
                        .shallow_member
                        .clone()
                        .unwrap_or_else(|| res.member.clone()),
                    "collection" => res.collection.clone(),
                    _ => join(&[&res.collection, "new"]),
                };
                let inner = Scope {
                    path,
                    level: Level::Member,
                    ..scope.clone()
                };
                self.walk(body, &inner);
            }
            "root" => {
                let to = opts
                    .get("to")
                    .cloned()
                    .or_else(|| args.first().map(|first| self.opt(first, scope)));
                // A Rack application or a redirect reaches no controller.
                if to.as_ref().is_some_and(|to| to.rack) {
                    return;
                }
                if let Some(head) = to.as_ref().and_then(|to| to.controller_head.clone()) {
                    let reach = Reach::Controller(controller_path(&head, scope));
                    return self.unknown(
                        scope,
                        reach,
                        None,
                        None,
                        "root to an interpolated action",
                    );
                }
                let (mut controller, mut action) =
                    split_to(to.and_then(|to| to.text).as_deref(), scope);
                if controller.is_none() {
                    controller = scope
                        .controller
                        .as_deref()
                        .map(|controller| controller_path(controller, scope));
                }
                if action.is_none() {
                    action = text("action");
                }
                let spec = join(&[&scope.path, "/"]);
                let extra = route_defaults(&opts);
                self.emit(scope, &spec, controller, action, extra, node);
            }
            "mount" => {
                let mut app = args.first().and_then(|first| constant_name(first));
                let mut at = text("at");
                for argument in arguments(node) {
                    let elements: Vec<Node<'pr>> =
                        if let Some(hash) = argument.as_keyword_hash_node() {
                            hash.elements().iter().collect()
                        } else if let Some(hash) = argument.as_hash_node() {
                            hash.elements().iter().collect()
                        } else {
                            continue;
                        };
                    for element in elements {
                        if let Some(assoc) = element.as_assoc_node()
                            && let Some(constant) = constant_name(&assoc.key())
                        {
                            app = Some(constant);
                            at = lit(&assoc.value(), scope);
                        }
                    }
                }
                if let Some(app) = app {
                    let prefix = join(&[&scope.path, at.as_deref().unwrap_or("")]);
                    self.mounts.entry(app.clone()).or_default().push(prefix);
                    if app.ends_with("Engine") {
                        self.mounted.push(app);
                    }
                }
            }
            // The gem's macro routes to the controllers it is handed, each bounded to its own.
            "devise_for" => {
                for argument in arguments(node) {
                    let Some(hash) = argument.as_keyword_hash_node() else {
                        continue;
                    };
                    for element in hash.elements().iter() {
                        let Some(assoc) = element.as_assoc_node() else {
                            continue;
                        };
                        if lit(&assoc.key(), scope).as_deref() != Some("controllers") {
                            continue;
                        }
                        let value = assoc.value();
                        let pairs: Vec<Node<'pr>> = value
                            .as_hash_node()
                            .map(|hash| hash.elements().iter().collect())
                            .unwrap_or_default();
                        for pair in pairs {
                            let reach = pair
                                .as_assoc_node()
                                .and_then(|pair| lit(&pair.value(), scope))
                                .map_or(Reach::Any, |controller| {
                                    Reach::Controller(controller_path(&controller, scope))
                                });
                            self.unknown(
                                scope,
                                reach,
                                None,
                                Some((node, false)),
                                "a devise_for controller",
                            );
                        }
                    }
                }
            }
            verb if VERBS.contains(&verb) => self.verb(node, &args, &opts, rocket, scope),
            quiet if QUIET.contains(&quiet) => {}
            passing if PASSING.contains(&passing) => {
                let inner = if passing == "shallow" {
                    Scope {
                        shallow: true,
                        ..scope.clone()
                    }
                } else {
                    scope.clone()
                };
                self.walk(body, &inner);
            }
            _ => self.unknown_call(node, &name, &args, &opts, body, scope),
        }
    }

    /// A call the DSL does not have: a project macro that hands `resources` its first argument is
    /// bounded to that resource's controller; any other may draw anything.
    fn unknown_call(
        &mut self,
        node: &CallNode<'pr>,
        name: &str,
        args: &[Node<'pr>],
        opts: &BTreeMap<String, Opt>,
        body: Option<Node<'pr>>,
        scope: &Scope,
    ) {
        if let Some(Macro::Forwards { singular }) = self.context.macros.get(name)
            && let Some(resource) = args.first().and_then(|first| lit(first, scope))
        {
            let controller = opts
                .get("controller")
                .and_then(|opt| opt.text.clone())
                .unwrap_or_else(|| {
                    if *singular {
                        pluralize(&resource)
                    } else {
                        resource
                    }
                });
            let inner = match opts.get("module").and_then(|opt| opt.text.clone()) {
                Some(module) => Scope {
                    module: Some(join_module(scope.module.as_deref(), &module)),
                    ..scope.clone()
                },
                None => scope.clone(),
            };
            let reach = Reach::Controller(controller_path(&controller, &inner));
            self.unknown(scope, reach, None, None, "a project macro");
            if body.is_some() {
                self.walk_unknown(body, scope, node, "a project macro's block");
            }
            return;
        }
        if body.is_some() {
            self.walk_unknown(
                body,
                scope,
                node,
                "the block of a call the routes do not have",
            );
        }
        self.unknown(
            scope,
            Reach::Any,
            None,
            Some((node, true)),
            "a call the routes do not have",
        );
    }

    fn concerned(&mut self, concern: Option<String>, scope: &Scope, node: &CallNode<'pr>) {
        let body = concern
            .as_ref()
            .and_then(|concern| self.concerns.get(concern));
        match body {
            Some(body) => {
                let body = clone_node(body);
                self.walk(body, scope);
            }
            None => self.unknown(
                scope,
                Reach::Any,
                None,
                Some((node, false)),
                "a concern not found",
            ),
        }
    }

    fn verb(
        &mut self,
        node: &CallNode<'pr>,
        args: &[Node<'pr>],
        opts: &BTreeMap<String, Opt>,
        rocket: Option<Rocket>,
        scope: &Scope,
    ) {
        let text = |key: &str| opts.get(key).and_then(|opt| opt.text.clone());
        let (path, to) = match rocket {
            Some((path, to)) => (path, Some(to)),
            None => (
                args.first().and_then(|first| lit(first, scope)),
                opts.get("to").cloned(),
            ),
        };
        if to.as_ref().is_some_and(|to| to.rack) {
            return;
        }
        let (mut controller, mut action) = (None, None);
        if let Some(to) = &to {
            match &to.text {
                Some(target) => {
                    (controller, action) = split_to(Some(target), scope);
                    if !target.contains('#') {
                        action = Some(target.clone());
                    }
                }
                None => {
                    if let Some(head) = &to.controller_head {
                        let reach = Reach::Controller(controller_path(head, scope));
                        return self.unknown(
                            scope,
                            reach,
                            None,
                            None,
                            "a target with an interpolated action",
                        );
                    }
                    return self.unknown(
                        scope,
                        Reach::Any,
                        None,
                        Some((node, false)),
                        "a target the text does not hold",
                    );
                }
            }
        }
        if controller.is_none() {
            controller = text("controller").map(|written| controller_path(&written, scope));
        }
        if action.is_none() {
            action = text("action");
        }
        let res = scope.res.clone();
        let on = text("on");
        let mut base = scope.path.clone();
        if let Some(res) = &res {
            match on.as_deref() {
                Some("member") => {
                    base = res
                        .shallow_member
                        .clone()
                        .unwrap_or_else(|| res.member.clone());
                }
                Some("collection") => base = res.collection.clone(),
                Some("new") => base = join(&[&res.collection, "new"]),
                _ if scope.level == Level::Resources => base = res.nested.clone(),
                _ => {}
            }
        }
        if controller.is_none() {
            controller = res.as_ref().map(|res| res.controller.clone());
        }
        if controller.is_none() {
            controller = scope
                .controller
                .as_deref()
                .map(|written| controller_path(written, scope));
        }
        let Some(path) = path else {
            let reach = controller.map_or(Reach::Any, Reach::Controller);
            return self.unknown(
                scope,
                reach,
                action,
                Some((node, false)),
                "a path the text does not hold",
            );
        };
        if names_its_target(&path) {
            return self.unknown(
                scope,
                Reach::Any,
                None,
                Some((node, false)),
                "a path naming its own target",
            );
        }
        let stripped = path
            .strip_suffix("(.:format)")
            .unwrap_or(&path)
            .trim_start_matches('/');
        if action.is_none() && !stripped.is_empty() && stripped.bytes().all(word_or_dash) {
            action = Some(stripped.replace('-', "_"));
        }
        if controller.is_none()
            && action.is_none()
            && let Some((head, last)) = stripped.rsplit_once('/')
            && stripped
                .bytes()
                .all(|byte| word_or_dash(byte) || byte == b'/')
        {
            controller = Some(controller_path(&head.replace('-', "_"), scope));
            action = Some(last.replace('-', "_"));
        }
        // No controller written anywhere: Rails takes the scope's module.
        if controller.is_none() {
            controller = scope.module.clone().filter(|module| !module.is_empty());
        }
        let spec = join(&[&base, &path]);
        let extra = route_defaults(opts);
        self.emit(scope, &spec, controller, action, extra, node);
    }

    #[allow(clippy::too_many_arguments)]
    fn resources(
        &mut self,
        node: &CallNode<'pr>,
        singular: bool,
        args: &[Node<'pr>],
        opts: &BTreeMap<String, Opt>,
        body: Option<Node<'pr>>,
        scope: &Scope,
    ) {
        let names: Option<Vec<String>> = args.iter().map(|arg| lit(arg, scope)).collect();
        let names = match names {
            Some(names) if !names.is_empty() => names,
            _ => return self.walk_unknown(body, scope, node, "a resource's name"),
        };
        let only = opts.get("only").map(|opt| opt.list.clone());
        let except = opts.get("except").map(|opt| opt.list.clone());
        if matches!(only, Some(None)) || matches!(except, Some(None)) {
            return self.unknown(
                scope,
                Reach::Any,
                None,
                Some((node, false)),
                "only: or except: not literal",
            );
        }
        let (only, except) = (only.flatten(), except.flatten());
        let text = |key: &str| opts.get(key).and_then(|opt| opt.text.clone());
        for name in names {
            let segment = text("path").unwrap_or_else(|| name.clone());
            let controller = text("controller").unwrap_or_else(|| {
                if singular {
                    pluralize(&name)
                } else {
                    name.clone()
                }
            });
            let module = text("module");
            let controller_scope = match &module {
                Some(module) => Scope {
                    module: Some(join_module(scope.module.as_deref(), module)),
                    ..scope.clone()
                },
                None => scope.clone(),
            };
            let controller = controller_path(&controller, &controller_scope);
            let param = text("param").unwrap_or_else(|| "id".to_owned());
            let one = if singular {
                name.clone()
            } else {
                singularize(&name)
            };
            let base = match (&scope.res, scope.level) {
                (Some(res), Level::Resources) => res.nested.clone(),
                _ => scope.path.clone(),
            };
            let collection = join(&[&base, &segment]);
            let member = if singular {
                collection.clone()
            } else {
                join(&[&collection, &format!(":{param}")])
            };
            let nested = if singular {
                collection.clone()
            } else {
                join(&[&collection, &format!(":{one}_{param}")])
            };
            let own_shallow = opts.get("shallow").is_some_and(|opt| opt.yes);
            let shallow_now = (scope.shallow || own_shallow)
                && scope.res.is_some()
                && scope.level == Level::Resources;
            let shallow_member = (shallow_now && !singular)
                .then(|| join(&[&scope.shallow_path, &segment, &format!(":{param}")]));
            let all: &[&str] = if singular {
                &SINGULAR_ACTIONS
            } else {
                &PLURAL_ACTIONS
            };
            let actions: Vec<&str> = all
                .iter()
                .copied()
                .filter(|action| {
                    only.as_ref()
                        .is_none_or(|only| only.iter().any(|kept| kept == action))
                        && except
                            .as_ref()
                            .is_none_or(|except| !except.iter().any(|dropped| dropped == action))
                })
                .collect();
            let mut inner = scope.clone();
            if let Some(pairs) = opts.get("defaults").and_then(|opt| opt.pairs.as_ref()) {
                inner.defaults.extend(
                    pairs
                        .iter()
                        .filter_map(|(key, class)| Some((key.clone(), (*class)?))),
                );
            }
            let member_spec = shallow_member.clone().unwrap_or_else(|| member.clone());
            let (to_controller, to_action) = split_to(text("to").as_deref(), scope);
            let mut without_format = opts.clone();
            without_format.remove("format");
            for action in actions {
                let spec = match action {
                    "index" | "create" => collection.clone(),
                    "new" => join(&[&collection, "new"]),
                    "edit" => join(&[&member_spec, "edit"]),
                    _ => member_spec.clone(),
                };
                let extra = route_defaults(&without_format);
                self.emit(
                    &inner,
                    &spec,
                    Some(to_controller.clone().unwrap_or_else(|| controller.clone())),
                    Some(to_action.clone().unwrap_or_else(|| action.to_owned())),
                    extra,
                    node,
                );
            }
            let res = Res {
                controller: controller.clone(),
                collection: collection.clone(),
                member,
                nested,
                shallow_member,
            };
            let child = Scope {
                path: collection,
                res: Some(Rc::new(res)),
                level: Level::Resources,
                shallow: scope.shallow || own_shallow,
                module: if module.is_some() {
                    controller_scope.module.clone()
                } else {
                    scope.module.clone()
                },
                ..inner
            };
            if let Some(concerns) = opts.get("concerns") {
                for concern in concerns.list.clone().unwrap_or_default() {
                    self.concerned(Some(concern), &child, node);
                }
            }
            let body = node
                .block()
                .and_then(|block| block.as_block_node())
                .and_then(|block| block.body());
            self.walk(body, &child);
        }
    }
}

/// A block's body again, for one walked more than once (a concern, an engine's block): its
/// statements, or a `begin` where it rescues.
fn clone_node<'pr>(node: &Node<'pr>) -> Option<Node<'pr>> {
    node.as_statements_node()
        .map(|statements| statements.as_node())
        .or_else(|| node.as_begin_node().map(|begin| begin.as_node()))
}

/// A controller's path as written under the scope's module: `posts` in `namespace :admin` is
/// `admin/posts`, and `/posts` is `posts` wherever it is written.
fn controller_path(name: &str, scope: &Scope) -> String {
    if let Some(absolute) = name.strip_prefix('/') {
        return absolute.to_owned();
    }
    match scope.module.as_deref() {
        Some(module) if !module.is_empty() => format!("{module}/{name}"),
        _ => name.to_owned(),
    }
}

/// Whether a path names its own target (`/:controller/:action`), which may then be any.
fn names_its_target(path: &str) -> bool {
    [":controller", ":action"].iter().any(|word| {
        path.match_indices(word).any(|(at, _)| {
            path.as_bytes()
                .get(at + word.len())
                .is_none_or(|next| !(next.is_ascii_alphanumeric() || *next == b'_'))
        })
    })
}

fn word_or_dash(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// `admin` then `posts` is `admin/posts`.
fn join_module(outer: Option<&str>, inner: &str) -> String {
    match outer {
        Some(outer) if !outer.is_empty() => format!("{outer}/{inner}"),
        _ => inner.to_owned(),
    }
}

/// `"c#a"` as a controller (under the scope's module) and an action.
fn split_to(target: Option<&str>, scope: &Scope) -> (Option<String>, Option<String>) {
    match target.and_then(|target| target.split_once('#')) {
        Some((controller, action)) => (
            Some(controller_path(controller, scope)),
            Some(action.to_owned()),
        ),
        None => (None, None),
    }
}

/// The values a route's own options give a key: its `defaults:`, a `format:` written as a string,
/// and every option Rails does not read itself that is not a `Regexp`.
fn route_defaults(opts: &BTreeMap<String, Opt>) -> BTreeMap<String, &'static str> {
    let mut out = BTreeMap::new();
    if let Some(pairs) = opts.get("defaults").and_then(|opt| opt.pairs.as_ref()) {
        out.extend(
            pairs
                .iter()
                .filter_map(|(key, class)| Some((key.clone(), (*class)?))),
        );
    }
    if opts.get("format").and_then(|opt| opt.class) == Some("String") {
        out.insert("format".to_owned(), "String");
    }
    for (key, opt) in opts {
        if RESERVED.contains(&key.as_str()) || opt.pattern {
            continue;
        }
        if let Some(class) = opt.class {
            out.insert(key.clone(), class);
        }
    }
    out
}

/// A call's positional arguments: no hash, no `&block`.
fn positional<'pr>(call: &CallNode<'pr>) -> Vec<Node<'pr>> {
    arguments(call)
        .into_iter()
        .filter(|argument| {
            argument.as_keyword_hash_node().is_none()
                && argument.as_hash_node().is_none()
                && argument.as_block_argument_node().is_none()
        })
        .collect()
}

fn arguments<'pr>(call: &CallNode<'pr>) -> Vec<Node<'pr>> {
    call.arguments()
        .map(|arguments| arguments.arguments().iter().collect())
        .unwrap_or_default()
}

/// A constant's full name as written, without a leading `::`.
fn constant_name(node: &Node<'_>) -> Option<String> {
    if let Some(constant) = node.as_constant_read_node() {
        return Some(String::from_utf8_lossy(constant.name().as_slice()).into_owned());
    }
    let path = node.as_constant_path_node()?;
    let name = String::from_utf8_lossy(path.name()?.as_slice()).into_owned();
    match path.parent() {
        None => Some(name),
        Some(parent) => Some(format!("{}::{name}", constant_name(&parent)?)),
    }
}

/// Every `"c#a"` string literal a call writes.
#[derive(Default)]
struct Targets {
    targets: Vec<(String, String)>,
}

impl<'pr> Visit<'pr> for Targets {
    fn visit_string_node(&mut self, node: &ruby_prism::StringNode<'pr>) {
        let text = String::from_utf8_lossy(node.unescaped()).into_owned();
        if let Some((controller, action)) = text.split_once('#')
            && !controller.is_empty()
            && controller
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'/')
            && action
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && !self
                .targets
                .contains(&(controller.to_owned(), action.to_owned()))
        {
            self.targets
                .push((controller.to_owned(), action.to_owned()));
        }
    }
}

/// The names `draw` takes, anywhere in a file.
fn draws_in(source: &str) -> Vec<String> {
    #[derive(Default)]
    struct Draws {
        found: Vec<String>,
    }
    impl<'pr> Visit<'pr> for Draws {
        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            if node.name().as_slice() == b"draw"
                && node.receiver().is_none()
                && let Some(first) = arguments(node).first()
                && let Some(name) = lit(first, &Scope::root("/", None))
            {
                self.found.push(name);
            }
            ruby_prism::visit_call_node(self, node);
        }
    }
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut draws = Draws::default();
    draws.visit(&parsed.node());
    draws.found
}

/// The route blocks a file writes: `routes.draw`/`append`/`prepend` calls, outermost only.
fn route_blocks<'pr>(node: &Node<'pr>) -> Vec<CallNode<'pr>> {
    #[derive(Default)]
    struct Blocks<'pr> {
        found: Vec<CallNode<'pr>>,
    }
    impl<'pr> Visit<'pr> for Blocks<'pr> {
        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            let drawn = matches!(node.name().as_slice(), b"draw" | b"append" | b"prepend")
                && node.block().is_some()
                && node.receiver().is_some_and(|receiver| {
                    receiver
                        .as_call_node()
                        .is_some_and(|routes| routes.name().as_slice() == b"routes")
                });
            if drawn {
                self.found.extend(node.as_node().as_call_node());
                return;
            }
            ruby_prism::visit_call_node(self, node);
        }
    }
    let mut blocks = Blocks::default();
    blocks.visit(node);
    blocks.found
}

/// Every route the files draw, and every one they cannot be read for.
#[must_use]
pub fn read_targets(sources: &[RouteSource<'_>], context: &RouteContext<'_>) -> RouteTable {
    // Every file a `draw` reaches, read before the walk so a concern or an engine block written in
    // one outlives the call that reached it.
    let mut texts: Vec<(String, String)> = Vec::new();
    let mut pending: Vec<String> = sources
        .iter()
        .flat_map(|source| draws_in(source.text))
        .collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(text) = (context.drawn)(&name) {
            pending.extend(draws_in(&text));
            texts.push((name, text));
        }
    }
    let drawn: HashMap<String, ruby_prism::ParseResult<'_>> = texts
        .iter()
        .map(|(name, text)| (name.clone(), ruby_prism::parse(text.as_bytes())))
        .collect();
    let parsed: Vec<(bool, ruby_prism::ParseResult<'_>)> = sources
        .iter()
        .map(|source| (source.whole, ruby_prism::parse(source.text.as_bytes())))
        .collect();
    let mut walk = Walk {
        context,
        drawn: &drawn,
        concerns: HashMap::new(),
        engines: BTreeMap::new(),
        mounts: BTreeMap::new(),
        mounted: Vec::new(),
        table: RouteTable::default(),
    };
    for (whole, result) in &parsed {
        let program = result.node();
        if *whole {
            let statements = program
                .as_program_node()
                .map(|program| program.statements().as_node());
            walk.walk(statements, &Scope::root("/", None));
        } else {
            for block in route_blocks(&program) {
                walk.call(&block, &Scope::root("/", None));
            }
        }
    }
    // Engines: each block under every prefix its engine is mounted at, or none.
    let engines = std::mem::take(&mut walk.engines);
    for (engine, blocks) in &engines {
        let prefixes = walk
            .mounts
            .get(engine)
            .cloned()
            .unwrap_or_else(|| vec!["/".to_owned()]);
        let module = context.isolated.get(engine).and_then(|module| {
            module
                .split("::")
                .map(underscore)
                .collect::<Option<Vec<String>>>()
                .map(|segments| segments.join("/"))
        });
        for prefix in prefixes {
            for block in blocks {
                walk.walk(clone_node(block), &Scope::root(&prefix, module.clone()));
            }
        }
    }
    // An engine mounted from a gem: its routes are not in the text, and the project may reopen or
    // subclass its controllers, so every controller under its namespace is unreadable.
    for engine in std::mem::take(&mut walk.mounted) {
        if engines.contains_key(&engine) {
            continue;
        }
        let module = context.isolated.get(&engine).cloned().unwrap_or_else(|| {
            engine
                .strip_suffix("::Engine")
                .unwrap_or(&engine)
                .to_owned()
        });
        let namespace = module
            .split("::")
            .map(underscore)
            .collect::<Option<Vec<String>>>()
            .map_or(Reach::Any, |segments| Reach::Namespace(segments.join("/")));
        walk.table.unreadable.push(Unreadable {
            reach: namespace,
            action: None,
            call: None,
            why: "a gem engine's routes",
        });
    }
    walk.table
}

/// Every method a file calls with no receiver: what a routes file may hand to a project method.
#[must_use]
pub fn called_in(source: &str) -> BTreeSet<String> {
    #[derive(Default)]
    struct Calls {
        found: BTreeSet<String>,
    }
    impl<'pr> Visit<'pr> for Calls {
        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            if node.receiver().is_none() {
                self.found.insert(name_of(node));
            }
            ruby_prism::visit_call_node(self, node);
        }
    }
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut calls = Calls::default();
    calls.visit(&parsed.node());
    calls.found
}

/// What each project method a routes file may call does, from one file's `def`s: one handing its
/// first parameter to `resources`, and one whose last expression builds an object.
#[must_use]
pub fn read_route_macros(source: &str) -> BTreeMap<String, Macro> {
    struct Defs {
        found: BTreeMap<String, Macro>,
    }
    impl<'pr> Visit<'pr> for Defs {
        fn visit_def_node(&mut self, node: &ruby_prism::DefNode<'pr>) {
            let name = String::from_utf8_lossy(node.name().as_slice()).into_owned();
            let first = node
                .parameters()
                .and_then(|parameters| parameters.requireds().iter().next())
                .and_then(|first| first.as_required_parameter_node())
                .map(|first| first.name().as_slice().to_vec());
            if let (Some(first), Some(body)) = (first, node.body()) {
                let mut forwards = Forwards {
                    parameter: first,
                    found: None,
                };
                forwards.visit(&body);
                if let Some(singular) = forwards.found {
                    self.found
                        .insert(name.clone(), Macro::Forwards { singular });
                }
            }
            let last = node.body().and_then(|body| {
                body.as_statements_node()
                    .and_then(|statements| statements.body().iter().last())
            });
            if let Some(last) = last.as_ref().and_then(Node::as_call_node)
                && last.name().as_slice() == b"new"
                && last.receiver().is_some_and(|receiver| {
                    receiver.as_constant_read_node().is_some()
                        || receiver.as_constant_path_node().is_some()
                })
            {
                self.found.entry(name).or_insert(Macro::RackApp);
            }
            ruby_prism::visit_def_node(self, node);
        }
    }
    struct Forwards {
        parameter: Vec<u8>,
        found: Option<bool>,
    }
    impl<'pr> Visit<'pr> for Forwards {
        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            let name = node.name();
            let name = name.as_slice();
            if matches!(name, b"resources" | b"resource")
                && node.receiver().is_none()
                && arguments(node).first().is_some_and(|first| {
                    first
                        .as_local_variable_read_node()
                        .is_some_and(|local| local.name().as_slice() == self.parameter.as_slice())
                })
            {
                self.found = Some(name == b"resource");
            }
            ruby_prism::visit_call_node(self, node);
        }
    }
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut defs = Defs {
        found: BTreeMap::new(),
    };
    defs.visit(&parsed.node());
    defs.found
}

/// Each engine class one file writes `isolate_namespace` in, and the module it names, the class's
/// name joined from its nesting.
#[must_use]
pub fn read_isolated(source: &str) -> BTreeMap<String, String> {
    struct Isolated {
        nesting: Vec<String>,
        found: BTreeMap<String, String>,
    }
    impl<'pr> Visit<'pr> for Isolated {
        fn visit_module_node(&mut self, node: &ruby_prism::ModuleNode<'pr>) {
            let name = constant_name(&node.constant_path());
            if let Some(name) = name {
                self.nesting.push(name);
                ruby_prism::visit_module_node(self, node);
                self.nesting.pop();
            }
        }

        fn visit_class_node(&mut self, node: &ruby_prism::ClassNode<'pr>) {
            let Some(name) = constant_name(&node.constant_path()) else {
                return;
            };
            self.nesting.push(name);
            let full = self.nesting.join("::");
            for statement in node
                .body()
                .and_then(|body| body.as_statements_node())
                .map(|statements| statements.body().iter().collect::<Vec<_>>())
                .unwrap_or_default()
            {
                if let Some(call) = statement.as_call_node()
                    && call.name().as_slice() == b"isolate_namespace"
                    && let Some(module) = arguments(&call).first().and_then(constant_name)
                {
                    self.found.insert(full.clone(), module);
                }
            }
            ruby_prism::visit_class_node(self, node);
            self.nesting.pop();
        }
    }
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut isolated = Isolated {
        nesting: Vec::new(),
        found: BTreeMap::new(),
    };
    isolated.visit(&parsed.node());
    isolated.found
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// A table read from routes files, each read for its route blocks, `draw` reading `drawn`.
    fn table(sources: &[&str], drawn: &[(&str, &str)], macros: &[(&str, Macro)]) -> RouteTable {
        let drawn: BTreeMap<String, String> = drawn
            .iter()
            .map(|(name, text)| ((*name).to_owned(), (*text).to_owned()))
            .collect();
        let isolated = BTreeMap::from([("Shop::Engine".to_owned(), "Shop::Admin".to_owned())]);
        let macros: BTreeMap<String, Macro> = macros
            .iter()
            .map(|(name, kind)| ((*name).to_owned(), *kind))
            .collect();
        let lookup = |name: &str| drawn.get(name).cloned();
        let sources: Vec<RouteSource<'_>> = sources
            .iter()
            .map(|text| RouteSource { text, whole: false })
            .collect();
        read_targets(
            &sources,
            &RouteContext {
                drawn: &lookup,
                isolated: &isolated,
                macros: &macros,
            },
        )
    }

    /// Each route as `controller#action spec [required] {defaults}`.
    fn routes(table: &RouteTable) -> Vec<String> {
        table
            .targets
            .iter()
            .map(|target| {
                let defaults: Vec<String> = target
                    .defaults
                    .iter()
                    .map(|(key, class)| format!("{key}={class}"))
                    .collect();
                format!(
                    "{}#{} {} [{}] {{{}}}",
                    target.controller,
                    target.action,
                    target.spec,
                    target.required.join(","),
                    defaults.join(",")
                )
            })
            .collect()
    }

    fn unreadable(table: &RouteTable) -> Vec<String> {
        table
            .unreadable
            .iter()
            .map(|unreadable| {
                let reach = match &unreadable.reach {
                    Reach::Any => "*".to_owned(),
                    Reach::Controller(controller) => controller.clone(),
                    Reach::Namespace(namespace) => format!("{namespace}/*"),
                };
                format!(
                    "{reach}#{} {} {}",
                    unreadable.action.as_deref().unwrap_or("?"),
                    unreadable.why,
                    unreadable.call.as_deref().unwrap_or("-")
                )
            })
            .collect()
    }

    #[test]
    fn a_path_requires_the_segments_outside_its_parentheses() {
        assert_eq!(
            required_of("/posts/:post_id/comments(/:page)(.:format)/*rest/:a1_b"),
            ["post_id", "rest", "a1_b"]
        );
        assert_eq!(required_of("/a/:/b/*"), Vec::<String>::new());
        assert_eq!(join(&["/", "", "admin", "/posts"]), "/admin/posts");
        assert_eq!(join(&["a"]), "/a");
        assert_eq!(join(&["(/:locale)", "x"]), "(/:locale)/x");
        assert_eq!(join(&["/a/", "(//:b)"]), "/a(/:b)");
        assert!(names_its_target("/:controller/:action(/:id)"));
        assert!(names_its_target("/x(/:action)"));
        assert!(!names_its_target("/:controller_id/:actions"));
    }

    #[test]
    fn resources_draw_their_actions_under_their_scopes() {
        let source = "\
Rails.application.routes.draw do
  resources :posts, only: %i[index show], param: :slug do
    resources :comments, except: :destroy, shallow: true
    resource :cover, path: \"image\", controller: \"covers\"
    member do
      get :preview
    end
    collection do
      post :sort, on: :collection
    end
    new do
      get :draft
    end
    get :stats, on: :member
    get :feed, on: :collection
    get :blank, on: :new
    get \"chart\", to: \"charts#show\"
    namespace :admin do
      get :audit
    end
  end
  namespace :admin, path: \"manage\", module: \"staff\" do
    resources :users, module: \"people\", to: \"members#list\", defaults: { format: :json }
  end
  scope \"(:locale)\", module: \"web\", controller: \"pages\", defaults: { theme: \"dark\" } do
    get :about
    get \"help\", action: \"faq\"
  end
  scope path: \"v1\" do
    get \"x\", controller: \"/api\", action: \"x\", page: 1, constraint: /a/
  end
  scope path: nil do
    get \"y\", to: \"y#z\"
  end
  scope nil do
    get \"w\", to: \"w#z\"
  end
  controller :sessions do
    get \"login\", action: :new
  end
  defaults format: :rss do
    get \"feed\", to: \"feeds#index\"
  end
  with_options only: :show do
    resources :tags
  end
  shallow do
    resources :boards do
      resources :pins
    end
  end
  constraints(id: /x/) do
    get \"z\", to: \"z#z\", format: \"csv\"
  end
end
";
        let read = table(&[source], &[], &[]);
        assert_eq!(unreadable(&read), Vec::<String>::new());
        assert_eq!(
            routes(&read),
            [
                "posts#index /posts(.:format) [] {}",
                "posts#show /posts/:slug(.:format) [slug] {}",
                "comments#index /posts/:post_slug/comments(.:format) [post_slug] {}",
                "comments#create /posts/:post_slug/comments(.:format) [post_slug] {}",
                "comments#new /posts/:post_slug/comments/new(.:format) [post_slug] {}",
                "comments#show /comments/:id(.:format) [id] {}",
                "comments#update /comments/:id(.:format) [id] {}",
                "comments#edit /comments/:id/edit(.:format) [id] {}",
                "covers#show /posts/:post_slug/image(.:format) [post_slug] {}",
                "covers#create /posts/:post_slug/image(.:format) [post_slug] {}",
                "covers#update /posts/:post_slug/image(.:format) [post_slug] {}",
                "covers#destroy /posts/:post_slug/image(.:format) [post_slug] {}",
                "covers#new /posts/:post_slug/image/new(.:format) [post_slug] {}",
                "covers#edit /posts/:post_slug/image/edit(.:format) [post_slug] {}",
                "posts#preview /posts/:slug/preview(.:format) [slug] {}",
                "posts#sort /posts/sort(.:format) [] {}",
                "posts#draft /posts/new/draft(.:format) [] {}",
                "posts#stats /posts/:slug/stats(.:format) [slug] {}",
                "posts#feed /posts/feed(.:format) [] {}",
                "posts#blank /posts/new/blank(.:format) [] {}",
                "charts#show /posts/:post_slug/chart(.:format) [post_slug] {}",
                "admin#audit /posts/:post_slug/admin/audit(.:format) [post_slug] {}",
                // `to:` is read in the scope around the resource, without its own `module:`, as
                // Rails' `resources … to:` reads it.
                "staff/members#list /manage/users(.:format) [] {format=Symbol}",
                "staff/members#list /manage/users(.:format) [] {format=Symbol}",
                "staff/members#list /manage/users/new(.:format) [] {format=Symbol}",
                "staff/members#list /manage/users/:id(.:format) [id] {format=Symbol}",
                "staff/members#list /manage/users/:id(.:format) [id] {format=Symbol}",
                "staff/members#list /manage/users/:id(.:format) [id] {format=Symbol}",
                "staff/members#list /manage/users/:id/edit(.:format) [id] {format=Symbol}",
                "web/pages#about /(:locale)/about(.:format) [] {theme=String}",
                "web/pages#faq /(:locale)/help(.:format) [] {theme=String}",
                "api#x /v1/x(.:format) [] {page=Integer}",
                "y#z /y(.:format) [] {}",
                "w#z /w(.:format) [] {}",
                "sessions#new /login(.:format) [] {}",
                "feeds#index /feed(.:format) [] {format=Symbol}",
                "tags#show /tags/:id(.:format) [id] {}",
                "boards#index /boards(.:format) [] {}",
                "boards#create /boards(.:format) [] {}",
                "boards#new /boards/new(.:format) [] {}",
                "boards#show /boards/:id(.:format) [id] {}",
                "boards#update /boards/:id(.:format) [id] {}",
                "boards#destroy /boards/:id(.:format) [id] {}",
                "boards#edit /boards/:id/edit(.:format) [id] {}",
                "pins#index /boards/:board_id/pins(.:format) [board_id] {}",
                "pins#create /boards/:board_id/pins(.:format) [board_id] {}",
                "pins#new /boards/:board_id/pins/new(.:format) [board_id] {}",
                "pins#show /pins/:id(.:format) [id] {}",
                "pins#update /pins/:id(.:format) [id] {}",
                "pins#destroy /pins/:id(.:format) [id] {}",
                "pins#edit /pins/:id/edit(.:format) [id] {}",
                "z#z /z(.:format) [] {format=String}",
            ]
        );
    }

    #[test]
    fn a_route_whose_target_or_path_the_text_does_not_hold_is_unreadable() {
        let source = "\
Rails.application.routes.draw do
  get \"r\", to: redirect(\"/x\")
  get \"l\", to: ->(env) { [200, {}, []] }
  get \"a\", to: RackApp
  get \"p\", to: Proc.new { }
  get \"h\", to: rack_helper
  root to: redirect(\"/home\")
  get \"v\", to: some_value
  get \"i\", to: \"posts##{action}\"
  get \"j\", to: \"#{x}\", as: :j
  get \"k\", to: \"#{x}\" + \"posts#show\"
  get path_var, to: \"posts#index\"
  get \":controller/:action\"
  get :plain
  get \"admin/reports\"
  get \"/a-b\", controller: :things
  get \"c/d\", controller: :things
  root \"home#index\"
  root to: \"home##{x}\"
  scope controller: :pages do
    root action: :index
  end
  root
  namespace(name) { get :x, to: \"n#x\" }
  scope(path) { get :y, to: \"s#y\" }
  controller(name) { get :z }
  resources(name) { get :w, to: \"r#w\" }
  resources :things, only: list
  [1].each { |n| get \"x#{n}\", to: \"loop#x\" }
  items.each { |n| get \"y\", to: \"loop#y\" }
  %w[a b].each do |kind|
    get kind, to: \"kinds##{kind}\"
  end
  devise_for :users, controllers: { sessions: \"users/sessions\", other: helper }
  my_macro :x
  my_macro do
    get \"m\", to: \"m#m\"
  end
  admin_resources :orders, module: \"admin\"
  admin_resource :profile, controller: \"accounts\" do
    get \"q\", to: \"q#q\"
  end
  admin_resources nil
  concerns :missing
  draw computed
  draw :absent
  mount Shop::Engine, at: \"/shop\"
  mount Blazer::Engine => \"/blazer\"
  mount Sidekiq::Web => \"/sidekiq\"
  mount Other::Engine, at: \"/other\"
  direct(:x) { }
  get \"\", to: \"empty#x\"
  require \"x\"
end
Shop::Engine.routes.draw do
  resources :orders, only: :show
end
module Plain
  def helper; end
end
";
        let read = table(
            &[source],
            &[],
            &[
                ("admin_resources", Macro::Forwards { singular: false }),
                ("admin_resource", Macro::Forwards { singular: true }),
                ("rack_helper", Macro::RackApp),
            ],
        );
        let routes = routes(&read);
        for expected in [
            "loop#x /x1(.:format) [] {}",
            "admin#reports /admin/reports(.:format) [] {}",
            "things#a_b /a-b(.:format) [] {}",
            "home#index /(.:format) [] {}",
            "pages#index /(.:format) [] {}",
            "kinds#a /a(.:format) [] {}",
            "kinds#b /b(.:format) [] {}",
            "shop/admin/orders#show /shop/orders/:id(.:format) [id] {}",
            "empty#x /(.:format) [] {}",
        ] {
            assert!(
                routes.contains(&expected.to_owned()),
                "{expected}\n{routes:#?}"
            );
        }
        assert!(
            !routes.iter().any(|route| route.starts_with("loop#y")),
            "{routes:#?}"
        );
        assert_eq!(
            unreadable(&read),
            [
                "*#? a target the text does not hold -",
                "posts#? a target with an interpolated action -",
                "*#? a target the text does not hold -",
                // The `"c#a"` its text writes bounds what it reaches.
                "posts#show a target the text does not hold -",
                "posts#index a path the text does not hold -",
                "*#? a path naming its own target -",
                "*#plain no target -",
                "things#? no target -",
                "home#? root to an interpolated action -",
                "*#? no target -",
                "n#? a namespace's name -",
                "s#? a scope's path -",
                // Under a name the text cannot read, a route its text does read still lands
                // nowhere it can name.
                "*#z no target -",
                "r#? a resource's name -",
                "*#? only: or except: not literal -",
                "loop#? a loop over a value -",
                "users/sessions#? a devise_for controller -",
                "*#? a devise_for controller -",
                "*#? a call the routes do not have my_macro",
                "m#? the block of a call the routes do not have -",
                "*#? a call the routes do not have my_macro",
                "admin/orders#? a project macro -",
                "accounts#? a project macro -",
                "q#? a project macro's block -",
                "*#? a call the routes do not have admin_resources",
                "*#? a concern not found -",
                "*#? a drawn file's name -",
                "*#? a drawn file missing -",
                "blazer/*#? a gem engine's routes -",
                "other/*#? a gem engine's routes -",
            ]
        );
    }

    #[test]
    fn a_drawn_file_and_a_concern_are_read_where_they_are_named() {
        let main = "\
Rails.application.routes.draw do
  concern :commentable do
    resources :comments, only: :index
  end
  namespace :admin do
    draw :admin
  end
  resources :posts, concerns: :commentable
  resources :photos, concerns: [:commentable, :missing]
  resources :books do
    concerns :commentable
  end
  begin
    get \"x\", to: \"x#x\"
  rescue StandardError
  end
  (get \"y\", to: \"y#y\")
  if ENV[\"A\"]
    get \"a\", to: \"a#a\"
  elsif other
    get \"b\", to: \"b#b\"
  else
    get \"c\", to: \"c#c\"
  end
  unless ENV[\"D\"]
    get \"d\", to: \"d#d\"
  else
    get \"e\", to: \"e#e\"
  end
  case ENV[\"F\"]
  when \"1\" then get \"f\", to: \"f#f\"
  else get \"g\", to: \"g#g\"
  end
  prefix = \"p\"
  get prefix, to: \"p#p\"
  prefix = computed
  get prefix, to: \"q#q\"
  get \"#{\"i\"}#{1}\", to: \"i#i\"
  get \"#{a; b}\", to: \"j#j\"
  get :\"sym\", to: \"k#k\"
  client.routes do
    get \"o\", to: \"o#o\"
  end
end
";
        let drawn = "resources :users, only: :show\ndraw :nested\n";
        let read = table(
            &[
                main,
                "Rails.application.routes.prepend do\n  get \"pre\", to: \"pre#x\"\nend\n",
            ],
            &[("admin", drawn), ("nested", "get \"n\", to: \"n#n\"\n")],
            &[],
        );
        let routes = routes(&read);
        for expected in [
            "admin/users#show /admin/users/:id(.:format) [id] {}",
            "admin/n#n /admin/n(.:format) [] {}",
            "comments#index /posts/:post_id/comments(.:format) [post_id] {}",
            "comments#index /photos/:photo_id/comments(.:format) [photo_id] {}",
            "comments#index /books/:book_id/comments(.:format) [book_id] {}",
            "x#x /x(.:format) [] {}",
            "y#y /y(.:format) [] {}",
            "a#a /a(.:format) [] {}",
            "b#b /b(.:format) [] {}",
            "c#c /c(.:format) [] {}",
            "d#d /d(.:format) [] {}",
            "e#e /e(.:format) [] {}",
            "f#f /f(.:format) [] {}",
            "g#g /g(.:format) [] {}",
            "p#p /p(.:format) [] {}",
            "i#i /i1(.:format) [] {}",
            "k#k /sym(.:format) [] {}",
            "o#o /o(.:format) [] {}",
            "pre#x /pre(.:format) [] {}",
        ] {
            assert!(
                routes.contains(&expected.to_owned()),
                "{expected}\n{routes:#?}"
            );
        }
        assert_eq!(
            unreadable(&read),
            [
                "*#? a concern not found -",
                "q#q a path the text does not hold -",
                "j#j a path the text does not hold -",
            ]
        );
    }

    #[test]
    fn a_projects_route_methods_are_read_from_their_defs() {
        let source = "\
module Routing
  def admin_resources(resource, **options)
    namespace :admin do
      resources resource, **options
    end
  end

  def admin_resource(resource)
    resource resource
  end

  def rack_app
    Builder.new
  end

  def scoped_app
    Rack::Builder.new
  end

  def plain(x)
    x
  end

  def other(name)
    resources :fixed
    router.resources name
  end

  def builds_lowercase
    builder.new
  end

  def nothing; end
end
";
        assert_eq!(
            read_route_macros(source),
            BTreeMap::from([
                (
                    "admin_resource".to_owned(),
                    Macro::Forwards { singular: true }
                ),
                (
                    "admin_resources".to_owned(),
                    Macro::Forwards { singular: false }
                ),
                ("rack_app".to_owned(), Macro::RackApp),
                ("scoped_app".to_owned(), Macro::RackApp),
            ])
        );
        assert_eq!(
            read_isolated(
                "module Shop\n  module Admin\n    class Engine < Rails::Engine\n      \
                 isolate_namespace Shop::Admin\n      other\n    end\n  end\nend\n\
                 class Plain\n  isolate_namespace\nend\nclass << self\nend\nmodule (x)::Y\nend\n"
            ),
            BTreeMap::from([("Shop::Admin::Engine".to_owned(), "Shop::Admin".to_owned())])
        );
        assert_eq!(
            called_in("draw :x\nadmin_resources :orders\nfoo.bar\n"),
            BTreeSet::from([
                "admin_resources".to_owned(),
                "draw".to_owned(),
                "foo".to_owned()
            ])
        );
        assert_eq!(draws_in("draw :admin\ndraw name\nx.draw :y\n"), ["admin"]);
    }
    #[test]
    fn the_rest_of_the_dsl_reads_as_rails_reads_it() {
        let main = "\
Rails.application.routes.draw do
  ROUTES_VERSION
  local_path = \"lp\"
  get \"r1\" => \"a#r1\"
  get \"#{\"r2\"}\" => \"a#r2\"
  get local_path => \"a#r3\"
  get \"r4\", { to: \"a#r4\" }
  get \"r5\", to: \"a#r5\", **opts
  get \"r6(.:format)\", to: \"a#r6\"
  get \"r7.:format\", to: \"a#r7\", flag: false, kind: computed, bare: nil
  get \"r8\", to: \"show\", controller: \"a\"
  get \"r9\", to: Rack::App
  get \"r10\", to: \"#{x}\"
  get \"r11\", to: \"a#{x}\"
  get \"/\", controller: :home
  get \"a.b\", controller: :things
  get \"under_score\", to: \"a#u\"
  get \"/q/r\"
  get \"x/y.z\"
  get \"a.c\"
  controller :snakes do
    get \"snake_case\"
  end
  scope module: \"\" do
    namespace :inner do
      get \"i\", to: \"i#i\"
    end
  end
  mount({ Other::Engine => \"/o\" })
  get \"v2\", to: some_value, other: \"admin_x/p#q\"
  mount ::Blazer2::Engine, at: \"/b\"
  inner.draw do
    Admin::Engine.routes.draw
  end
  resource :profile
  resources
  resources name_var
  member do
    get :loose
  end
  scope path: computed do
    get \"s\", to: \"s#s\"
  end
  scope module: \"\" do
    get \"t\", to: \"t#t\"
  end
  defaults kind: :k, other: computed do
    get \"d\", to: \"d#d\"
  end
  concern name_var do
  end
  concern :empty
  concern :rescued do
    get \"c\", to: \"c#c\"
  rescue StandardError
  end
  resources :posts, concerns: :rescued
  mount at: \"/nowhere\"
  mount Admin::Engine, **opts
  mount(Shop::Engine => \"/shop\")
  devise_for :users, path: \"u\", **opts
  devise_for :admins, controllers: helpers
  admin_resource :account
  foo.draw do
    get \"f\", to: \"f#f\"
  end
  foo.bar
  each do |x|
    get \"e\", to: \"e#e\"
  end
  [1].each(&blk)
  draw :twice
  draw :twice
  draw
end
Rails.routes.draw do
  get \"rails\", to: \"r#r\"
end
Shop::Application.routes.draw do
  get \"shop\", to: \"s#s\"
end
Lonely::Engine.routes.draw do
  get \"lonely\", to: \"l#l\"
end
Admin::Engine.routes.draw
Rails.application.routes.draw
";
        let read = table(
            &[main],
            &[("twice", "get \"tw\", to: \"tw#tw\"\n")],
            &[("admin_resource", Macro::Forwards { singular: true })],
        );
        let routes = routes(&read);
        for expected in [
            "a#r1 /r1(.:format) [] {}",
            "a#r2 /r2(.:format) [] {}",
            "a#r3 /lp(.:format) [] {}",
            "a#r4 /r4(.:format) [] {}",
            "a#r5 /r5(.:format) [] {}",
            "a#r6 /r6(.:format) [] {}",
            "a#r7 /r7.:format [format] {bare=nil,flag=bool}",
            "a#show /r8(.:format) [] {}",
            "a#u /under_score(.:format) [] {}",
            "q#r /q/r(.:format) [] {}",
            "profiles#show /profile(.:format) [] {}",
            "t#t /t(.:format) [] {}",
            "d#d /d(.:format) [] {kind=Symbol}",
            "c#c /posts/:post_id/c(.:format) [post_id] {}",
            "f#f /f(.:format) [] {}",
            "tw#tw /tw(.:format) [] {}",
            "r#r /rails(.:format) [] {}",
            "s#s /shop(.:format) [] {}",
            "l#l /lonely(.:format) [] {}",
            "snakes#snake_case /snake_case(.:format) [] {}",
            "inner/i#i /inner/i(.:format) [] {}",
        ] {
            assert!(
                routes.contains(&expected.to_owned()),
                "{expected}\n{routes:#?}"
            );
        }
        assert_eq!(
            routes
                .iter()
                .filter(|route| route.starts_with("tw#"))
                .count(),
            2
        );
        let unreadable = unreadable(&read);
        for expected in [
            "*#? a target the text does not hold -",
            "home#? no target -",
            "things#? no target -",
            "*#? no target -",
            "*#loose no target -",
            "*#? a resource's name -",
            "s#? a scope's path -",
            "accounts#? a project macro -",
            "*#? a drawn file's name -",
            // `each` with no receiver is no loop the text can bound.
            "e#? the block of a call the routes do not have -",
            "*#? a call the routes do not have each",
            "admin_x/p#q a target the text does not hold -",
            "blazer2/*#? a gem engine's routes -",
            "other/*#? a gem engine's routes -",
        ] {
            assert!(
                unreadable.contains(&expected.to_owned()),
                "{expected}\n{unreadable:#?}"
            );
        }
        assert!(
            !routes
                .iter()
                .any(|route| route.contains("r9") || route.contains("r10"))
        );
    }

    #[test]
    fn a_whole_file_is_read_statement_by_statement() {
        let none = |_: &str| None;
        let empty = BTreeMap::new();
        let macros: BTreeMap<String, Macro> = BTreeMap::new();
        let read = read_targets(
            &[RouteSource {
                text: "get \"w\", to: \"w#w\"\n",
                whole: true,
            }],
            &RouteContext {
                drawn: &none,
                isolated: &empty,
                macros: &macros,
            },
        );
        assert_eq!(routes(&read), ["w#w /w(.:format) [] {}"]);
    }

    #[test]
    fn a_text_s_targets_and_names_are_read_strictly() {
        let read = table(
            &[
                "Rails.application.routes.draw do\n  odd \"#a\", \"x y#z\", \"x#y z\", \"p#q\", \"p#q\"\nend\n",
            ],
            &[],
            &[],
        );
        // A call the routes do not have is bounded by nothing its text writes.
        assert_eq!(unreadable(&read), ["*#? a call the routes do not have odd"]);
        let read = table(
            &[
                "Rails.application.routes.draw do\n  get \"v\", to: some_value, as: \"#a\", via: \"x y#z\", on: \"x#y z\", other: \"p#q\", again: \"p#q\"\nend\n",
            ],
            &[],
            &[],
        );
        // Only a well-formed `"c#a"` bounds it, once.
        assert_eq!(unreadable(&read), ["p#q a target the text does not hold -"]);
        assert_eq!(draws_in("draw\n"), Vec::<String>::new());
        assert_eq!(
            called_in("x.y\nz(1)\n"),
            BTreeSet::from(["x".to_owned(), "z".to_owned()])
        );
        assert_eq!(
            read_isolated(
                "class foo::Engine\nend\nclass Engine\n  X = 1\n  isolate_namespace Mod\nend\n"
            ),
            BTreeMap::from([("Engine".to_owned(), "Mod".to_owned())])
        );
    }
}
