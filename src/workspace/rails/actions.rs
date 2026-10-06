//! Which actions a controller's `def` runs under, and whether every route reaching them gives a key.
//!
//! A read of `params[:id]` is a `String` only where every request that runs it was routed through a
//! path requiring `:id` (or a default giving it). Which requests run a `def` is Rails' dispatch:
//!
//! 1. **A public `def` is an action** of its class and of every subclass that does not write its
//!    own.
//! 2. **A callback** (`before_action :set_post, only: %i[show]`) runs it under the actions its
//!    `only:`/`except:` list, or every routed action of its class and subclasses.
//! 3. **A private helper** runs under its callers' actions: the bare calls of its name in the class,
//!    its ancestors and descendants, followed to a fixpoint.
//!
//! **Every doubt refuses**: a `def` in a module (a concern's includers are not read here), a
//! singleton `def`, a callback in a module or with a non-literal `only:`, a name a symbol hands
//! to `send`, `try`, `method`, `layout`, `with:`, `if:` or `unless:` (it may run anywhere), a
//! `helper_method` (views call it), a caller whose own actions are unknown, a route the reader could
//! not read that may reach one of the actions, and no route at all.

use std::collections::{BTreeMap, BTreeSet};

use ruby_prism::{CallNode, Node, Visit};

use super::inflect::camelize;
use super::syntax::constant_spelling;
use super::targets::{Reach, RouteTable};

/// The callbacks that run a method under an action.
const CALLBACKS: [&str; 12] = [
    "before_action",
    "prepend_before_action",
    "append_before_action",
    "around_action",
    "prepend_around_action",
    "append_around_action",
    "after_action",
    "prepend_after_action",
    "append_after_action",
    "before_filter",
    "around_filter",
    "after_filter",
];

/// Calls whose first symbol names a method that then runs wherever the call does.
const NAMING: [&str; 8] = [
    "send",
    "public_send",
    "__send__",
    "try",
    "try!",
    "method",
    "layout",
    "respond_to?",
];

/// The keys whose symbol names a method run wherever the call runs.
const NAMING_KEYS: [&str; 3] = ["with", "if", "unless"];

/// A class Rails defines that a controller inherits straight from: none of its ancestors is a gem's
/// controller a gem's routing macro reaches.
const BASES: [&str; 4] = [
    "ActionController::Base",
    "ActionController::API",
    "ActionController::Metal",
    "ApplicationController",
];

/// A callback's `only:` or `except:` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// Not written.
    None,
    /// Written as literals.
    Listed(Vec<String>),
    /// Written as something only running Ruby knows.
    Unknown,
}

/// One class body, as written: its nesting and its superclass's spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenClass {
    pub nesting: Vec<String>,
    pub superclass: Option<String>,
}

/// One `def`, by the nesting it is written in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenDef {
    pub owner: Vec<String>,
    pub name: String,
    pub public: bool,
    pub singleton: bool,
}

/// One callback declaration: the method it names, or `None` for a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenCallback {
    pub owner: Vec<String>,
    pub method: Option<String>,
    pub only: Filter,
    pub except: Filter,
}

/// What one file says about the controllers it writes ([`read_controllers`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Written {
    pub classes: Vec<WrittenClass>,
    pub modules: Vec<Vec<String>>,
    /// Each `include` in a body: the body's nesting and the module's spelling.
    pub includes: Vec<(Vec<String>, String)>,
    pub defs: Vec<WrittenDef>,
    pub callbacks: Vec<WrittenCallback>,
    /// Each bare call (no receiver, or `self.`) made in a `def`: its name, the body, the `def`.
    pub bare: Vec<(String, Vec<String>, String)>,
    /// Each method a symbol names for a call that runs it wherever the call runs.
    pub named: BTreeSet<String>,
    /// Each `helper_method`: the body and the name.
    pub helpers: Vec<(Vec<String>, String)>,
}

/// Every controller body, `def`, callback and call one file writes.
#[must_use]
pub fn read_controllers(source: &str) -> Written {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut reader = Reader {
        source,
        nesting: Vec::new(),
        public: vec![true],
        singleton: 0,
        def: None,
        found: Written::default(),
    };
    reader.visit(&parsed.node());
    reader.found
}

struct Reader<'s> {
    source: &'s str,
    nesting: Vec<String>,
    /// Whether a `def` written now is public, per body.
    public: Vec<bool>,
    /// How many `class << self` the walk is inside.
    singleton: u32,
    /// The `def` the walk is inside.
    def: Option<String>,
    found: Written,
}

fn literal(node: &Node<'_>) -> Option<String> {
    if let Some(symbol) = node.as_symbol_node() {
        return Some(String::from_utf8_lossy(symbol.unescaped()).into_owned());
    }
    let string = node.as_string_node()?;
    Some(String::from_utf8_lossy(string.unescaped()).into_owned())
}

