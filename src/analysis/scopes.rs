//! Which variable is which, for the variables rubydex does not model.
//!
//! The graph knows about constants and methods. It knows nothing about local variables —
//! `LocalVariable` appears nowhere in rubydex's source — and while it records an instance
//! variable's *assignment* as a declaration it records no references to one, so a bare `@name` is
//! invisible to it. Both are ordinary things to put a cursor on, so answering for them means
//! owning the scope rules: this is the third direct use of Prism, after `cursor` and `requires`.
//!
//! # Why the scopes are Prism's rather than ours
//!
//! Deciding which `x` is which by hand means reimplementing Ruby's scoping: a block sees the
//! locals around it and a `def` does not, a block parameter shadows the local it is spelled like,
//! `for` declares into the enclosing scope while `->() {}` does not. Prism has already done it.
//! Every local-variable node carries a **`depth`** — how many scopes out the name was resolved to
//! — so two occurrences are the same variable exactly when they have the same name and land on
//! the same entry of the scope stack. Nothing here re-derives that, which is why
//! `x = 1; [1].each { |x| x }` separates correctly with no rule about shadowing written down.
//!
//! It also means the awkward spellings arrive already reduced: `rescue => e` and `in [a, b]` are
//! both `LocalVariableTargetNode`, `def f(a, (b, c))` destructures into plain parameters, and
//! `_1` is a read of a local the block declares. None needs a case here.
//!
//! # Instance variables are scoped by what `self` is
//!
//! `@v` in `def a` and `@v` in `def self.b` are different variables — one belongs to an instance
//! of the class and the other to the class object — and a highlight that joins them is wrong in a
//! way the user can see. There is no depth to read for these, so [`SelfContext`] tracks it: a
//! namespace body *is* the class object, `class << self` is the object one singleton step above
//! it, and a `def` with no receiver is an instance of whatever `self` is where it is written.
//! That last rule is what makes `def c` inside `class << self` land back on the class, and so
//! share its `@v` with `def self.b`.

use ruby_prism::{
    BlockLocalVariableNode, BlockNode, BlockParameterNode, ClassNode, ConstantId, DefNode,
    InstanceVariableAndWriteNode, InstanceVariableOperatorWriteNode, InstanceVariableOrWriteNode,
    InstanceVariableReadNode, InstanceVariableTargetNode, InstanceVariableWriteNode,
    ItLocalVariableReadNode, KeywordRestParameterNode, LambdaNode, LocalVariableAndWriteNode,
    LocalVariableOperatorWriteNode, LocalVariableOrWriteNode, LocalVariableReadNode,
    LocalVariableTargetNode, LocalVariableWriteNode, Location, ModuleNode, Node,
    OptionalKeywordParameterNode, OptionalParameterNode, RequiredKeywordParameterNode,
    RequiredParameterNode, RestParameterNode, SingletonClassNode, Visit,
};

/// One place a variable is written or read.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Occurrence {
    pub start: u32,
    pub end: u32,
    /// `true` where the variable is assigned, or declared as a parameter.
    ///
    /// An operator assignment (`x += 1`, `@v ||= []`) is a write: it reads too, but the thing
    /// the reader of a highlighted file is looking for is where the value comes from.
    pub write: bool,
}

/// Every occurrence of the variable under `offset`, or `None` when the cursor is not on one.
///
/// `None` is the answer for everything else in a Ruby file — a method call, a constant, a
/// comment, the inside of a string — and it is what lets the caller fall through to the graph.
#[must_use]
pub fn occurrences(source: &str, offset: u32) -> Option<Vec<Occurrence>> {
    variable(source, offset).map(|(_, occurrences)| occurrences)
}

/// The variable under `offset`: its name as Ruby spells it, and every place it appears.
///
/// The name comes back from here rather than being cut out of the source at the first span,
/// because the identity a set of occurrences shares already *is* the name — so a caller that
/// needs it has no reason to re-derive it, and no absent case to handle if it does not. An
/// instance variable's name carries its `@`, which is how [`rename`](super::rename) knows one
/// without looking at the syntax again.
#[must_use]
pub fn variable(source: &str, offset: u32) -> Option<(String, Vec<Occurrence>)> {
    let result = ruby_prism::parse(source.as_bytes());
    let mut walk = Walk::new(source);
    walk.visit(&result.node());
    walk.under(offset)
}

/// Every write to `@name` on an *instance* of the class written as `path`.
///
/// [`variable`] answers "which occurrences share the one under this cursor", and there is one
/// caller with no cursor in the file it has to ask about: a template's instance variables are
/// assigned in a controller, and the question is asked from the view. Same algebra, entered by
/// name instead of by offset — `path` is the namespace as the file spells it, and an instance
/// is `level` 0, so a `def self.` and a `class << self` are excluded here exactly as they are
/// for a cursor. A second copy of that algebra in the caller is the one thing certain to drift.
///
/// Writes only: what types a variable is what was assigned to it, and a read has no value to
/// look at. Ordered by offset, so "the textually last one" is the caller's to take.
#[must_use]
pub fn writes_to(source: &str, path: &str, name: &str) -> Vec<Occurrence> {
    let result = ruby_prism::parse(source.as_bytes());
    let mut walk = Walk::new(source);
    walk.visit(&result.node());
    walk.written(path, name)
}

