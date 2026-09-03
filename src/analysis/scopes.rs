//! Which variable is which, for the variables rubydex does not model.
//!
//! # Why this exists
//!
//! The graph knows about constants and methods. It knows nothing about local variables —
//! `LocalVariable` does not appear anywhere in rubydex's source — and while it records an
//! instance variable's *assignment* as a declaration it records no references to one, so a bare
//! `@name` is invisible to it. Both are ordinary things to put a cursor on, so answering for
//! them means owning the scope rules, which makes this the third direct use of Prism after
//! `cursor` and `requires`.
//!
//! # Why the scopes are Prism's rather than ours
//!
//! Deciding which `x` is which by hand means reimplementing Ruby's scoping: a block sees the
//! locals around it and a `def` does not, a block parameter shadows the local it is spelled
//! like, `for` declares into the enclosing scope while `->() {}` does not. Prism has already
//! done it. Every local-variable node carries a **`depth`** — how many scopes out the name was
//! resolved to — so two occurrences are the same variable exactly when they have the same name
//! and land on the same entry of the scope stack. Nothing here re-derives that, which is why
//! `x = 1; [1].each { |x| x }` separates correctly without a rule about shadowing being written
//! down anywhere.
//!
//! It also means the awkward spellings arrive already reduced: `rescue => e` and `in [a, b]`
//! are both `LocalVariableTargetNode`, `def f(a, (b, c))` destructures into plain parameters,
//! and `_1` is a read of a local the block declares. None of them needs a case here.
//!
//! # Instance variables are scoped by what `self` is
//!
//! `@v` in `def a` and `@v` in `def self.b` are different variables — one belongs to an
//! instance of the class and the other to the class object — and a highlight that joins them is
//! wrong in a way the user can see. There is no depth to read for these, so [`SelfContext`]
//! tracks it: a namespace body *is* the class object, `class << self` is the object one
//! singleton step above it, and a `def` with no receiver is an instance of whatever `self` is
//! where it is written. That last rule is what makes `def c` inside `class << self` land back
//! on the class, and therefore share its `@v` with `def self.b`.

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

    /// Run `body` inside a freshly numbered local scope.
    fn scoped(&mut self, body: impl FnOnce(&mut Self)) {
        self.scopes.push(self.next_scope);
        self.next_scope += 1;
        body(self);
        self.scopes.pop();
    }

    /// Run `body` with `self` bound to `this`.
    fn as_self(&mut self, this: SelfContext, body: impl FnOnce(&mut Self)) {
        let outer = std::mem::replace(&mut self.this, this);
        body(self);
        self.this = outer;
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
        self.record(
            Variable::Instance {
                name: spelled(name),
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
        self.as_self(this, |walk| {
            walk.scoped(|walk| {
                if let Some(body) = node.body() {
                    walk.visit(&body);
                }
            });
        });
    }

    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        let this = self.namespace(&node.constant_path());
        self.as_self(this, |walk| {
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
        let this = if matches!(expression, Node::SelfNode { .. }) {
            SelfContext {
                path: self.this.path.clone(),
                level: self.this.level + 1,
            }
        } else {
            self.island(&node.location())
        };
        self.as_self(this, |walk| {
            walk.scoped(|walk| {
                if let Some(body) = node.body() {
                    walk.visit(&body);
                }
            });
        });
    }

    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        let this = match node.receiver() {
            None => SelfContext {
                path: self.this.path.clone(),
                level: self.this.level - 1,
            },
            Some(receiver) => {
                self.visit(&receiver);
                if matches!(receiver, Node::SelfNode { .. }) {
                    self.this.clone()
                } else {
                    self.island(&node.location())
                }
            }
        };
        self.as_self(this, |walk| {
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
}