fn filter_of(node: &Node<'_>) -> Filter {
    if let Some(array) = node.as_array_node() {
        return array
            .elements()
            .iter()
            .map(|element| literal(&element))
            .collect::<Option<Vec<String>>>()
            .map_or(Filter::Unknown, Filter::Listed);
    }
    literal(node).map_or(Filter::Unknown, |one| Filter::Listed(vec![one]))
}

fn arguments<'pr>(call: &CallNode<'pr>) -> Vec<Node<'pr>> {
    call.arguments()
        .map(|arguments| arguments.arguments().iter().collect())
        .unwrap_or_default()
}

impl Reader<'_> {
    fn body(
        &mut self,
        name: &Node<'_>,
        class: Option<Option<String>>,
        walk: &mut dyn FnMut(&mut Self),
    ) {
        self.nesting.push(constant_spelling(self.source, name));
        match class {
            Some(superclass) => self.found.classes.push(WrittenClass {
                nesting: self.nesting.clone(),
                superclass,
            }),
            None => self.found.modules.push(self.nesting.clone()),
        }
        self.public.push(true);
        let def = self.def.take();
        walk(self);
        self.def = def;
        self.public.pop();
        self.nesting.pop();
    }

    /// A call written straight in a body: visibility, callbacks, `helper_method`, `include`.
    fn statement(&mut self, node: &CallNode<'_>, name: &str) {
        let args = arguments(node);
        match name {
            "private" | "protected" | "public" if args.is_empty() => {
                // One entry per body, the file's own at the bottom.
                let innermost = self.public.len() - 1;
                self.public[innermost] = name == "public";
            }
            "private" | "protected" | "public" => {
                for argument in &args {
                    if let Some(target) = literal(argument) {
                        let public = name == "public";
                        for def in self
                            .found
                            .defs
                            .iter_mut()
                            .filter(|def| def.owner == self.nesting && def.name == target)
                        {
                            def.public = public;
                        }
                    }
                }
            }
            "helper_method" => {
                for argument in &args {
                    if let Some(method) = literal(argument) {
                        self.found.helpers.push((self.nesting.clone(), method));
                    }
                }
            }
            "include" | "prepend" => {
                for argument in &args {
                    if argument.as_constant_read_node().is_some()
                        || argument.as_constant_path_node().is_some()
                    {
                        let module = constant_spelling(self.source, argument);
                        self.found.includes.push((self.nesting.clone(), module));
                    }
                }
            }
            callback if CALLBACKS.contains(&callback) => {
                let (mut only, mut except) = (Filter::None, Filter::None);
                for argument in &args {
                    let Some(hash) = argument.as_keyword_hash_node() else {
                        continue;
                    };
                    for element in hash.elements().iter() {
                        let Some(assoc) = element.as_assoc_node() else {
                            continue;
                        };
                        match literal(&assoc.key()).as_deref() {
                            Some("only") => only = filter_of(&assoc.value()),
                            Some("except") => except = filter_of(&assoc.value()),
                            _ => {}
                        }
                    }
                }
                let methods: Vec<Option<String>> = args
                    .iter()
                    .filter_map(literal)
                    .map(Some)
                    .chain(node.block().is_some().then_some(None))
                    .collect();
                for method in methods {
                    self.found.callbacks.push(WrittenCallback {
                        owner: self.nesting.clone(),
                        method,
                        only: only.clone(),
                        except: except.clone(),
                    });
                }
            }
            _ => {}
        }
    }
}

