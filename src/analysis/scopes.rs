//! Which variable is which, for the variables rubydex does not model.
//!
//! The graph knows constants and methods. It does not model local variables at all. It records an
//! instance variable's *assignment* as a declaration but no references to one, so a bare `@name` is
//! invisible to it. Both are ordinary cursor targets, so this module owns their scope rules, using
//! Prism directly.
//!
//! # Why the scopes are Prism's rather than ours
//!
//! Deciding which `x` is which by hand means reimplementing Ruby's scoping:
//! - a block sees the locals around it, a `def` does not;
//! - a block parameter shadows the local it is spelled like;
//! - `for` declares into the enclosing scope, `->() {}` does not.
//!
//! Prism has already done it. Every local-variable node carries a **`depth`**: how many scopes out
//! the name resolved to. Two occurrences are the same variable exactly when they share a name and
//! land on the same scope-stack entry. So `x = 1; [1].each { |x| x }` separates correctly with no
//! shadowing rule written here.
//!
//! The awkward spellings arrive already reduced: `rescue => e` and `in [a, b]` are both
//! `LocalVariableTargetNode`, `def f(a, (b, c))` destructures into plain parameters, and `_1` reads
//! a local the block declares. None needs a case.
//!
//! # Instance variables are scoped by what `self` is
//!
//! `@v` in `def a` and `@v` in `def self.b` are different variables: one belongs to an instance,
//! the other to the class object. A highlight that joins them is visibly wrong. There is no depth
//! to read, so [`SelfContext`] tracks `self`:
//! - a namespace body *is* the class object;
//! - `class << self` is one singleton step above it;
//! - a `def` with no receiver is an instance of whatever `self` is where it is written.
//!
//! The last rule makes `def c` inside `class << self` land back on the class, sharing its `@v` with
//! `def self.b`.

use std::{cell::RefCell, rc::Rc};

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
    /// An operator assignment (`x += 1`, `@v ||= []`) is a write. It reads too, but a reader
    /// scanning a highlighted file wants to see where the value comes from.
    pub write: bool,
}

/// Every occurrence of the variable under `offset`, or `None` when the cursor is not on one.
///
/// `None` covers everything else in a Ruby file (a method call, a constant, a comment, the inside
/// of a string) and lets the caller fall through to the graph.
#[must_use]
pub fn occurrences(source: &str, offset: u32) -> Option<Vec<Occurrence>> {
    variable(source, offset).map(|(_, occurrences)| occurrences)
}

/// The variable under `offset`: its name as Ruby spells it, and every place it appears.
///
/// The name comes from here, not cut out of the source, because the identity the occurrences share
/// already *is* the name. A caller has nothing to re-derive and no absent case. An instance
/// variable's name carries its `@`, which is how [`rename`](super::rename) recognises one without
/// re-reading the syntax.
#[must_use]
pub fn variable(source: &str, offset: u32) -> Option<(String, Vec<Occurrence>)> {
    scoped(source).under(offset)
}

/// Every write to `@name` on an *instance* of the class written as `path`.
///
/// [`variable`] asks "which occurrences share the one under this cursor?". One caller has no cursor
/// in the file: a template's instance variables are assigned in a controller, and the question
/// comes from the view. So this is the same algebra entered by name:
/// - `path` is the namespace as the file spells it;
/// - an instance is `level` 0, so `def self.` and `class << self` are excluded exactly as for a
///   cursor.
///
/// A second copy of that algebra in the caller would be certain to drift.
///
/// Writes only: a variable's type is what was assigned to it, and a read has no value to look at.
/// Ordered by offset, so the caller takes "the textually last one" itself.
#[must_use]
pub fn writes_to(source: &str, path: &str, name: &str) -> Vec<Occurrence> {
    scoped(source).written(path, name)
}

/// Which locals a byte range borrows from around it, and whether anything it writes escapes.
///
/// [`code_actions`](super::code_actions) asks this before lifting statements into a method of their
/// own. It is answered here because the answer is the scope stack: two `x`s Prism resolved to
/// different scopes are two variables, and an extraction that passed one and left the other would
/// compile and be wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crossing {
    /// Locals the range reads before it writes them: the parameters an extracted method needs, in
    /// the order the range first reaches for them.
    pub reads: Vec<String>,
    /// `true` when the range writes a local that is touched again after it.
    ///
    /// An extracted method has no single value to hand back for that, so the caller declines
    /// instead of approximating. Any later occurrence counts, read or write: [`Occurrence`] records
    /// `x = 1` and `x += 1` both as writes, and cannot tell the one needing the old value from the
    /// one that does not.
    pub escapes: bool,
}