/// Which locals a byte range borrows from around it, and whether anything it writes escapes.
///
/// The question [`code_actions`](super::code_actions) has to ask before it may lift a run of
/// statements into a method of its own, and it is asked here because the answer is the scope
/// stack: two `x`s that Prism resolved to different scopes are two variables, and an extraction
/// that passed one and left the other would compile and be wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crossing {
    /// Locals the range reads before it writes them — the parameters an extracted method needs,
    /// in the order the range first reaches for them.
    pub reads: Vec<String>,
    /// `true` when the range writes a local that is touched again after it.
    ///
    /// There is no single value an extracted method could hand back for that, so the caller
    /// declines rather than approximating. Any occurrence after the range counts, read or
    /// write: [`Occurrence`] records `x = 1` and `x += 1` both as writes, and telling the one
    /// that needs the old value from the one that does not is a distinction this does not have
    /// and would be wrong about.
    pub escapes: bool,
}

/// Which locals `source[start..end]` reads from outside itself, and what it writes that outlives
/// it.
#[must_use]
pub fn crossing(source: &str, start: u32, end: u32) -> Crossing {
    let result = ruby_prism::parse(source.as_bytes());
    let mut walk = Walk::new(source);
    walk.visit(&result.node());
    walk.crossing(start, end)
}

/// The instance variable at `offset` and where an accessor for it would have to be declared, or
/// `None` when there is no such place.
///
/// `Some` only where the variable belongs to an **instance** of a namespace written in this
/// file, and that condition is the whole point of answering from here. `attr_reader :count`
/// declares an instance method reading an *instance's* `@count`, so offering it for the
/// `@count` inside `def self.count` or a `class << self` writes an accessor that reads a
/// different variable — code that runs, returns `nil`, and looks right. [`SelfContext`]'s level
/// already knows the difference; a second copy of that algebra in the caller is the one thing
/// certain to drift.
///
/// The offset is where the namespace's body begins, which is where the declaration goes.
#[must_use]
pub fn accessor_site(source: &str, offset: u32) -> Option<(String, u32)> {
    let result = ruby_prism::parse(source.as_bytes());
    let mut walk = Walk::new(source);
    walk.visit(&result.node());
    walk.accessor(offset)
}

/// What `self` is at a point in the file, which is what an instance variable belongs to.
///
/// `level` counts singleton steps above an *instance* of `path`: 0 is an instance, 1 is the
/// class object itself, 2 is its singleton class. Entering `class << self` adds a step and a
/// receiverless `def` takes one away, so `def self.b` and a `def c` inside `class << self`
/// arrive at the same context — which is what Ruby does.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SelfContext {
    /// The lexical namespace path as written, so that a class reopened in the same file under
    /// the same spelling keeps its instance variables together.
    path: String,
    level: i32,
}

/// The identity two occurrences must share to be the same variable.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Variable {
    /// A local, parameter or block-local, keyed by the scope Prism resolved it to.
    Local { name: String, scope: u32 },
    /// An instance variable, keyed by the object it hangs off.
    Instance { name: String, owner: SelfContext },
}

struct Walk<'s> {
    source: &'s str,
    /// The scope stack, innermost last. A local at `depth` was declared in the entry `depth`
    /// places from the end — that indexing is the whole of the scoping logic.
    scopes: Vec<u32>,
    next_scope: u32,
    this: SelfContext,
    found: Vec<(Variable, Occurrence)>,
    /// Where the body of the namespace `self` belongs to begins, or `None` when there is no
    /// such namespace written in this file — the top level, and either island.
    body: Option<u32>,
    /// Every instance-variable occurrence that belongs to an instance of a namespace, with the
    /// name and the offset an accessor for it would be declared at.
    ///
    /// Kept beside [`Self::found`] rather than inside it because it is a different question:
    /// `found` is "which occurrences are the same variable", which is what identity is for, and
    /// putting a body offset into [`Variable`] would make a class reopened twice in one file
    /// into two variables.
    sites: Vec<(Occurrence, String, u32)>,
}

impl<'s> Walk<'s> {
    fn new(source: &'s str) -> Self {
        Self {
            source,
            // The file's own scope, pushed before the walk rather than by a visit, so that the
            // stack is never empty and neither of the lookups below has an absent case.
            scopes: vec![0],
            next_scope: 1,
            // The top level: `self` is `main`, an ordinary object, under the empty path.
            this: SelfContext {
                path: String::new(),
                level: 1,
            },
            found: Vec::new(),
            body: None,
            sites: Vec::new(),
        }
    }

    /// The name of the variable the cursor is on, and the occurrences sharing it.
    fn under(mut self, offset: u32) -> Option<(String, Vec<Occurrence>)> {
        // Sorted before the search so that "the first one covering the cursor" is a fact about
        // the file rather than about the order a visitor happened to record writes in.
        self.found.sort_by_key(|(_, occurrence)| occurrence.start);
        let (variable, _) = self
            .found
            .iter()
            // Inclusive of the end, as `locator::covers` is: a cursor parked just past the last
            // character of a name is still on it.
            .find(|(_, at)| at.start <= offset && offset <= at.end)?;

        let name = match variable {
            Variable::Local { name, .. } | Variable::Instance { name, .. } => name.clone(),
        };
        Some((
            name,
            self.found
                .iter()
                .filter(|(candidate, _)| candidate == variable)
                .map(|(_, at)| at.clone())
                .collect(),
        ))
    }