impl<'pr> Visit<'pr> for Reader<'_> {
    fn visit_class_node(&mut self, node: &ruby_prism::ClassNode<'pr>) {
        let superclass = node
            .superclass()
            .map(|superclass| constant_spelling(self.source, &superclass));
        let name = node.constant_path();
        // One body walker for both kinds, so a class and a module share its lines.
        self.body(&name, Some(superclass), &mut |this| {
            ruby_prism::visit_class_node(this, node);
        });
    }

    fn visit_module_node(&mut self, node: &ruby_prism::ModuleNode<'pr>) {
        let name = node.constant_path();
        self.body(&name, None, &mut |this| {
            ruby_prism::visit_module_node(this, node);
        });
    }

    fn visit_singleton_class_node(&mut self, node: &ruby_prism::SingletonClassNode<'pr>) {
        self.singleton += 1;
        self.public.push(true);
        ruby_prism::visit_singleton_class_node(self, node);
        self.public.pop();
        self.singleton -= 1;
    }

    fn visit_def_node(&mut self, node: &ruby_prism::DefNode<'pr>) {
        let name = String::from_utf8_lossy(node.name().as_slice()).into_owned();
        self.found.defs.push(WrittenDef {
            owner: self.nesting.clone(),
            name: name.clone(),
            public: self.public.last().copied().unwrap_or(true),
            singleton: node.receiver().is_some() || self.singleton > 0,
        });
        let outer = self.def.replace(name);
        ruby_prism::visit_def_node(self, node);
        self.def = outer;
    }

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        let name = String::from_utf8_lossy(node.name().as_slice()).into_owned();
        let bare = node
            .receiver()
            .is_none_or(|receiver| receiver.as_self_node().is_some());
        if self.def.is_none() && node.receiver().is_none() {
            self.statement(node, &name);
            // `private def x` is visited with its visibility.
            if matches!(name.as_str(), "private" | "protected" | "public")
                && let Some(def) = arguments(node).first().and_then(Node::as_def_node)
            {
                self.public.push(name == "public");
                self.visit_def_node(&def);
                self.public.pop();
                return;
            }
        }
        if let Some(def) = &self.def
            && bare
        {
            self.found
                .bare
                .push((name.clone(), self.nesting.clone(), def.clone()));
        }
        let args = arguments(node);
        if NAMING.contains(&name.as_str())
            && let Some(first) = args.first().and_then(|first| first.as_symbol_node())
        {
            self.found
                .named
                .insert(String::from_utf8_lossy(first.unescaped()).into_owned());
        }
        for argument in &args {
            let Some(hash) = argument.as_keyword_hash_node() else {
                continue;
            };
            for element in hash.elements().iter() {
                if let Some(assoc) = element.as_assoc_node()
                    && literal(&assoc.key()).is_some_and(|key| NAMING_KEYS.contains(&key.as_str()))
                    && let Some(symbol) = assoc.value().as_symbol_node()
                {
                    self.found
                        .named
                        .insert(String::from_utf8_lossy(symbol.unescaped()).into_owned());
                }
            }
        }
        ruby_prism::visit_call_node(self, node);
    }
}

/// A body of [`Controllers`]: a class or a module, by its resolved name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Body {
    module: bool,
    superclass: Option<String>,
}

/// One callback, its body resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Callback {
    owner: String,
    method: Option<String>,
    only: Filter,
    except: Filter,
}

/// Every controller body the project writes, each name resolved, and every route.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Controllers {
    bodies: BTreeMap<String, Body>,
    /// `(body, name)` → whether any `def` of it there is public.
    defs: BTreeMap<(String, String), bool>,
    /// `(body, name)` of singleton `def`s, which are no action.
    singletons: BTreeSet<(String, String)>,
    callbacks: Vec<Callback>,
    /// Name → every `(body, def)` that calls it bare.
    bare: BTreeMap<String, BTreeSet<(String, String)>>,
    named: BTreeSet<String>,
    helpers: BTreeSet<(String, String)>,
}

impl Controllers {
    /// One file's bodies, each nesting resolved by `resolve` (the way Ruby's constant lookup
    /// does) and each superclass by `superclass` (from the nesting it is written in).
    pub fn absorb(
        &mut self,
        written: &Written,
        resolve: &dyn Fn(&[String]) -> String,
        superclass: &dyn Fn(&[String], &str) -> String,
    ) {
        for class in &written.classes {
            let name = resolve(&class.nesting);
            let outer = &class.nesting[..class.nesting.len() - 1];
            let parent = class
                .superclass
                .as_deref()
                .map(|written| superclass(outer, written));
            let body = self.bodies.entry(name).or_insert(Body {
                module: false,
                superclass: None,
            });
            if parent.is_some() {
                body.superclass = parent;
            }
        }
        for module in &written.modules {
            self.bodies.entry(resolve(module)).or_insert(Body {
                module: true,
                superclass: None,
            });
        }
        for def in &written.defs {
            let key = (resolve(&def.owner), def.name.clone());
            if def.singleton {
                self.singletons.insert(key);
            } else {
                *self.defs.entry(key).or_insert(false) |= def.public;
            }
        }
        for callback in &written.callbacks {
            self.callbacks.push(Callback {
                owner: resolve(&callback.owner),
                method: callback.method.clone(),
                only: callback.only.clone(),
                except: callback.except.clone(),
            });
        }
        for (name, owner, def) in &written.bare {
            self.bare
                .entry(name.clone())
                .or_default()
                .insert((resolve(owner), def.clone()));
        }
        self.named.extend(written.named.iter().cloned());
        for (owner, name) in &written.helpers {
            self.helpers.insert((resolve(owner), name.clone()));
        }
    }

    fn parent(&self, class: &str) -> Option<&str> {
        self.bodies.get(class)?.superclass.as_deref()
    }

    fn ancestors(&self, class: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut at = class;
        while let Some(parent) = self.parent(at) {
            if parent == class || out.iter().any(|seen| seen == parent) {
                break;
            }
            out.push(parent.to_owned());
            at = parent;
        }
        out
    }