/// Which locals `source[start..end]` reads from outside itself, and what it writes that outlives
/// it.
#[must_use]
pub fn crossing(source: &str, start: u32, end: u32) -> Crossing {
    scoped(source).crossing(start, end)
}

/// The instance variable at `offset` and where an accessor for it would be declared, or `None` when
/// there is no such place.
///
/// `Some` only where the variable belongs to an **instance** of a namespace written in this file.
/// That condition is the point. `attr_reader :count` reads an *instance's* `@count`, so offering it
/// for the `@count` in `def self.count` or `class << self` writes an accessor for a different
/// variable: code that runs, returns `nil`, and looks right. [`SelfContext`]'s level already knows
/// the difference; a copy of that algebra in the caller would drift.
///
/// The offset is where the namespace's body begins, which is where the declaration goes.
#[must_use]
pub fn accessor_site(source: &str, offset: u32) -> Option<(String, u32)> {
    scoped(source).accessor(offset)
}

/// The walk's answers for one source text. All four questions above are asked of it.
///
/// **It holds no borrow of the text.** A [`Variable`] carries its name as a `String` and an
/// [`Occurrence`] is four numbers, so the visitor's `&str` is never read after the visit. That is
/// why the memo below can exist. Keep it so if a new question wants to cut text out of the source.
struct Scoped {
    /// Every occurrence in the file with the identity it shares, sorted by offset **once**.
    ///
    /// Sorted here, not in each question: "the first one covering the cursor" is a fact about the
    /// file, not about the order the visitor recorded writes in.
    found: Vec<(Variable, Occurrence)>,
    /// Every instance-variable occurrence that belongs to an instance of a namespace, with the
    /// name and the offset an accessor for it would be declared at.
    sites: Vec<(Occurrence, String, u32)>,
}

/// How many source texts the memo holds at once.
///
/// **Two: that is how many are in play at a time.**
/// - A request asks about the buffer under the cursor.
/// - The one caller with a second text is [`cursor`](super::cursor)'s `type_the_instance_variable`.
///   It walks a *repaired* copy of that buffer (the half-typed call removed), alternating with the
///   real one in a loop.
///
/// One slot would thrash between the two. A map keyed by document would need a bound, an eviction
/// rule and a reason to trust both; decide that when a third text turns up.
///
/// More slots do not help. Almost every remaining miss is **compulsory**: a sweep asks about
/// thousands of distinct files, and nearly every walk is the first anyone asked of that text. No
/// cache size skips that.
const SOURCES_HELD: usize = 2;

thread_local! {
    /// The walks already done, most recently walked first.
    ///
    /// **Keyed by the text itself, compared for equality, not hashed.**
    /// - The four questions are pure in `source`, so identical text has identical answers. No
    ///   second input, no stamp, no invalidation rule to get wrong; the argument
    ///   [`Knowledge::refresh`](crate::knowledge::Knowledge::refresh) makes for memoising a parse.
    /// - Equality has no collision class. A hash would risk answering about a different file, at
    ///   the tier a reader can least check.
    /// - A `memcmp` over a document is far cheaper than the parse.
    ///
    /// **Thread-local, so nothing is shared or locked.** A second thread gets its own, which is
    /// right: this is a cache, and every entry is reproducible from its key.
    ///
    /// A hit does not reorder. With two slots, reordering could only decide which of two live texts
    /// survives a third, and the alternating pair above is the only pattern there is.
    static WALKED: RefCell<Vec<(Box<str>, Rc<Scoped>)>> = const { RefCell::new(Vec::new()) };
}