    /// Every write to `@name` on an instance of `path`, in the order the file writes them.
    fn written(mut self, path: &str, name: &str) -> Vec<Occurrence> {
        self.found.sort_by_key(|(_, occurrence)| occurrence.start);
        self.found
            .into_iter()
            .filter(|(variable, occurrence)| {
                occurrence.write
                    && matches!(
                        variable,
                        Variable::Instance { name: spelled, owner }
                            if spelled == name && owner.path == path && owner.level == 0
                    )
            })
            .map(|(_, occurrence)| occurrence)
            .collect()
    }

    /// The parameters a range would need, and whether anything it writes outlives it.
    fn crossing(mut self, start: u32, end: u32) -> Crossing {
        // Sorted so that "the first occurrence inside the range" is a fact about the file, and
        // so that the parameters come out in the order the range reaches for them.
        self.found.sort_by_key(|(_, occurrence)| occurrence.start);
        let inside = |at: &Occurrence| start <= at.start && at.end <= end;

        let mut crossing = Crossing {
            reads: Vec::new(),
            escapes: false,
        };
        let mut seen: Vec<&Variable> = Vec::new();
        // Walked over what is *inside* the range, in order, so that the first occurrence of a
        // variable reached here is the first one the range makes — which is both what decides
        // the answer and the order the parameters come out in.
        for (variable, at) in &self.found {
            let Variable::Local { name, .. } = variable else {
                continue;
            };
            if !inside(at) || seen.contains(&variable) {
                continue;
            }
            seen.push(variable);
            // A read means the value came from outside and has to be passed in; a write means
            // the range declares the variable itself, so an extracted method would declare it.
            if !at.write {
                crossing.reads.push(name.clone());
            }
            let mine = || {
                self.found
                    .iter()
                    .filter(|(it, _)| it == variable)
                    .map(|(_, at)| at)
            };
            crossing.escapes |=
                mine().any(|at| inside(at) && at.write) && mine().any(|at| at.start >= end);
        }
        crossing
    }

    /// The instance variable at `offset`, and where its accessor would be declared.
    fn accessor(self, offset: u32) -> Option<(String, u32)> {
        self.sites
            .into_iter()
            // Inclusive of the end, as `under` is: a cursor parked just past the last character
            // of a name is still on it.
            .find(|(at, _, _)| at.start <= offset && offset <= at.end)
            .map(|(_, name, body)| (name, body))
    }

    /// Run `body` inside a freshly numbered local scope.
    fn scoped(&mut self, body: impl FnOnce(&mut Self)) {
        self.scopes.push(self.next_scope);
        self.next_scope += 1;
        body(self);
        self.scopes.pop();
    }

    /// Run `run` with `self` bound to `this`, and with `site` as the place an accessor for an
    /// instance of it would be declared.
    ///
    /// The two travel together because they are decided together: every construct that changes
    /// what `self` is either opens a namespace body, keeps the one around it, or is an island
    /// with no body at all, and separating them is how one of the three would come to be missed.
    fn as_self(&mut self, this: SelfContext, site: Option<u32>, run: impl FnOnce(&mut Self)) {
        let outer = std::mem::replace(&mut self.this, this);
        let outside = std::mem::replace(&mut self.body, site);
        run(self);
        self.this = outer;
        self.body = outside;
    }

    /// The scope a local resolved `depth` steps out from the innermost one.
    ///
    /// Total, and deliberately: the file's scope is on the stack before the walk starts, and
    /// Prism never resolves a name to a scope it did not parse. A depth past the outermost one
    /// lands on the file rather than being a case with anything to do about it.
    fn scope_at(&self, depth: u32) -> u32 {
        self.scopes[self.scopes.len().saturating_sub(1 + depth as usize)]
    }

    fn local(&mut self, name: &ConstantId<'_>, depth: u32, at: &Location<'_>, write: bool) {
        let scope = self.scope_at(depth);
        self.record(
            Variable::Local {
                name: spelled(name),
                scope,
            },
            at,
            write,
        );
    }

    /// A parameter, a block-local or `it`: declared in the innermost scope, with no depth of
    /// its own to read because there is nowhere else it could have come from.
    fn here(&mut self, name: String, at: &Location<'_>, write: bool) {
        let scope = self.scope_at(0);
        self.record(Variable::Local { name, scope }, at, write);
    }

    fn instance(&mut self, name: &ConstantId<'_>, at: &Location<'_>, write: bool) {
        let spelling = spelled(name);
        // Level 0 is an instance, and it is the only level an `attr_` accessor can read. A
        // namespace body is the class object and `class << self` is a step above that, so both
        // are excluded here rather than by a rule of their own — and so is the top level, which
        // has no body to declare into.
        if let (0, Some(body)) = (self.this.level, self.body) {
            let start = at.start_offset() as u32;
            self.sites.push((
                Occurrence {
                    start,
                    end: at.end_offset() as u32,
                    write,
                },
                spelling.clone(),
                body,
            ));
        }
        self.record(
            Variable::Instance {
                name: spelling,
                owner: self.this.clone(),
            },
            at,
            write,
        );
    }

    fn record(&mut self, variable: Variable, at: &Location<'_>, write: bool) {
        // A keyword parameter's name span carries its colon (`d:`), and the colon is not part
        // of the name anywhere else it is written. Trimmed here rather than at each call site
        // so that the four keyword spellings cannot disagree.
        let start = at.start_offset() as u32;
        let end = at.end_offset() as u32;
        let end = if self.source[start as usize..end as usize].ends_with(':') {
            end - 1
        } else {
            end
        };
        self.found
            .push((variable, Occurrence { start, end, write }));
    }