    fn descendants(&self, class: &str) -> Vec<String> {
        self.bodies
            .keys()
            .filter(|other| {
                other.as_str() != class && self.ancestors(other).iter().any(|a| a == class)
            })
            .cloned()
            .collect()
    }

    fn defines(&self, class: &str, method: &str) -> bool {
        self.defs
            .contains_key(&(class.to_owned(), method.to_owned()))
    }

    /// The classes whose instances run `owner`'s `def method`: itself and every descendant that
    /// does not write its own, nor has one between.
    fn runners(&self, owner: &str, method: &str) -> Vec<String> {
        let mut out = vec![owner.to_owned()];
        for descendant in self.descendants(owner) {
            let mut at = descendant.as_str();
            let mut overridden = false;
            while at != owner {
                if self.defines(at, method) {
                    overridden = true;
                    break;
                }
                // A descendant's chain reaches `owner`.
                at = self.parent(at).unwrap_or(owner);
            }
            if !overridden {
                out.push(descendant);
            }
        }
        out
    }

    /// Whether a class descends from a class this project does not write, other than Rails' own
    /// bases: a gem's controller, which a gem's routing macro may route to.
    fn gem_descendant(&self, class: &str) -> bool {
        let mut at = class.to_owned();
        let mut seen = BTreeSet::new();
        while seen.insert(at.clone()) {
            let Some(parent) = self.parent(&at) else {
                return false;
            };
            if BASES.contains(&parent) {
                return false;
            }
            if !self.bodies.contains_key(parent) {
                return true;
            }
            at = parent.to_owned();
        }
        false
    }

    /// The `(class, action)` pairs a `def` runs under, or `None` where any doubt refuses.
    fn actions(
        &self,
        owner: &str,
        method: &str,
        routed: &dyn Fn(&str) -> Vec<String>,
        depth: u32,
    ) -> Option<BTreeSet<(String, String)>> {
        if self.bodies.get(owner).is_none_or(|body| body.module) || depth > 6 {
            return None;
        }
        if self.named.contains(method) {
            return None;
        }
        let ancestors = self.ancestors(owner);
        let descendants = self.descendants(owner);
        let related: BTreeSet<&str> = std::iter::once(owner)
            .chain(ancestors.iter().map(String::as_str))
            .chain(descendants.iter().map(String::as_str))
            .collect();
        if self
            .helpers
            .iter()
            .any(|(body, name)| name == method && related.contains(body.as_str()))
        {
            return None;
        }
        let public = self
            .defs
            .get(&(owner.to_owned(), method.to_owned()))
            .copied()?;
        let mut pairs = BTreeSet::new();
        if public {
            for class in self.runners(owner, method) {
                pairs.insert((class, method.to_owned()));
            }
        }
        for callback in self
            .callbacks
            .iter()
            .filter(|callback| callback.method.as_deref() == Some(method))
        {
            // A module's callback runs in whatever includes it, which is not read here.
            if self
                .bodies
                .get(&callback.owner)
                .is_none_or(|body| body.module)
            {
                return None;
            }
            if !related.contains(callback.owner.as_str()) {
                continue;
            }
            if callback.only == Filter::Unknown || callback.except == Filter::Unknown {
                return None;
            }
            let classes = if callback.owner == owner || ancestors.contains(&callback.owner) {
                self.runners(owner, method)
            } else {
                std::iter::once(callback.owner.clone())
                    .chain(self.descendants(&callback.owner))
                    .collect()
            };
            for class in classes {
                let actions = match &callback.only {
                    Filter::Listed(only) => only.clone(),
                    _ => routed(&class),
                };
                for action in actions {
                    if let Filter::Listed(except) = &callback.except
                        && except.contains(&action)
                    {
                        continue;
                    }
                    pairs.insert((class.clone(), action));
                }
            }
        }
        let mine: BTreeSet<&str> = std::iter::once(owner)
            .chain(descendants.iter().map(String::as_str))
            .collect();
        for (caller, def) in self.bare.get(method).into_iter().flatten() {
            if caller == owner && def == method {
                continue;
            }
            let caller_body = self.bodies.get(caller);
            if !related.contains(caller.as_str()) && caller_body.is_none_or(|body| !body.module) {
                continue;
            }
            let theirs = self.actions(caller, def, routed, depth + 1)?;
            pairs.extend(
                theirs
                    .into_iter()
                    .filter(|(class, _)| mine.contains(class.as_str())),
            );
        }
        (public || !pairs.is_empty()).then_some(pairs)
    }