/// The walk for `source`, done once per text instead of once per question.
///
/// The parse is Prism's own work, so there is nothing to make faster here, only a reason not to
/// repeat it. Most calls arrive through [`locator::variable_at`](super::locator::variable_at):
/// `definition`, `hover` and `documentHighlight` all ask about the same cursor in a document the
/// settle just parsed, and `documentHighlight` fires on every cursor move.
fn scoped(source: &str) -> Rc<Scoped> {
    // Looked up and released before the walk, not held across it. `Scoped::of` runs with no borrow
    // of `WALKED` outstanding, so a future question that reached back in here could not panic on
    // the `RefCell`.
    let held = WALKED.with_borrow(|walked| {
        walked
            .iter()
            .find(|(text, _)| &**text == source)
            .map(|(_, scoped)| Rc::clone(scoped))
    });
    if let Some(scoped) = held {
        return scoped;
    }
    let fresh = Rc::new(Scoped::of(source));
    WALKED.with_borrow_mut(|walked| {
        if walked.len() == SOURCES_HELD {
            walked.pop();
        }
        walked.insert(0, (Box::from(source), Rc::clone(&fresh)));
    });
    fresh
}

/// The texts the memo holds, most recently walked first.
///
/// Test-only. A test cannot otherwise see a walk that did not happen: the answers are identical
/// either way, which is the point.
#[cfg(test)]
fn walked() -> Vec<String> {
    WALKED.with_borrow(|walked| walked.iter().map(|(text, _)| text.to_string()).collect())
}

/// Drop everything the memo holds, so a test about the memo starts from a known state.
///
/// Test-only. `cargo test --test-threads=1` runs every test on one thread and so on one `WALKED`; a
/// test asserting about its contents must say where it starts.
#[cfg(test)]
fn forget() {
    WALKED.with_borrow_mut(Vec::clear);
}

impl Scoped {
    /// Parse, walk and sort: the work the memo exists to do once.
    fn of(source: &str) -> Self {
        let result = ruby_prism::parse(source.as_bytes());
        let mut walk = Walk::new(source);
        walk.visit(&result.node());
        walk.found.sort_by_key(|(_, occurrence)| occurrence.start);
        Self {
            found: walk.found,
            sites: walk.sites,
        }
    }