    /// A namespace body: `self` is the class or module object, under the path as written.
    fn namespace(&self, path: &Node<'_>) -> SelfContext {
        let written = &self.source[path.location().start_offset()..path.location().end_offset()];
        SelfContext {
            path: if self.this.path.is_empty() {
                written.to_owned()
            } else {
                format!("{}::{written}", self.this.path)
            },
            level: 1,
        }
    }

    /// A singleton or definee that is some object rather than `self`: `class << obj`,
    /// `def obj.f`. What it is cannot be known without types, so it gets an island of its own
    /// named by where it is written — two of them never share an instance variable, which
    /// highlights too little rather than joining two unrelated ones.
    fn island(&self, at: &Location<'_>) -> SelfContext {
        SelfContext {
            path: format!("{}<{}>", self.this.path, at.start_offset()),
            level: 1,
        }
    }
}

impl<'pr> Visit<'pr> for Walk<'_> {
    // The four scope openers below hand-visit their children rather than deferring to the
    // default walk, because **a superclass, a singleton's receiver and a definee are written
    // outside the scope they open**. `class Foo < v` reads the enclosing scope's `v` and Prism
    // numbers its depth from there, so visiting it after the push would resolve it against the
    // class body and split one variable in two.

    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        if let Some(superclass) = node.superclass() {
            self.visit(&superclass);
        }
        let this = self.namespace(&node.constant_path());
        self.as_self(this, opens(node.body().as_ref()), |walk| {
            walk.scoped(|walk| {
                if let Some(body) = node.body() {
                    walk.visit(&body);
                }
            });
        });
    }

    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        let this = self.namespace(&node.constant_path());
        self.as_self(this, opens(node.body().as_ref()), |walk| {
            walk.scoped(|walk| {
                if let Some(body) = node.body() {
                    walk.visit(&body);
                }
            });
        });
    }

    fn visit_singleton_class_node(&mut self, node: &SingletonClassNode<'pr>) {
        let expression = node.expression();
        self.visit(&expression);
        // `class << self` is a step above the namespace around it and keeps its body; `class <<
        // obj` is an island, and an island has no body an accessor could be written into.
        let (this, site) = if matches!(expression, Node::SelfNode { .. }) {
            (
                SelfContext {
                    path: self.this.path.clone(),
                    level: self.this.level + 1,
                },
                self.body,
            )
        } else {
            (self.island(&node.location()), None)
        };
        self.as_self(this, site, |walk| {
            walk.scoped(|walk| {
                if let Some(body) = node.body() {
                    walk.visit(&body);
                }
            });
        });
    }

    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        let (this, site) = match node.receiver() {
            None => (
                SelfContext {
                    path: self.this.path.clone(),
                    level: self.this.level - 1,
                },
                self.body,
            ),
            Some(receiver) => {
                self.visit(&receiver);
                if matches!(receiver, Node::SelfNode { .. }) {
                    (self.this.clone(), self.body)
                } else {
                    (self.island(&node.location()), None)
                }
            }
        };
        self.as_self(this, site, |walk| {
            walk.scoped(|walk| {
                if let Some(parameters) = node.parameters() {
                    walk.visit_parameters_node(&parameters);
                }
                if let Some(body) = node.body() {
                    walk.visit(&body);
                }
            });
        });
    }

    // A block and a lambda open a scope but not a `self`: what `self` is inside one is what it
    // was outside, which is why `@v` in a block belongs to the enclosing method's object.

    fn visit_block_node(&mut self, node: &BlockNode<'pr>) {
        self.scoped(|walk| ruby_prism::visit_block_node(walk, node));
    }

    fn visit_lambda_node(&mut self, node: &LambdaNode<'pr>) {
        self.scoped(|walk| ruby_prism::visit_lambda_node(walk, node));
    }

    // Locals: every one of these carries the depth that says which scope it belongs to.

    fn visit_local_variable_read_node(&mut self, node: &LocalVariableReadNode<'pr>) {
        self.local(&node.name(), node.depth(), &node.location(), false);
    }

    fn visit_local_variable_write_node(&mut self, node: &LocalVariableWriteNode<'pr>) {
        self.local(&node.name(), node.depth(), &node.name_loc(), true);
        ruby_prism::visit_local_variable_write_node(self, node);
    }

    fn visit_local_variable_target_node(&mut self, node: &LocalVariableTargetNode<'pr>) {
        self.local(&node.name(), node.depth(), &node.location(), true);
    }

    fn visit_local_variable_and_write_node(&mut self, node: &LocalVariableAndWriteNode<'pr>) {
        self.local(&node.name(), node.depth(), &node.name_loc(), true);
        ruby_prism::visit_local_variable_and_write_node(self, node);
    }

    fn visit_local_variable_or_write_node(&mut self, node: &LocalVariableOrWriteNode<'pr>) {
        self.local(&node.name(), node.depth(), &node.name_loc(), true);
        ruby_prism::visit_local_variable_or_write_node(self, node);
    }

    fn visit_local_variable_operator_write_node(
        &mut self,
        node: &LocalVariableOperatorWriteNode<'pr>,
    ) {
        self.local(&node.name(), node.depth(), &node.name_loc(), true);
        ruby_prism::visit_local_variable_operator_write_node(self, node);
    }

    // Parameters and block-locals: declared in the scope being visited, so no depth to read.

    fn visit_required_parameter_node(&mut self, node: &RequiredParameterNode<'pr>) {
        self.here(spelled(&node.name()), &node.location(), true);
    }

    fn visit_optional_parameter_node(&mut self, node: &OptionalParameterNode<'pr>) {
        self.here(spelled(&node.name()), &node.name_loc(), true);
        ruby_prism::visit_optional_parameter_node(self, node);
    }

    fn visit_required_keyword_parameter_node(&mut self, node: &RequiredKeywordParameterNode<'pr>) {
        self.here(spelled(&node.name()), &node.name_loc(), true);
    }

    fn visit_optional_keyword_parameter_node(&mut self, node: &OptionalKeywordParameterNode<'pr>) {
        self.here(spelled(&node.name()), &node.name_loc(), true);
        ruby_prism::visit_optional_keyword_parameter_node(self, node);
    }

    fn visit_rest_parameter_node(&mut self, node: &RestParameterNode<'pr>) {
        // An anonymous `*` or `**` has no name and nothing to highlight; rubydex records those
        // under the sigil itself, and here they are simply not a variable.
        if let (Some(name), Some(at)) = (node.name(), node.name_loc()) {
            self.here(spelled(&name), &at, true);
        }
    }

    fn visit_keyword_rest_parameter_node(&mut self, node: &KeywordRestParameterNode<'pr>) {
        if let (Some(name), Some(at)) = (node.name(), node.name_loc()) {
            self.here(spelled(&name), &at, true);
        }
    }

    fn visit_block_parameter_node(&mut self, node: &BlockParameterNode<'pr>) {
        if let (Some(name), Some(at)) = (node.name(), node.name_loc()) {
            self.here(spelled(&name), &at, true);
        }
    }

    fn visit_block_local_variable_node(&mut self, node: &BlockLocalVariableNode<'pr>) {
        self.here(spelled(&node.name()), &node.location(), true);
    }

    fn visit_it_local_variable_read_node(&mut self, node: &ItLocalVariableReadNode<'pr>) {
        // `it` names no variable Prism resolves, so it is keyed by the block it reads in — the
        // same scope `_1` would be declared in, which is what makes the two behave alike.
        self.here("it".to_owned(), &node.location(), false);
    }

    // Instance variables: no depth, so `self` is what decides.

    fn visit_instance_variable_read_node(&mut self, node: &InstanceVariableReadNode<'pr>) {
        self.instance(&node.name(), &node.location(), false);
    }

    fn visit_instance_variable_write_node(&mut self, node: &InstanceVariableWriteNode<'pr>) {
        self.instance(&node.name(), &node.name_loc(), true);
        ruby_prism::visit_instance_variable_write_node(self, node);
    }

    fn visit_instance_variable_target_node(&mut self, node: &InstanceVariableTargetNode<'pr>) {
        self.instance(&node.name(), &node.location(), true);
    }

    fn visit_instance_variable_and_write_node(&mut self, node: &InstanceVariableAndWriteNode<'pr>) {
        self.instance(&node.name(), &node.name_loc(), true);
        ruby_prism::visit_instance_variable_and_write_node(self, node);
    }

    fn visit_instance_variable_or_write_node(&mut self, node: &InstanceVariableOrWriteNode<'pr>) {
        self.instance(&node.name(), &node.name_loc(), true);
        ruby_prism::visit_instance_variable_or_write_node(self, node);
    }

    fn visit_instance_variable_operator_write_node(
        &mut self,
        node: &InstanceVariableOperatorWriteNode<'pr>,
    ) {
        self.instance(&node.name(), &node.name_loc(), true);
        ruby_prism::visit_instance_variable_operator_write_node(self, node);
    }
}