    /// What a read of `key` in `owner`'s `def method` holds where every route reaching it gives
    /// the key: `String` for a required segment, a default's class otherwise, joined. `None` where
    /// any route lacks it, none reaches it, or any doubt above refuses.
    #[must_use]
    pub fn proven(
        &self,
        routes: &RouteTable,
        owner: &str,
        method: &str,
        key: &str,
        defined: &dyn Fn(&str) -> bool,
    ) -> Option<Vec<&'static str>> {
        if self
            .singletons
            .contains(&(owner.to_owned(), method.to_owned()))
        {
            return None;
        }
        let by_class = |class: &str| -> Vec<String> {
            routes
                .targets
                .iter()
                .filter(|target| class_of(&target.controller).as_deref() == Some(class))
                .map(|target| target.action.clone())
                .collect()
        };
        let pairs = self.actions(owner, method, &by_class, 0)?;
        for (class, action) in &pairs {
            let reached = routes
                .unreadable
                .iter()
                .any(|unreadable| match &unreadable.reach {
                    Reach::Any => false,
                    Reach::Controller(controller) => {
                        class_of(controller).as_deref() == Some(class.as_str())
                            && unreadable.action.as_ref().is_none_or(|only| only == action)
                    }
                    Reach::Namespace(namespace) => class_of(namespace).is_some_and(|prefix| {
                        let prefix = prefix.trim_end_matches("Controller");
                        class.starts_with(&format!("{prefix}::"))
                    }),
                });
            if reached {
                return None;
            }
        }
        // A route that may reach any controller: a project's own call refuses every proof; a
        // gem's routing macro reaches only the controllers that descend from that gem's.
        for unreadable in &routes.unreadable {
            if unreadable.reach != Reach::Any {
                continue;
            }
            let gem = unreadable
                .call
                .as_deref()
                .is_some_and(|call| !defined(call));
            if !gem || pairs.iter().any(|(class, _)| self.gem_descendant(class)) {
                return None;
            }
        }
        let mut classes: Vec<&'static str> = Vec::new();
        let mut any = false;
        for (class, action) in &pairs {
            for target in routes.targets.iter().filter(|target| {
                &target.action == action
                    && class_of(&target.controller).as_deref() == Some(class.as_str())
            }) {
                any = true;
                // A default stands where the path leaves the key out; an optional segment that
                // writes it, `(.:format)` included, makes it the `String` the path held.
                let held: &[&'static str] = if target.required.iter().any(|segment| segment == key)
                {
                    &["String"]
                } else if written_in(&target.spec, key) {
                    &[*target.defaults.get(key)?, "String"]
                } else {
                    &[*target.defaults.get(key)?]
                };
                for class in held {
                    if !classes.contains(class) {
                        classes.push(class);
                    }
                }
            }
        }
        any.then_some(classes)
    }
}

/// Whether a path writes a segment for `key` anywhere, optional or not.
fn written_in(spec: &str, key: &str) -> bool {
    [':', '*'].iter().any(|sigil| {
        let segment = format!("{sigil}{key}");
        spec.match_indices(&segment).any(|(at, _)| {
            spec.as_bytes()
                .get(at + segment.len())
                .is_none_or(|next| !(next.is_ascii_alphanumeric() || *next == b'_'))
        })
    })
}