    /// The name of the variable the cursor is on, and the occurrences sharing it.
    fn under(&self, offset: u32) -> Option<(String, Vec<Occurrence>)> {
        let (variable, _) = self
            .found
            .iter()
            // Inclusive of the end, as in `locator::covers`: a cursor just past a name's last
            // character is still on it.
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
    fn written(&self, path: &str, name: &str) -> Vec<Occurrence> {
        self.found
            .iter()
            .filter(|(variable, occurrence)| {
                occurrence.write
                    && matches!(
                        variable,
                        Variable::Instance { name: spelled, owner }
                            if spelled == name && owner.path == path && owner.level == 0
                    )
            })
            .map(|(_, occurrence)| occurrence.clone())
            .collect()
    }

    /// The parameters a range would need, and whether anything it writes outlives it.
    fn crossing(&self, start: u32, end: u32) -> Crossing {
        let inside = |at: &Occurrence| start <= at.start && at.end <= end;

        let mut crossing = Crossing {
            reads: Vec::new(),
            escapes: false,
        };
        let mut seen: Vec<&Variable> = Vec::new();
        // Walked over what is *inside* the range, in order. The first occurrence of a variable
        // reached here is the range's first, which decides the answer and the parameter order.
        for (variable, at) in &self.found {
            let Variable::Local { name, .. } = variable else {
                continue;
            };
            if !inside(at) || seen.contains(&variable) {
                continue;
            }
            seen.push(variable);
            // A read means the value came from outside and must be passed in. A write means the
            // range declares the variable, so an extracted method would declare it.
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
    fn accessor(&self, offset: u32) -> Option<(String, u32)> {
        self.sites
            .iter()
            // Inclusive of the end, as in `under`: a cursor just past a name's last character is
            // still on it.
            .find(|(at, _, _)| at.start <= offset && offset <= at.end)
            .map(|(_, name, body)| (name.clone(), *body))
    }
}

/// What `self` is at a point in the file, which is what an instance variable belongs to.
///
/// `level` counts singleton steps above an *instance* of `path`:
/// - 0 is an instance;
/// - 1 is the class object;
/// - 2 is its singleton class.
///
/// Entering `class << self` adds a step and a receiverless `def` takes one away. So `def self.b`
/// and a `def c` inside `class << self` arrive at the same context, as in Ruby.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SelfContext {
    /// The lexical namespace path as written, so a class reopened in the same file under the same
    /// spelling keeps its instance variables together.
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
    /// The scope stack, innermost last. A local at `depth` was declared in the entry `depth` places
    /// from the end; that indexing is the whole scoping logic.
    scopes: Vec<u32>,
    next_scope: u32,
    this: SelfContext,
    found: Vec<(Variable, Occurrence)>,
    /// Where the body of the namespace `self` belongs to begins. `None` when no such namespace is
    /// written in this file: the top level, and either island.
    body: Option<u32>,
    /// Every instance-variable occurrence that belongs to an instance of a namespace, with the name
    /// and the offset an accessor for it would be declared at.
    ///
    /// Kept beside [`Self::found`], not inside it, because it answers a different question. `found`
    /// is "which occurrences are the same variable"; a body offset in [`Variable`] would split a
    /// class reopened in one file into two variables.
    sites: Vec<(Occurrence, String, u32)>,
}

impl<'s> Walk<'s> {
    fn new(source: &'s str) -> Self {
        Self {
            source,
            // The file's own scope, pushed before the walk so the stack is never empty and neither
            // lookup below has an absent case.
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

    /// Run `body` inside a freshly numbered local scope.
    fn scoped(&mut self, body: impl FnOnce(&mut Self)) {
        self.scopes.push(self.next_scope);
        self.next_scope += 1;
        body(self);
        self.scopes.pop();
    }

    /// Run `run` with `self` bound to `this`, and `site` as where an accessor for an instance of it
    /// would be declared.
    ///
    /// The two travel together because they are decided together. Every construct that changes
    /// `self` either opens a namespace body, keeps the one around it, or is an island with no body.
    /// Separating them is how one of the three would get missed.
    fn as_self(&mut self, this: SelfContext, site: Option<u32>, run: impl FnOnce(&mut Self)) {
        let outer = std::mem::replace(&mut self.this, this);
        let outside = std::mem::replace(&mut self.body, site);
        run(self);
        self.this = outer;
        self.body = outside;
    }

    /// The scope a local resolved `depth` steps out from the innermost one.
    ///
    /// Total on purpose: the file's scope is on the stack before the walk, and Prism never resolves
    /// a name to a scope it did not parse. A depth past the outermost lands on the file.
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

    /// A parameter, a block-local or `it`: declared in the innermost scope, with no depth to read
    /// because it could come from nowhere else.
    fn here(&mut self, name: String, at: &Location<'_>, write: bool) {
        let scope = self.scope_at(0);
        self.record(Variable::Local { name, scope }, at, write);
    }

    fn instance(&mut self, name: &ConstantId<'_>, at: &Location<'_>, write: bool) {
        let spelling = spelled(name);
        // Level 0 is an instance, the only level an `attr_` accessor can read. A namespace body is
        // the class object and `class << self` is a step above it, so both drop out here with no
        // rule of their own. So does the top level, which has no body to declare into.
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
        // A keyword parameter's name span carries its colon (`d:`), and no other spelling of the
        // name does. Trimmed here, not at each call site, so the four keyword spellings cannot
        // disagree.
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

    /// A singleton or definee that is some object other than `self`: `class << obj`, `def obj.f`.
    /// What it is needs types, so it gets its own island named by where it is written. Two islands
    /// never share an instance variable: better to highlight too little than to join two unrelated
    /// ones.
    fn island(&self, at: &Location<'_>) -> SelfContext {
        SelfContext {
            path: format!("{}<{}>", self.this.path, at.start_offset()),
            level: 1,
        }
    }
}

impl<'pr> Visit<'pr> for Walk<'_> {
    // The four scope openers below visit their children by hand, not through the default walk,
    // because **a superclass, a singleton's receiver and a definee are written outside the scope
    // they open**. `class Foo < v` reads the enclosing scope's `v`, and Prism numbers its depth
    // from there. Visiting it after the push would resolve it against the class body and split one
    // variable in two.

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
        // `class << self` is a step above the namespace around it and keeps its body.
        // `class << obj` is an island, with no body an accessor could go in.
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

    // A block and a lambda open a scope but not a `self`: `self` inside is what it was outside. So
    // `@v` in a block belongs to the enclosing method's object.

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

    // Parameters and block-locals: declared in the scope being visited, so there is no depth to
    // read.

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
        // An anonymous `*` or `**` has no name and nothing to highlight. rubydex records those
        // under the sigil; here they are not a variable.
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
        // `it` names no variable Prism resolves, so it is keyed by the block it reads in: the scope
        // `_1` would be declared in, so the two behave alike.
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

/// Where a declaration added to the top of a namespace body would go.
fn opens(body: Option<&Node<'_>>) -> Option<u32> {
    body.map(|body| body.location().start_offset() as u32)
}

/// A name as Prism interned it. Source is `&str`, so the bytes are always valid UTF-8.
fn spelled(name: &ConstantId<'_>) -> String {
    String::from_utf8_lossy(name.as_slice()).into_owned()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// What the memo holds, and that holding it changes no answer.
    ///
    /// The memo is invisible in every answer, so this test reaches for [`walked`] to see it. Two
    /// properties:
    /// 1. A text already walked is not walked again.
    /// 2. A third text evicts the older of the two held: [`SOURCES_HELD`] is the alternating pair
    ///    `cursor` asks about, nothing wider.
    #[test]
    fn a_text_is_walked_once_and_a_third_evicts_the_older_of_two() {
        forget();
        let a = "class A\n  def a\n    @v = 1\n    @v\n  end\nend\n";
        let b = "class B\n  def b\n    @w = 2\n  end\nend\n";
        let c = "class C\n  def c\n    @x = 3\n  end\nend\n";
        let at = |source: &str, name: &str| source.find(name).expect("the variable") as u32;

        let cold = variable(a, at(a, "@v"));
        assert!(
            cold.is_some(),
            "the fixture has a variable under the cursor"
        );
        assert_eq!(walked(), vec![a.to_owned()]);

        // The same text again is answered from the memo: nothing added, and the answer is the
        // walk's.
        assert_eq!(variable(a, at(a, "@v")), cold);
        assert_eq!(walked(), vec![a.to_owned()]);

        // A second text joins it instead of replacing it: the reason for two slots.
        // `cursor::Finder::type_the_instance_variable` alternates a buffer with a repaired copy of
        // itself, and one slot would hold neither.
        assert_eq!(
            writes_to(b, "B", "@w").len(),
            1,
            "the fixture writes @w once"
        );
        assert_eq!(walked(), vec![b.to_owned(), a.to_owned()]);
        assert_eq!(
            variable(a, at(a, "@v")),
            cold,
            "the first is still there to answer"
        );
        assert_eq!(walked(), vec![b.to_owned(), a.to_owned()]);

        // A third evicts the older of the two.
        assert!(
            accessor_site(c, at(c, "@x")).is_some(),
            "the fixture has a site"
        );
        assert_eq!(walked(), vec![c.to_owned(), b.to_owned()]);
    }

    /// Every question answers the same warm as cold: what a memo owes.
    ///
    /// All four are asked, not only [`variable`], because they share one [`Scoped`]. A question
    /// reading something the memo does not carry would be wrong only on the second call, and only
    /// for that question.
    #[test]
    fn every_question_answers_the_same_warm_as_cold() {
        let source = "class Story\n  def bump(n)\n    @views = n\n    total = @views + 1\n    total\n  end\nend\n";
        let offset = source.find("@views").expect("the variable") as u32;
        let range = (
            source.find("total = ").expect("the range") as u32,
            source.find("    total\n").expect("the range") as u32,
        );

        forget();
        let cold = (
            variable(source, offset),
            writes_to(source, "Story", "@views"),
            crossing(source, range.0, range.1),
            accessor_site(source, offset),
        );
        // Cold again, so two walks are compared instead of a walk with itself.
        forget();
        assert_eq!(variable(source, offset), cold.0);
        // Now warm: the three below are answered off the walk the line above stored.
        assert_eq!(writes_to(source, "Story", "@views"), cold.1);
        assert_eq!(crossing(source, range.0, range.1), cold.2);
        assert_eq!(accessor_site(source, offset), cold.3);
        assert_eq!(walked().len(), 1, "one text, one walk");
    }

    /// The occurrences at the `~`, drawn over the source: `w` under a write, `r` under a read.
    ///
    /// Drawn, not asserted as offsets, for the same reason as the signature card: a span one
    /// character out lands under the wrong text and reads as the bug it is, while a list of numbers
    /// reads as nothing. Lines with nothing marked are dropped, so the assertion shows what lit up,
    /// and the remaining lines carry their own text. That lets "and not that other one" be shown,
    /// not claimed.
    fn drawn(marked: &str) -> String {
        let offset = marked.find('~').expect("a ~ marking the cursor") as u32;
        let source = marked.replace('~', "");
        let Some(found) = occurrences(&source, offset) else {
            return "none".to_owned();
        };

        // Indexed in bytes, as an `Occurrence` is. Line breaks are kept so the mask lines up with
        // the source line for line.
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
        // The trap this module is shaped around. `class Foo < v` reads the *enclosing* scope's `v`,
        // and Prism numbers its depth from there. Visiting it after pushing the class scope
        // resolves it against the body and splits one variable in two. A fixture that never
        // inherits from an expression cannot show this.
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
        // The same rule in the two other places Prism puts an expression beside a scope it is not
        // inside.
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
        // Two Ruby scoping rules in one assertion, neither written down in this module:
        // - the block's read is the same variable, at depth 1;
        // - the `def` opens a scope the name cannot reach out of, so its `total` is a different
        //   variable spelled the same.
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
        // `rescue => e`, a pattern capture and a destructured parameter all reduce to nodes this
        // module already handles. A test, not a comment, because each *looks* like it needs its own
        // case.
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
        // A keyword parameter's name span carries its colon and no other spelling does, so
        // highlighting `d:` for `d` would draw over the punctuation.
        assert_eq!(
            drawn("def f(a, b = 1, *c, d~:, e: 2, **f, &g)\n  [a, b, c, d, e, f, g]\nend\n"),
            "def f(a, b = 1, *c, d:, e: 2, **f, &g)\n\
             \u{20}                   w\n\
             \u{20} [a, b, c, d, e, f, g]\n\
             \u{20}           r"
        );
        // An anonymous `*`, `**` or `&` names nothing. The cursor finds no variable and the caller
        // falls through to the graph instead of highlighting a lone sigil.
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
        // It reads as well as writes, and the write is what a reader looking for a value's source
        // wants.
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
        // A lambda closes over surrounding locals exactly as a block does, and its parameter
        // shadows the same way. It is a separate node in Prism, so it is a separate case here.
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
        // Four spellings of the same write, each its own node: the plain assignment, the three
        // operator forms, and the target form used by multiple assignment and `rescue`.
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
        // The whole `SelfContext` algebra in one file. The class body, `def self.b` and the `def c`
        // inside `class << self` are all the same object, as Ruby says and anyone who debugged a
        // class-level `@cache` knows. `def a` is an instance, and the body of `class << self` is
        // one step further up.
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
        // Keyed by the path as written, not by a counter, so the two bodies are one object, as they
        // are.
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
    /// The four rejections are the test. Asked by name, the caller has no cursor to prove which
    /// variable it meant, so every part of the identity is re-checked here: the spelling, the
    /// class, and how many singleton steps above an instance it is.
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
        // Positions the caller must be able to fall through from: a method call, a constant, a
        // comment, a string, and past the end of the file.
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
        // The first occurrence inside decides, and each line is a different answer:
        // - `a` is read before the range writes anything;
        // - `b` is read and never written;
        // - `c` is the range's own, because the range assigns it first;
        // - `d` is never touched inside.
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
        // In the order the range reaches for them: the parameter order.
        assert_eq!(
            borrowed("x = 1\ny = 2\n~puts y\nputs x~\n").reads,
            ["y", "x"]
        );
    }

    #[test]
    fn a_local_the_range_writes_and_something_after_it_touches_escapes() {
        // A read after the range and a write after it both count. The second is why: `Occurrence`
        // records `y = 1` and `y += 1` the same way, so it cannot tell which needs the old value.
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
        // Two `n`s: the block's own, which the range writes before reading, and the method's, which
        // the range never mentions. A name-based answer would pass one of them.
        assert_eq!(
            borrowed("n = 1\n~[2].each { |n| puts n }~\nputs n\n"),
            Crossing {
                reads: Vec::new(),
                escapes: false,
            }
        );
        // An instance variable is not a local and never a parameter. It travels with `self`, which
        // an extracted method in the same class keeps.
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
        // What the caller cannot work out alone. Each is an `@count` inside a class, and for none
        // would `attr_reader :count` read that variable:
        // - the first three belong to the class object, not an instance;
        // - the fourth hangs off an object nobody can name without types;
        // - the last has no class body to declare into.
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
            // A cursor *before* every site in the file: the other way the lookup can miss. The file
            // has a site, just not this one.
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