/// A name as Prism interned it. Source is `&str`, so the bytes are always valid UTF-8.
/// Where a declaration added to the top of a namespace body would go.
fn opens(body: Option<&Node<'_>>) -> Option<u32> {
    body.map(|body| body.location().start_offset() as u32)
}

fn spelled(name: &ConstantId<'_>) -> String {
    String::from_utf8_lossy(name.as_slice()).into_owned()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// The occurrences at the `~`, drawn over the source: `w` under a write, `r` under a read.
    ///
    /// Drawn rather than asserted as offsets for the reason the signature card is: a span one
    /// character out lands under the wrong text, which reads as the bug it is, and a list of
    /// numbers reads as nothing. Lines with nothing marked are dropped, so what an assertion
    /// shows is what lit up — and the lines that remain carry their own text, which is what
    /// makes "and not that other one" something the fixture shows rather than claims.
    fn drawn(marked: &str) -> String {
        let offset = marked.find('~').expect("a ~ marking the cursor") as u32;
        let source = marked.replace('~', "");
        let Some(found) = occurrences(&source, offset) else {
            return "none".to_owned();
        };

        // Indexed in bytes, because that is what an `Occurrence` is measured in; the line
        // breaks are kept so the mask lines up with the source line for line.
        let mut mask: Vec<u8> = source
            .bytes()
            .map(|byte| if byte == b'\n' { b'\n' } else { b' ' })
            .collect();
        for at in &found {
            for cell in &mut mask[at.start as usize..at.end as usize] {
                *cell = if at.write { b'w' } else { b'r' };
            }
        }

        let mask = String::from_utf8(mask).expect("spaces, newlines and two ASCII letters");
        let mut lines = Vec::new();
        for (line, marks) in source.lines().zip(mask.lines()) {
            if marks.trim().is_empty() {
                continue;
            }
            lines.push(line.to_owned());
            lines.push(marks.trim_end().to_owned());
        }
        lines.join("\n")
    }

    #[test]
    fn a_superclass_is_written_outside_the_class_it_opens() {
        // The trap this module is shaped around. `class Foo < v` reads the *enclosing* scope's
        // `v` and Prism numbers its depth from there, so visiting it after pushing the class
        // scope resolves it against the body and splits one variable into two. Nothing about
        // this is visible in a fixture that never inherits from an expression.
        assert_eq!(
            drawn("v~ = Object\nclass Foo < v\nend\n"),
            "v = Object\n\
             w\n\
             class Foo < v\n\
             \u{20}           r"
        );
    }

    #[test]
    fn a_singleton_receiver_and_a_definee_are_written_outside_too() {
        // The same rule, in the two other places Prism puts an expression beside a scope it is
        // not inside.
        assert_eq!(
            drawn("o~ = Object.new\nclass << o\nend\ndef o.f; end\n"),
            "o = Object.new\n\
             w\n\
             class << o\n\
             \u{20}        r\n\
             def o.f; end\n\
             \u{20}   r"
        );
    }

    #[test]
    fn a_block_sees_the_locals_around_it_and_a_def_does_not() {
        // Two rules of Ruby scoping in one assertion, and neither is written down anywhere in
        // this module: the block's read is the same variable at depth 1, and the `def` opens a
        // scope the name cannot reach out of, so its own `total` is a different variable that
        // happens to be spelled the same.
        assert_eq!(
            drawn(
                "total~ = 0\n[1].each { |n| total += n }\ndef other\n  total = 1\n  total\nend\n"
            ),
            "total = 0\n\
             wwwww\n\
             [1].each { |n| total += n }\n\
             \u{20}              wwwww"
        );
    }

    #[test]
    fn a_parameter_shadows_and_is_not_the_same_variable() {
        assert_eq!(
            drawn("x = 1\n[1].each { |x~| x }\nx\n"),
            "[1].each { |x| x }\n\
             \u{20}           w  r"
        );
    }

    #[test]
    fn the_awkward_spellings_arrive_as_ordinary_targets() {
        // `rescue => e`, a pattern capture and a destructured parameter are all reduced by
        // Prism to nodes this module already handles. There is a test rather than a comment
        // because each of them *looks* like it would need a case of its own.
        assert_eq!(
            drawn("begin\nrescue => e~\n  e\nend\n"),
            "rescue => e\n\
             \u{20}         w\n\
             \u{20} e\n\
             \u{20} r"
        );
        assert_eq!(
            drawn("case x\nin [a~, b] then a\nend\n"),
            "in [a, b] then a\n\
             \u{20}   w          r"
        );
        assert_eq!(
            drawn("def f(a, (b~, c))\n  b\nend\n"),
            "def f(a, (b, c))\n\
             \u{20}         w\n\
             \u{20} b\n\
             \u{20} r"
        );
    }

    #[test]
    fn every_parameter_kind_is_a_variable_and_an_anonymous_one_is_not() {
        // A keyword parameter's name span carries its colon and no other spelling of the name
        // does, so highlighting `d:` where the name is `d` would draw over the punctuation.
        assert_eq!(
            drawn("def f(a, b = 1, *c, d~:, e: 2, **f, &g)\n  [a, b, c, d, e, f, g]\nend\n"),
            "def f(a, b = 1, *c, d:, e: 2, **f, &g)\n\
             \u{20}                   w\n\
             \u{20} [a, b, c, d, e, f, g]\n\
             \u{20}           r"
        );
        // An anonymous `*`, `**` or `&` names nothing, so the cursor on one finds no variable
        // and the caller falls through to the graph rather than highlighting a lone sigil.
        assert_eq!(drawn("def f(*~, **, &)\nend\n"), "none");
    }

    #[test]
    fn a_block_local_is_the_blocks_own() {
        assert_eq!(
            drawn("y = 1\n[1].each { |n; y~| y = n }\ny\n"),
            "[1].each { |n; y| y = n }\n\
             \u{20}              w  w"
        );
    }

    #[test]
    fn an_operator_assignment_is_a_write() {
        // It reads as well as writes, and the write is what a reader scanning for where a value
        // comes from is looking for.
        assert_eq!(
            drawn("count = 0\ncount~ += 1\ncount ||= 2\ncount &&= 3\ncount\n"),
            "count = 0\n\
             wwwww\n\
             count += 1\n\
             wwwww\n\
             count ||= 2\n\
             wwwww\n\
             count &&= 3\n\
             wwwww\n\
             count\n\
             rrrrr"
        );
    }

    #[test]
    fn it_and_the_numbered_parameter_belong_to_their_own_block() {
        assert_eq!(
            drawn("[1].each { it~ + it }\n[1].each { it }\n"),
            "[1].each { it + it }\n\
             \u{20}          rr   rr"
        );
        assert_eq!(
            drawn("[1].each { _1~ + _1 }\n[1].each { _1 }\n"),
            "[1].each { _1 + _1 }\n\
             \u{20}          rr   rr"
        );
    }

    #[test]
    fn a_lambda_opens_a_scope_and_a_parameter_of_its_own() {
        // A lambda closes over the locals around it exactly as a block does, and its parameter
        // shadows exactly as a block's does. It is a node of its own in Prism, so it is a case
        // of its own here.
        assert_eq!(
            drawn("n = 1\nadd = ->(n~) { n + 1 }\nn\n"),
            "add = ->(n) { n + 1 }\n\
             \u{20}        w    r"
        );
        assert_eq!(
            drawn("total~ = 1\nrun = -> { total }\ntotal\n"),
            "total = 1\n\
             wwwww\n\
             run = -> { total }\n\
             \u{20}          rrrrr\n\
             total\n\
             rrrrr"
        );
    }

    #[test]
    fn an_instance_variable_is_written_however_the_assignment_is_spelled() {
        // Four spellings of the same write, each a node of its own: the plain assignment, the
        // three operator forms, and the target form a multiple assignment and a `rescue` use.
        assert_eq!(
            drawn(
                "class Foo\n  def a\n    @v~ = 1\n    @v ||= 2\n    @v &&= 3\n    @v += 4\n    @v, @w = 5, 6\n    @v\n  end\nend\n"
            ),
            "    @v = 1\n\
             \u{20}   ww\n\
             \u{20}   @v ||= 2\n\
             \u{20}   ww\n\
             \u{20}   @v &&= 3\n\
             \u{20}   ww\n\
             \u{20}   @v += 4\n\
             \u{20}   ww\n\
             \u{20}   @v, @w = 5, 6\n\
             \u{20}   ww\n\
             \u{20}   @v\n\
             \u{20}   rr"
        );
        assert_eq!(
            drawn("begin\nrescue => @e~\n  @e\nend\n"),
            "rescue => @e\n\
             \u{20}         ww\n\
             \u{20} @e\n\
             \u{20} rr"
        );
    }

    #[test]
    fn an_instance_variable_belongs_to_the_object_self_is() {
        // The whole `SelfContext` algebra in one file. The class body, `def self.b` and the
        // `def c` inside `class << self` are all the same object — Ruby says so, and a reader
        // who has ever debugged a class-level `@cache` knows it — while `def a` is an instance
        // and `def self.b`'s singleton body is one step further up again.
        let source = "class Foo\n  @v = 1\n  def a; @v; end\n  def self.b; @v; end\n  class << self\n    def c; @v; end\n    @v = 2\n  end\nend\n";
        assert_eq!(
            drawn(&source.replace("@v = 1", "@v~ = 1")),
            "  @v = 1\n\
             \u{20} ww\n\
             \u{20} def self.b; @v; end\n\
             \u{20}             rr\n\
             \u{20}   def c; @v; end\n\
             \u{20}          rr"
        );
        // The instance's, which is none of those.
        assert_eq!(
            drawn(&source.replace("def a; @v", "def a; @v~")),
            "  def a; @v; end\n\
             \u{20}        rr"
        );
        // And the singleton class's own, which is one step above the class object.
        assert_eq!(
            drawn(&source.replace("    @v = 2", "    @v~ = 2")),
            "    @v = 2\n\
             \u{20}   ww"
        );
    }

    #[test]
    fn a_class_reopened_under_the_same_spelling_keeps_its_instance_variables_together() {
        // Keyed by the path as written rather than by a counter, so the two bodies are one
        // object — which they are.
        assert_eq!(
            drawn(
                "class Foo\n  @v~ = 1\nend\nclass Foo\n  @v\nend\nmodule Bar\n  class Foo\n    @v\n  end\nend\n"
            ),
            "  @v = 1\n\
             \u{20} ww\n\
             \u{20} @v\n\
             \u{20} rr"
        );
    }

    #[test]
    fn a_block_does_not_change_what_self_is() {
        assert_eq!(
            drawn("class Foo\n  def a\n    [1].each { @v~ = 1 }\n    @v\n  end\nend\n"),
            "    [1].each { @v = 1 }\n\
             \u{20}              ww\n\
             \u{20}   @v\n\
             \u{20}   rr"
        );
    }

    /// Every write `writes_to` is offered, drawn as what it kept.
    ///
    /// The four rejections are the test. Asking by name instead of by offset means the caller
    /// has no cursor to prove which variable it meant, so every part of the identity — the
    /// spelling, the class, and how many singleton steps above an instance it is — has to be
    /// re-checked here rather than assumed.
    #[test]
    fn the_writes_a_class_makes_to_one_of_its_instance_variables() {
        let source = "\
class StoriesController
  def show
    @story = Story.new
    @draft = Draft.new
    @story
  end

  def edit
    @story = Story.find
  end

  def self.seed
    @story = Seed.new
  end

  class << self
    def warm
      @story = Warm.new
    end
  end
end

class CommentsController
  def show
    @story = Comment.new
  end
end
";
        let written: Vec<&str> = writes_to(source, "StoriesController", "@story")
            .iter()
            .map(|occurrence| {
                source[occurrence.start as usize..]
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim_end()
            })
            .collect();
        assert_eq!(
            written,
            ["@story = Story.new", "@story = Story.find"],
            "a read, another variable, a singleton def, a `class << self`, and another class \
             are all not this"
        );

        // The name is matched whole, and the class is the one written rather than any class.
        assert!(writes_to(source, "StoriesController", "@stories").is_empty());
        assert!(writes_to(source, "PostsController", "@story").is_empty());
    }

    #[test]
    fn the_cursor_on_anything_else_finds_no_variable() {
        // Every one of these is a position the caller has to be able to fall through from: a
        // method call, a constant, a comment, a string, and past the end of the file.
        for marked in [
            "foo~\n",
            "Foo~\n",
            "# name~\n",
            "x = \"na~me\"\n",
            "module Empty\nend\nfoo~\n",
            "@@v~ = 1\n",
            "$v~ = 1\n",
        ] {
            assert_eq!(drawn(marked), "none", "{marked:?}");
        }
    }

    /// The selection a fixture marks with two `~`, and the parameters a range would need.
    fn borrowed(marked: &str) -> Crossing {
        let start = marked.find('~').expect("a ~ opening the range") as u32;
        let rest = marked.replacen('~', "", 1);
        let end = rest[start as usize..]
            .find('~')
            .map(|at| start + at as u32)
            .expect("a ~ closing the range");
        crossing(&rest.replacen('~', "", 1), start, end)
    }

    #[test]
    fn a_range_borrows_the_locals_it_reads_before_it_writes_them() {
        // The first occurrence inside decides, and every line here is a different answer from
        // it: `a` is read before the range writes anything, `b` is read and never written at
        // all, `c` is the range's own because the range assigns it first, and `d` is never
        // touched inside.
        assert_eq!(
            borrowed(
                "\
a = 1
b = 2
d = 3
~puts a
a = a + 1
c = b
puts c~
puts d
"
            ),
            Crossing {
                reads: vec!["a".to_owned(), "b".to_owned()],
                escapes: false,
            }
        );
        // In the order the range reaches for them, which is the order the parameters go in.
        assert_eq!(
            borrowed("x = 1\ny = 2\n~puts y\nputs x~\n").reads,
            ["y", "x"]
        );
    }

    #[test]
    fn a_local_the_range_writes_and_something_after_it_touches_escapes() {
        // A read after the range and a write after it both count, and the second is why:
        // `Occurrence` records `y = 1` and `y += 1` the same way, so telling the one that needs
        // the old value from the one that does not is a distinction this does not have.
        assert!(borrowed("~y = 1~\nputs y\n").escapes);
        assert!(borrowed("~y = 1~\ny += 1\n").escapes);
        assert!(borrowed("y = 0\n~y = 1~\nputs y\n").escapes);
        // Written inside and never touched again: the range's own local.
        assert!(!borrowed("~y = 1\nputs y~\nputs 2\n").escapes);
        // Read inside and read after, but never written inside: nothing to hand back.
        assert!(!borrowed("y = 1\n~puts y~\nputs y\n").escapes);
    }

    #[test]
    fn which_variable_a_range_borrows_is_the_scope_stack_and_not_the_name() {
        // Two `n`s: the block's own, which the range writes before it reads, and the method's,
        // which the range never mentions. A name-based answer would pass one of them.
        assert_eq!(
            borrowed("n = 1\n~[2].each { |n| puts n }~\nputs n\n"),
            Crossing {
                reads: Vec::new(),
                escapes: false,
            }
        );
        // An instance variable is not a local and is never a parameter: it travels with `self`,
        // which an extracted method in the same class keeps.
        assert_eq!(borrowed("~puts @count~\n").reads, Vec::<String>::new());
    }

    #[test]
    fn an_accessor_belongs_to_the_class_whose_instance_the_variable_is_on() {
        // The offset is where the body begins, which is where a declaration goes.
        let source = "class Story\n  def bump\n    @views = 1\n  end\nend\n";
        assert_eq!(
            accessor_site(source, source.find("@views").unwrap() as u32),
            Some(("@views".to_owned(), source.find("def bump").unwrap() as u32))
        );
        // A nested namespace answers with its own body rather than the one around it.
        let nested =
            "module Outer\n  class Inner\n    def bump\n      @views = 1\n    end\n  end\nend\n";
        assert_eq!(
            accessor_site(nested, nested.find("@views").unwrap() as u32),
            Some(("@views".to_owned(), nested.find("def bump").unwrap() as u32))
        );
    }

    #[test]
    fn nothing_that_is_not_an_instances_variable_has_an_accessor_site() {
        // The whole of what the caller cannot work out for itself. Every one of these is an
        // `@count` written inside a class, and for none of them would `attr_reader :count` read
        // the variable being looked at: the first three are the class object's rather than an
        // instance's, the fourth hangs off an object nobody can name without types, and the
        // last has no class body to declare into at all.
        for marked in [
            "class Foo\n  def self.count\n    @count~\n  end\nend\n",
            "class Foo\n  class << self\n    def count\n      @count~\n    end\n  end\nend\n",
            "class Foo\n  @count~ = 1\nend\n",
            "obj = Object.new\nclass << obj\n  def count\n    @count~\n  end\nend\n",
            "def count\n  @count~\nend\n",
            // And a class with no body at all, which is where the site comes from.
            "class Empty\nend\nclass Foo\n  def count\n    @other\n  end\nend\n@count~ = 1\n",
            // A local is not an instance variable.
            "class Foo\n  def count\n    count~ = 1\n  end\nend\n",
            // A cursor *before* every site in the file, which is the other way the lookup can
            // miss: the file has one and it is not this.
            "~class Foo\n  def count\n    @count\n  end\nend\n",
        ] {
            let offset = marked.find('~').expect("a ~ marking the cursor") as u32;
            assert_eq!(
                accessor_site(&marked.replace('~', ""), offset),
                None,
                "{marked:?}"
            );
        }
    }
}