/// The class a controller path names: `admin/posts` is `Admin::PostsController`.
fn class_of(controller: &str) -> Option<String> {
    let segments = controller
        .split('/')
        .map(camelize)
        .collect::<Option<Vec<String>>>()?;
    Some(format!("{}Controller", segments.join("::")))
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::super::targets::{Macro, RouteContext, RouteSource, Unreadable, read_targets};
    use super::*;

    fn routes(source: &str) -> RouteTable {
        let none = |_: &str| None;
        let isolated = BTreeMap::new();
        let macros: BTreeMap<String, Macro> = BTreeMap::new();
        read_targets(
            &[RouteSource {
                text: source,
                whole: false,
            }],
            &RouteContext {
                drawn: &none,
                isolated: &isolated,
                macros: &macros,
            },
        )
    }

    fn controllers(files: &[&str]) -> Controllers {
        let mut controllers = Controllers::default();
        let resolve = |nesting: &[String]| nesting.join("::");
        let superclass = |_: &[String], written: &str| written.trim_start_matches("::").to_owned();
        for file in files {
            controllers.absorb(&read_controllers(file), &resolve, &superclass);
        }
        controllers
    }

    const ROUTES: &str = "\
Rails.application.routes.draw do
  resources :posts, only: %i[index show edit]
  get \"posts/:id/preview\", to: \"posts#preview\"
  resources :drafts, only: :show
  get \"feed\", to: \"posts#feed\", id: :latest
  get \"formatted\", to: \"posts#formatted\", defaults: { format: :json }
  get \"maybe\", to: \"posts#maybe\", id: nil
  namespace :admin do
    resources :posts, only: :show
  end
end
";

    const POSTS: &str = "\
class ApplicationController < ActionController::Base
end

class PostsController < ApplicationController
  before_action :set_post, only: %i[show edit preview]
  before_action :set_any
  before_action :set_some, except: :index
  before_action :set_most, except: %i[index formatted]
  before_action :unknown_only, only: ACTIONS
  before_action :unknown_except, except: list
  before_action :listed, only: :show
  before_action do
    params[:id]
  end
  helper_method :shown

  def index; end

  def show
    find_post
  end

  def edit
    find_post
    run_twice
  end

  def preview
    find_post
  end

  def feed
    self.find_post
  end

  def maybe; end

  def formatted; end

  def shown; end

  def self.build; end

  class << self
    def also; end
  end

  def run_twice
    run_twice
  end

  def sent
    send(:by_name)
    try(:tried, 1)
    respond_to?(:asked)
  end

  private

  def set_post; end
  def set_any; end
  def set_some; end
  def set_most; end
  def unknown_only; end
  def unknown_except; end
  def listed; end
  def find_post; end
  def by_name; end
  def tried; end
  def lonely; end
  def from_class_method; end

  public

  def back_in_public; end

  protected def guarded; end
  private def walled; end
  private :back_in_public
  public :lonely
  private \"x\", other

  def self.calls_bare
    from_class_method
  end
end

class DraftsController < PostsController
  def show
    find_post
  end
end

module Admin
  class PostsController < ::PostsController
  end
end

module Concern
  extend ActiveSupport::Concern

  included do
    before_action :from_concern
    rescue_from Error, with: :handled
    before_action :set_post, if: :ready?
  end

  def from_concern; end

  def helper
    find_post
  end
end

class GemChild < Devise::SessionsController
  def show; end
end

class Unrelated < ApplicationController
  include Concern
  prepend Other::Module
  include helper_module
end
";

    #[test]
    fn a_controller_file_is_read_for_its_bodies_defs_callbacks_and_calls() {
        let written = read_controllers(POSTS);
        assert_eq!(
            written.classes[1],
            WrittenClass {
                nesting: vec!["PostsController".to_owned()],
                superclass: Some("ApplicationController".to_owned()),
            }
        );
        assert_eq!(
            written.modules,
            [vec!["Admin".to_owned()], vec!["Concern".to_owned()]]
        );
        assert_eq!(
            written.includes,
            [
                (vec!["Unrelated".to_owned()], "Concern".to_owned()),
                (vec!["Unrelated".to_owned()], "Other::Module".to_owned()),
            ]
        );
        let visibility = |name: &str| {
            written
                .defs
                .iter()
                .find(|def| def.name == name)
                .map(|def| (def.public, def.singleton))
        };
        assert_eq!(visibility("show"), Some((true, false)));
        assert_eq!(visibility("set_post"), Some((false, false)));
        assert_eq!(visibility("back_in_public"), Some((false, false)));
        assert_eq!(visibility("lonely"), Some((true, false)));
        assert_eq!(visibility("guarded"), Some((false, false)));
        assert_eq!(visibility("walled"), Some((false, false)));
        assert_eq!(visibility("build"), Some((true, true)));
        assert_eq!(visibility("also"), Some((true, true)));
        assert_eq!(
            written
                .callbacks
                .iter()
                .map(|callback| (
                    callback.method.clone().unwrap_or_default(),
                    callback.only.clone(),
                    callback.except.clone()
                ))
                .collect::<Vec<_>>()[..5],
            [
                (
                    "set_post".to_owned(),
                    Filter::Listed(vec![
                        "show".to_owned(),
                        "edit".to_owned(),
                        "preview".to_owned()
                    ]),
                    Filter::None
                ),
                ("set_any".to_owned(), Filter::None, Filter::None),
                (
                    "set_some".to_owned(),
                    Filter::None,
                    Filter::Listed(vec!["index".to_owned()])
                ),
                (
                    "set_most".to_owned(),
                    Filter::None,
                    Filter::Listed(vec!["index".to_owned(), "formatted".to_owned()])
                ),
                ("unknown_only".to_owned(), Filter::Unknown, Filter::None),
            ]
        );
        assert!(
            written
                .callbacks
                .iter()
                .any(|callback| callback.method.is_none())
        );
        assert_eq!(
            written.named,
            BTreeSet::from([
                "asked".to_owned(),
                "by_name".to_owned(),
                "handled".to_owned(),
                "ready?".to_owned(),
                "tried".to_owned(),
            ])
        );
        assert_eq!(
            written.helpers,
            [(vec!["PostsController".to_owned()], "shown".to_owned())]
        );
        assert!(written.bare.contains(&(
            "find_post".to_owned(),
            vec!["PostsController".to_owned()],
            "feed".to_owned()
        )));
    }

    #[test]
    fn a_read_is_proven_where_every_route_reaching_its_actions_gives_the_key() {
        let table = routes(ROUTES);
        let index = controllers(&[POSTS]);
        let defined = |_: &str| false;
        let proven = |owner: &str, method: &str, key: &str| {
            index.proven(&table, owner, method, key, &defined)
        };
        // An action whose every route requires the key, its own and its subclasses' that run it
        // (`Admin::PostsController`; `DraftsController` writes its own).
        assert_eq!(
            proven("PostsController", "show", "id"),
            Some(vec!["String"])
        );
        assert_eq!(
            proven("DraftsController", "show", "id"),
            Some(vec!["String"])
        );
        assert_eq!(
            proven("PostsController", "edit", "id"),
            Some(vec!["String"])
        );
        // A callback limited to routed actions requiring the key.
        assert_eq!(
            proven("PostsController", "listed", "id"),
            Some(vec!["String"])
        );
        // `set_post` is one too, but a concern's `included do` names it as well, which runs in
        // whatever includes it.
        assert_eq!(proven("PostsController", "set_post", "id"), None);
        // A helper, through its callers: `feed` gives `id` a default, `index` reaches none.
        assert_eq!(
            proven("PostsController", "find_post", "id"),
            None,
            "Drafts' own show and the concern's helper call it too"
        );
        assert_eq!(
            proven("PostsController", "feed", "id"),
            Some(vec!["Symbol"])
        );
        // Every path writes `(.:format)`: a default `format` is the default or the `String` the
        // path held.
        assert_eq!(proven("PostsController", "feed", "format"), None);
        assert_eq!(
            proven("PostsController", "formatted", "format"),
            Some(vec!["Symbol", "String"])
        );
        assert!(written_in("/a/:id_x/*rest(.:format)", "rest"));
        assert!(!written_in("/a/:id_x", "id"));
        assert_eq!(proven("PostsController", "maybe", "id"), Some(vec!["nil"]));
        // A key some route lacks, an action no route reaches, a callback on every action.
        assert_eq!(proven("PostsController", "show", "slug"), None);
        assert_eq!(proven("PostsController", "lonely", "id"), None);
        assert_eq!(proven("PostsController", "set_any", "id"), None);
        // All but `index`, which still reaches `formatted`, whose route lacks `id`.
        assert_eq!(proven("PostsController", "set_some", "id"), None);
        // The rest each give `id`: as a segment, a symbol default and a `nil` one.
        assert_eq!(
            proven("PostsController", "set_most", "id"),
            Some(vec!["String", "Symbol", "nil"])
        );
        // A public method no route reaches runs under its callers' actions, its own call of itself
        // aside.
        assert_eq!(
            proven("PostsController", "run_twice", "id"),
            Some(vec!["String"])
        );
        // Every doubt refuses.
        for (owner, method) in [
            ("PostsController", "unknown_only"),
            ("PostsController", "unknown_except"),
            ("PostsController", "shown"),
            ("PostsController", "by_name"),
            ("PostsController", "tried"),
            ("PostsController", "build"),
            ("PostsController", "missing"),
            ("Concern", "helper"),
            ("NotRead", "show"),
            ("PostsController", "from_class_method"),
            ("PostsController", "set_post_elsewhere"),
        ] {
            assert_eq!(proven(owner, method, "id"), None, "{owner}#{method}");
        }
        // A callback named in a module runs wherever the module is included.
        assert_eq!(proven("Unrelated", "from_concern", "id"), None);
    }

    #[test]
    fn a_route_the_reader_could_not_read_refuses_what_it_may_reach() {
        let index = controllers(&[POSTS]);
        let table = routes(ROUTES);
        let base = index.proven(&table, "DraftsController", "show", "id", &|_| false);
        assert_eq!(base, Some(vec!["String"]));
        let with = |unreadable: Unreadable, defined: bool| {
            let mut table = table.clone();
            table.unreadable.push(unreadable);
            index.proven(&table, "DraftsController", "show", "id", &|_| defined)
        };
        let any = |call: Option<&str>| Unreadable {
            reach: Reach::Any,
            action: None,
            call: call.map(str::to_owned),
            why: "x",
        };
        assert_eq!(with(any(None), false), None);
        // A gem's routing macro reaches only what descends from a gem's controller.
        assert_eq!(with(any(Some("devise_for")), false), base);
        assert_eq!(with(any(Some("my_routes")), true), None);
        assert_eq!(
            index.proven(&table, "GemChild", "show", "id", &|_| false),
            None,
            "no route reaches it"
        );
        let controller = |action: Option<&str>| Unreadable {
            reach: Reach::Controller("drafts".to_owned()),
            action: action.map(str::to_owned),
            call: None,
            why: "x",
        };
        assert_eq!(with(controller(None), false), None);
        assert_eq!(with(controller(Some("show")), false), None);
        assert_eq!(with(controller(Some("index")), false), base);
        let namespace = |name: &str| Unreadable {
            reach: Reach::Namespace(name.to_owned()),
            action: None,
            call: None,
            why: "x",
        };
        // `PostsController#show` runs in `Admin::PostsController` too.
        let shown =
            |table: &RouteTable| index.proven(table, "PostsController", "show", "id", &|_| false);
        assert_eq!(shown(&table), Some(vec!["String"]));
        let mut walled = table.clone();
        walled.unreadable.push(namespace("admin"));
        assert_eq!(shown(&walled), None);
        assert_eq!(with(namespace("blazer"), false), base);
        // A gem's macro with a controller that descends from a gem's own in the project.
        let gem = controllers(&[
            POSTS,
            "class SessionsController < Devise::SessionsController\n  def create\n    params[:id]\n  end\nend\n",
        ]);
        let mut routed = routes(
            "Rails.application.routes.draw do\n  post \"sessions/:id\", to: \"sessions#create\"\nend\n",
        );
        assert_eq!(
            gem.proven(&routed, "SessionsController", "create", "id", &|_| false),
            Some(vec!["String"])
        );
        routed.unreadable.push(any(Some("devise_for")));
        assert_eq!(
            gem.proven(&routed, "SessionsController", "create", "id", &|_| false),
            None
        );
        assert_eq!(
            class_of("admin/posts").as_deref(),
            Some("Admin::PostsController")
        );
        assert_eq!(class_of("posts/1x"), None);
    }

    #[test]
    fn a_hierarchy_with_a_loop_ends() {
        let index = controllers(&[
            "class A < B\n  def show; end\nend\nclass B < A\nend\nclass C < C\nend\n",
        ]);
        assert!(index.ancestors("A").len() <= 2);
        assert!(index.ancestors("C").is_empty());
        assert!(!index.gem_descendant("A"));
        assert!(!index.gem_descendant("C"));
        let table =
            routes("Rails.application.routes.draw do\n  get \"a/:id\", to: \"a#show\"\nend\n");
        let _ = index.proven(&table, "A", "show", "id", &|_| false);
    }
    #[test]
    fn the_rest_of_a_controller_s_dispatch() {
        let source = "\
class BaseController < ActionController::Base
  before_action :inherited_hook, only: :show
  private :missing
  before_action :hooked, **options
  rescue_from Error, with: \"by_text\", **more
end

class ChildController < BaseController
  def show; end

  private

  def inherited_hook; end
  def hooked; end
  def d1 = d2
  def d2 = d3
  def d3 = d4
  def d4 = d5
  def d5 = d6
  def d6 = d7
  def d7 = d8
  def d8; end
end

class ElsewhereController < ActionController::Base
  before_action :inherited_hook

  def show
    d8
    inherited_hook
  end
end

class Plain
  def kept; end
  private :other_name
  protected
  def guarded; end
end

class GrandchildController < ChildController
  before_action :inherited_hook, only: :show
end

class Loop1 < Loop2
end

class Loop2 < Loop3
end

class Loop3 < Loop2
end
";
        let index = controllers(&[source]);
        let table = routes(
            "Rails.application.routes.draw do\n  get \"child/:id\", to: \"child#show\"\n  \
             get \"elsewhere/:other\", to: \"elsewhere#show\"\n  get \"d1/:id\", to: \"child#d1\"\nend\n",
        );
        let proven =
            |owner: &str, method: &str| index.proven(&table, owner, method, "id", &|_| false);
        // A callback declared on the class above: it runs where the method's class runs it.
        assert_eq!(
            proven("ChildController", "inherited_hook"),
            Some(vec!["String"])
        );
        // A callback with no `only:` runs under every routed action, here each with `id`; the
        // `**options` beside it could only narrow that.
        assert_eq!(proven("ChildController", "hooked"), Some(vec!["String"]));
        // An unrelated class calling the name bare is another method.
        assert_eq!(proven("ChildController", "d8"), None, "seven hops deep");
        assert_eq!(index.ancestors("Plain"), Vec::<String>::new());
        assert_eq!(index.ancestors("Loop1"), ["Loop2", "Loop3"]);
        assert!(written_in("/a/:id", "id") && !written_in("/a/:idx", "id"));
        assert!(!index.gem_descendant("Plain"));
        let mut walled = table.clone();
        walled.unreadable.push(Unreadable {
            reach: Reach::Controller("other".to_owned()),
            action: None,
            call: None,
            why: "x",
        });
        assert_eq!(
            index.proven(&walled, "ChildController", "show", "id", &|_| false),
            Some(vec!["String"])
        );
        assert!(!written_in("/a/:id_", "id"));
        assert!(written_in("/a/*id", "id"));
    }
}
