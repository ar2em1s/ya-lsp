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

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use ruby_prism::{
    BlockLocalVariableNode, BlockNode, BlockParameterNode, CallNode, ClassNode, ConstantId,
    DefNode, InstanceVariableAndWriteNode, InstanceVariableOperatorWriteNode,
    InstanceVariableOrWriteNode, InstanceVariableReadNode, InstanceVariableTargetNode,
    InstanceVariableWriteNode, ItLocalVariableReadNode, KeywordRestParameterNode, LambdaNode,
    LocalVariableAndWriteNode, LocalVariableOperatorWriteNode, LocalVariableOrWriteNode,
    LocalVariableReadNode, LocalVariableTargetNode, LocalVariableWriteNode, Location, ModuleNode,
    Node, OptionalKeywordParameterNode, OptionalParameterNode, RequiredKeywordParameterNode,
    RequiredParameterNode, RestParameterNode, SingletonClassNode, Visit,
};

use super::cursor;

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
    variable(&cursor::Parsed::new(source), offset).map(|(_, _, occurrences)| occurrences)
}

/// The variable under `offset`: its name as Ruby spells it, the occurrence the cursor is on, and
/// every place it appears.
///
/// The name comes from here, not cut out of the source, because the identity the occurrences share
/// already *is* the name. A caller has nothing to re-derive and no absent case. An instance
/// variable's name carries its `@`, which is how [`rename`](super::rename) recognises one without
/// re-reading the syntax.
#[must_use]
pub fn variable(
    text: &cursor::Parsed<'_>,
    offset: u32,
) -> Option<(String, Occurrence, Vec<Occurrence>)> {
    scoped_in(text).under(offset)
}

/// The calls that write an instance variable by name ([`reflections`]): the set, and the removal.
/// A document that writes one may build the name (`"@#{x}"`) and so never spell the variable it
/// writes, which a caller skipping texts by the name must know.
pub(crate) const REFLECTIVE_WRITERS: [&str; 2] =
    ["instance_variable_set", "remove_instance_variable"];

/// One occurrence of an instance variable as the walk places it, for a caller that places a
/// loose one ([`Group::loose`]) by what its block's call says `self` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub occurrence: Occurrence,
    /// [`Group::level`] of the object the walk hangs it off.
    pub level: i32,
    /// In a block or lambda straight in a namespace body, so the walk does not know its `self`.
    pub loose: bool,
}

/// The instance variable under `offset` and every occurrence of its name in the same namespace,
/// at any level: its name, which of them the cursor is on, and all of them in offset order.
///
/// `None` for anything else, including a local and a variable no namespace this file writes
/// owns (the top level, an island). [`variable`] is this family narrowed to the cursor's own
/// level; a caller that knows where a loose occurrence runs regroups it
/// (`locator::occurrences_at`).
#[must_use]
pub fn instance_family(
    text: &cursor::Parsed<'_>,
    offset: u32,
) -> Option<(String, usize, Vec<Placed>)> {
    scoped_in(text).family(offset)
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

/// Every `attr_writer` and `attr_accessor` in a text, as the instance variable each one writes.
///
/// A setter writes `@name` with whatever its caller passes, and no `@name =` is written anywhere
/// for it. [`cursor`](super::cursor)'s table keeps each as a write nothing can type, so a read it
/// can reach is refused instead of being folded over the writes that happen to be spelled out.
///
/// Only where `self` is a namespace this file names: a setter declared at the top level or on an
/// island belongs to an object no type can name here.
#[must_use]
pub fn setters(source: &str) -> Vec<Accessor> {
    scoped(source).setters.clone()
}

/// Every `attr_reader` and `attr_accessor` in a text, as the instance variable each one reads.
///
/// - **[`setters`]' other half, from the same visit**, so a reader and a setter declared by one
///   `attr_accessor` name the same variable at the same level. `types` reads a reader's return as
///   that variable, read on the object ([`types::method_return`](super::types::method_return)).
/// - **Not in a block written straight in a namespace body** ([`Group::loose`]): `included do`
///   runs on the class, `class_methods do` on its singleton, and nothing in the block says which
///   object's variable the reader returns.
#[must_use]
pub fn readers(source: &str) -> Vec<Accessor> {
    scoped(source).readers.clone()
}

/// One instance variable a declared accessor reads or writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accessor {
    /// As Ruby spells the variable, `@` included.
    pub name: String,
    /// What [`Group::level`] says for the variable: 0 for `attr_writer` in a class body.
    pub level: i32,
    /// Where the accessor's name starts, inside the colon or the quotes: where rubydex files the
    /// method it declares.
    pub at: u32,
}

/// Every call in a text that writes an instance variable by reflection:
/// `instance_variable_set` and `remove_instance_variable`.
///
/// No `@name =` is written for these, so without this list a fold over the spelled writes would
/// miss them. [`cursor`](super::cursor)'s table keeps each as a write nothing can type (a removal
/// as `nil`), on the object whose variable it reaches.
#[must_use]
pub fn reflections(source: &str) -> Vec<Reflection> {
    scoped(source).reflections.clone()
}

/// The class variables a text writes by reflection: the first argument of every
/// `class_variable_set`, on any receiver.
///
/// A written `@@name =` is rubydex's; this is the write no `@@` is spelled for. A name no literal
/// spells, a local's included, matches every variable.
#[must_use]
pub fn class_variable_sets(source: &str) -> Vec<Spelled> {
    scoped(source).class_reflections.clone()
}

/// One reflective write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reflection {
    /// Which names it can reach: one spelling for a literal, every value a local can hold for a
    /// local, and the argument callers pass for a method's own parameter.
    pub names: Vec<Spelled>,
    /// Whose variable it writes.
    pub on: Reflected,
    /// `remove_instance_variable`: the variable is `nil` again.
    pub removes: bool,
    /// Where the call's name is written.
    pub at: u32,
}

/// Whose variable a reflective call writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reflected {
    /// `self`'s, at [`Group::level`]; `None` where that is not known, inside a block straight in a
    /// namespace body.
    Own(Option<i32>),
    /// The top level's `self`: `main` in a script, the view in a template. Only the top level reads
    /// its variables.
    Main,
    /// Another object's (`obj.instance_variable_set`), or an island's (`def obj.f`).
    Other,
}

/// The names a reflective call can reach, as written in its first argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spelled {
    /// One name, `@` included.
    Exactly(String),
    /// Every name that starts with `head` and ends with `tail`: the literal parts around an
    /// interpolation. Both are empty for a name no literal spells, which matches every name.
    Like { head: String, tail: String },
    /// Whatever the callers of `method` pass as its positional argument `index`: the name is the
    /// enclosing method's own parameter. Only the callers can say; asked alone, every name.
    Argument { method: String, index: usize },
}

impl Spelled {
    /// The pattern every name fits.
    pub(super) fn everything() -> Self {
        Self::Like {
            head: String::new(),
            tail: String::new(),
        }
    }

    /// The pattern every writer's name fits: anything ending in `=`.
    pub(super) fn writer() -> Self {
        Self::Like {
            head: String::new(),
            tail: "=".to_owned(),
        }
    }

    /// Whether every name this spells is a writer's: a spelling or an interpolation's
    /// literal tail ending in `=`. A name nothing spells, or a parameter's, may be anything.
    #[must_use]
    pub fn names_a_writer(&self) -> bool {
        match self {
            Self::Exactly(exactly) => exactly.ends_with('='),
            Self::Like { tail, .. } => tail.ends_with('='),
            Self::Argument { .. } => false,
        }
    }

    /// Whether one name this spells can be a writer's: [`Self::names_a_writer`], or a name whose
    /// end nothing spells.
    #[must_use]
    pub fn may_name_a_writer(&self) -> bool {
        match self {
            Self::Like { tail, .. } if tail.is_empty() => true,
            Self::Argument { .. } => true,
            _ => self.names_a_writer(),
        }
    }

    /// Whether this can be the variable spelled `name`.
    #[must_use]
    pub fn matches(&self, name: &str) -> bool {
        match self {
            Self::Exactly(exactly) => exactly == name,
            Self::Like { head, tail } => {
                name.len() >= head.len() + tail.len()
                    && name.starts_with(head.as_str())
                    && name.ends_with(tail.as_str())
            }
            Self::Argument { .. } => true,
        }
    }
}

/// What the call whose name starts at `at` passes as its positional argument `index`, as the names
/// a reflective write given it could reach ([`Spelled`]).
///
/// `None` where no call's name starts there, the call passes fewer arguments, or a splat comes
/// first: the position cannot be read, so the caller must assume any name.
#[must_use]
pub fn argument_at(source: &str, at: u32, index: usize) -> Option<Spelled> {
    struct Find {
        at: u32,
        index: usize,
        found: Option<Option<Spelled>>,
    }
    impl<'pr> Visit<'pr> for Find {
        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            let starts = node
                .message_loc()
                .is_some_and(|message| message.start_offset() as u32 == self.at);
            // One call's name starts at any one offset, so the first match is the only one.
            if starts {
                let arguments: Vec<Node<'pr>> = node
                    .arguments()
                    .map(|found| found.arguments().iter().collect())
                    .unwrap_or_default();
                let splat = arguments
                    .iter()
                    .take(self.index + 1)
                    .any(|argument| argument.as_splat_node().is_some());
                self.found = Some(arguments.get(self.index).filter(|_| !splat).map(spelled_by));
            }
            ruby_prism::visit_call_node(self, node);
        }
    }
    let result = super::cursor::parse(source);
    let mut find = Find {
        at,
        index,
        found: None,
    };
    find.visit(&result.node());
    find.found.flatten()
}

/// Every variable in a text, each with the occurrences that are it.
///
/// [`variable`] asked of every occurrence at once, for the one caller that needs them all:
/// [`cursor`](super::cursor)'s reaching-writes table. For every read it asks which writes are the
/// *same* variable, and how far that variable's scope runs, which is where a block's later caller
/// can still reach it.
///
/// In order of each variable's first occurrence, so the answer does not depend on a hash.
#[must_use]
pub fn every_variable(source: &str) -> Vec<Group> {
    scoped(source).groups()
}

/// One variable, and every place it is written or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// As Ruby spells it, `@` included for an instance variable.
    pub name: String,
    /// The span of the scope a local lives in: its `def`, block, lambda or namespace body, or the
    /// whole file. `None` for an instance variable, which belongs to an object, not a scope.
    pub scope: Option<(u32, u32)>,
    /// For an instance variable, how many singleton steps its `self` is above an instance of the
    /// namespace it is written in: 0 in an instance method, 1 in a class body or `def self.`.
    ///
    /// `None` for a local, and for an instance variable whose `self` no namespace in the file names:
    /// the top level (`main`), and an island (`def obj.f`, `class << obj`).
    pub level: Option<i32>,
    /// The occurrences whose `self` is not known: those in a block or lambda written straight in
    /// a namespace body. `before_action { @story = … }` runs on an instance, `included do` on the
    /// class, and nothing in the block says which.
    pub loose: Vec<u32>,
    /// Every occurrence, in offset order.
    pub occurrences: Vec<Occurrence>,
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
    /// The span of every local scope, indexed by the number [`Variable::Local`] carries.
    spans: Vec<(u32, u32)>,
    /// Every occurrence whose `self` is not known ([`Group::loose`]), by where it starts.
    loose: std::collections::HashSet<u32>,
    /// Every declared setter ([`setters`]).
    setters: Vec<Accessor>,
    /// Every declared reader ([`readers`]).
    readers: Vec<Accessor>,
    /// Every reflective write ([`reflections`]).
    reflections: Vec<Reflection>,
    /// Every reflective class-variable write ([`class_variable_sets`]).
    class_reflections: Vec<Spelled>,
}

/// How many source texts the memo holds at once.
///
/// **Two: that is how many are in play at a time.**
/// - A request asks about the buffer under the cursor.
/// - The one caller with a second text is completion. Its receiver's variables are read from a
///   *repaired* copy of the buffer (the half-typed call removed, `cursor::Cursor::repaired`), while
///   the other surfaces read the buffer itself.
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
    scoped_in(&cursor::Parsed::new(source))
}

/// [`scoped`] for a text a request may already have parsed for another question
/// ([`cursor::Parsed`]): a held walk asks for no parse, and a walk made here parses once for both.
fn scoped_in(text: &cursor::Parsed<'_>) -> Rc<Scoped> {
    let source = text.source();
    held(source).unwrap_or_else(|| hold(source, Scoped::of(source, &text.result().node())))
}

/// Walk `source` from a tree a caller has already parsed, and hold the walk, so the next question
/// about the same text does not parse it again. [`cursor::shapes`](super::cursor::shapes) hands
/// its tree over, since it asks this module about the same text a moment later.
pub(super) fn seed(source: &str, node: &Node<'_>) {
    if held(source).is_none() {
        hold(source, Scoped::of(source, node));
    }
}

/// The walk of `source`, where the memo holds one.
///
/// Looked up and released before any walk, not held across it: [`Scoped::of`] runs with no borrow
/// of `WALKED` outstanding, so a future question that reached back in here could not panic on the
/// `RefCell`.
fn held(source: &str) -> Option<Rc<Scoped>> {
    WALKED.with_borrow(|walked| {
        walked
            .iter()
            .find(|(text, _)| &**text == source)
            .map(|(_, scoped)| Rc::clone(scoped))
    })
}

/// Keep `walked` as the walk of `source`, dropping the oldest where the memo is full.
fn hold(source: &str, walked: Scoped) -> Rc<Scoped> {
    let fresh = Rc::new(walked);
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
    /// Walk and sort one parsed text: the work the memo exists to do once.
    fn of(source: &str, node: &Node<'_>) -> Self {
        let mut walk = Walk::new(source);
        walk.visit(node);
        walk.found.sort_by_key(|(_, occurrence)| occurrence.start);
        // A local names every value any of its writes gives it; one with none this walk can read
        // (a block's parameter, a `rescue` binding) names every variable.
        for (index, variable) in std::mem::take(&mut walk.named_by) {
            walk.reflections[index].names = walk
                .values
                .get(&variable)
                .filter(|values| !values.is_empty())
                .cloned()
                .unwrap_or_else(|| vec![Spelled::everything()]);
        }
        Self {
            found: walk.found,
            sites: walk.sites,
            spans: walk.spans,
            loose: walk.loose_found,
            setters: walk.setters,
            readers: walk.readers,
            reflections: walk.reflections,
            class_reflections: walk.class_reflections,
        }
    }

    /// Every variable with its occurrences, in order of first occurrence.
    fn groups(&self) -> Vec<Group> {
        let mut index: HashMap<&Variable, usize> = HashMap::new();
        let mut groups: Vec<Group> = Vec::new();
        for (variable, occurrence) in &self.found {
            let at = *index.entry(variable).or_insert_with(|| {
                let (name, scope, level) = match variable {
                    Variable::Local { name, scope } => {
                        (name.clone(), Some(self.spans[*scope as usize]), None)
                    }
                    Variable::Instance { name, owner } => {
                        (name.clone(), None, owner.named().then_some(owner.level))
                    }
                };
                groups.push(Group {
                    name,
                    scope,
                    level,
                    loose: Vec::new(),
                    occurrences: Vec::new(),
                });
                groups.len() - 1
            });
            if self.loose.contains(&occurrence.start) {
                groups[at].loose.push(occurrence.start);
            }
            groups[at].occurrences.push(occurrence.clone());
        }
        groups
    }

    /// The name of the variable the cursor is on, and the occurrences sharing it.
    fn under(&self, offset: u32) -> Option<(String, Occurrence, Vec<Occurrence>)> {
        let (variable, under) = self
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
            under.clone(),
            self.found
                .iter()
                .filter(|(candidate, _)| candidate == variable)
                .map(|(_, at)| at.clone())
                .collect(),
        ))
    }

    /// See [`instance_family`].
    fn family(&self, offset: u32) -> Option<(String, usize, Vec<Placed>)> {
        let (variable, at) = self
            .found
            .iter()
            .find(|(_, at)| at.start <= offset && offset <= at.end)?;
        let Variable::Instance { name, owner } = variable else {
            return None;
        };
        if !owner.named() {
            return None;
        }
        let family: Vec<Placed> = self
            .found
            .iter()
            .filter_map(|(candidate, occurrence)| match candidate {
                Variable::Instance {
                    name: spelled,
                    owner: other,
                } if spelled == name && other.path == owner.path => Some(Placed {
                    occurrence: occurrence.clone(),
                    level: other.level,
                    loose: self.loose.contains(&occurrence.start),
                }),
                _ => None,
            })
            .collect();
        // The family is in offset order and holds the cursor's own occurrence.
        let cursor = family
            .iter()
            .take_while(|placed| placed.occurrence.start < at.start)
            .count();
        Some((name.clone(), cursor, family))
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SelfContext {
    /// The lexical namespace path as written, so a class reopened in the same file under the same
    /// spelling keeps its instance variables together.
    path: String,
    level: i32,
}

impl SelfContext {
    /// Whether a namespace this file writes is what `self` hangs off: not the top level, and not
    /// an island, whose path carries the `<offset>` [`Walk::island`] names it by.
    fn named(&self) -> bool {
        !self.path.is_empty() && !self.path.contains('<')
    }
}

/// The identity two occurrences must share to be the same variable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// Where every local scope begins and ends, indexed by its number. The file's own is first.
    spans: Vec<(u32, u32)>,
    /// Whether `self` was last set by a namespace body, not a `def`: where a block's `self` stops
    /// being known.
    in_body: bool,
    /// Whether the walk is inside a block or lambda written straight in a namespace body.
    loose: bool,
    /// Every instance-variable occurrence found while [`Self::loose`].
    loose_found: std::collections::HashSet<u32>,
    /// Every declared setter.
    setters: Vec<Accessor>,
    /// Every declared reader, outside a loose block.
    readers: Vec<Accessor>,
    /// Every reflective write.
    reflections: Vec<Reflection>,
    /// Every `class_variable_set`'s name.
    class_reflections: Vec<Spelled>,
    /// What every local can hold, for a reflective write that names its variable with one: each
    /// write's value as a [`Spelled`], or the argument a method's parameter is.
    values: HashMap<Variable, Vec<Spelled>>,
    /// The reflective writes whose name is a local, by index into `reflections`: resolved once
    /// every write of the local is known.
    named_by: Vec<(usize, Variable)>,
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
            spans: vec![(0, source.len() as u32)],
            in_body: true,
            loose: false,
            loose_found: std::collections::HashSet::new(),
            setters: Vec::new(),
            readers: Vec::new(),
            reflections: Vec::new(),
            class_reflections: Vec::new(),
            values: HashMap::new(),
            named_by: Vec::new(),
        }
    }

    /// Run `body` inside a freshly numbered local scope, written over `at`.
    fn scoped(&mut self, at: &Location<'_>, body: impl FnOnce(&mut Self)) {
        self.scopes.push(self.next_scope);
        self.next_scope += 1;
        self.spans
            .push((at.start_offset() as u32, at.end_offset() as u32));
        body(self);
        self.scopes.pop();
    }

    /// Run `run` with `self` bound to `this`, and `site` as where an accessor for an instance of it
    /// would be declared.
    ///
    /// The two travel together because they are decided together. Every construct that changes
    /// `self` either opens a namespace body, keeps the one around it, or is an island with no body.
    /// Separating them is how one of the three would get missed.
    ///
    /// `body` says whether the construct is a namespace body (`true`) or a `def` (`false`), which
    /// decides whether a block inside it knows its `self` ([`Group::loose`]).
    fn as_self(
        &mut self,
        this: SelfContext,
        site: Option<u32>,
        body: bool,
        run: impl FnOnce(&mut Self),
    ) {
        let outer = std::mem::replace(&mut self.this, this);
        let outside = std::mem::replace(&mut self.body, site);
        let was_body = std::mem::replace(&mut self.in_body, body);
        let was_loose = std::mem::replace(&mut self.loose, false);
        run(self);
        self.this = outer;
        self.body = outside;
        self.in_body = was_body;
        self.loose = was_loose;
    }

    /// Run `run` inside a block or lambda: its `self` is unknown when it is written straight in a
    /// namespace body, since a DSL may run it on an instance or on the class.
    fn closure(&mut self, run: impl FnOnce(&mut Self)) {
        let was_loose = self.loose;
        self.loose |= self.in_body && self.this.named();
        run(self);
        self.loose = was_loose;
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

    /// Note what a write gives a local, for [`Walk::values`].
    fn holds(&mut self, name: &ConstantId<'_>, depth: u32, value: Spelled) {
        let variable = Variable::Local {
            name: spelled(name),
            scope: self.scope_at(depth),
        };
        self.values.entry(variable).or_default().push(value);
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
        if self.loose {
            self.loose_found.insert(at.start_offset() as u32);
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
        self.as_self(this, opens(node.body().as_ref()), true, |walk| {
            walk.scoped(&node.location(), |walk| {
                if let Some(body) = node.body() {
                    walk.visit(&body);
                }
            });
        });
    }

    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        let this = self.namespace(&node.constant_path());
        self.as_self(this, opens(node.body().as_ref()), true, |walk| {
            walk.scoped(&node.location(), |walk| {
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
        self.as_self(this, site, true, |walk| {
            walk.scoped(&node.location(), |walk| {
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
        let method = String::from_utf8_lossy(node.name().as_slice()).into_owned();
        self.as_self(this, site, false, |walk| {
            walk.scoped(&node.location(), |walk| {
                if let Some(parameters) = node.parameters() {
                    // A positional parameter holds what the callers pass at its position.
                    let positional = parameters
                        .requireds()
                        .iter()
                        .chain(parameters.optionals().iter());
                    for (index, parameter) in positional.enumerate() {
                        let name = parameter
                            .as_required_parameter_node()
                            .map(|found| found.name())
                            .or_else(|| {
                                parameter
                                    .as_optional_parameter_node()
                                    .map(|found| found.name())
                            });
                        if let Some(name) = name {
                            walk.holds(
                                &name,
                                0,
                                Spelled::Argument {
                                    method: method.clone(),
                                    index,
                                },
                            );
                        }
                    }
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

    //
    // **Except straight in a namespace body**, where a DSL decides: `before_action { }` runs on an
    // instance and `included do` on the class. The variable is still grouped by the lexical
    // `self`, and [`Group::loose`] says the grouping is a guess.

    fn visit_block_node(&mut self, node: &BlockNode<'pr>) {
        self.closure(|walk| {
            walk.scoped(&node.location(), |walk| {
                ruby_prism::visit_block_node(walk, node)
            });
        });
    }

    fn visit_lambda_node(&mut self, node: &LambdaNode<'pr>) {
        self.closure(|walk| {
            walk.scoped(&node.location(), |walk| {
                ruby_prism::visit_lambda_node(walk, node)
            });
        });
    }

    // An accessor declared on `self`: `attr_writer :name` writes an instance's `@name`, and
    // `attr_reader :name` reads it.

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        let name = node.name();
        let declares = matches!(
            name.as_slice(),
            b"attr_reader" | b"attr_writer" | b"attr_accessor"
        ) && node
            .receiver()
            .is_none_or(|receiver| matches!(receiver, Node::SelfNode { .. }));
        if declares && self.this.named() {
            let writes = name.as_slice() != b"attr_reader";
            let reads = name.as_slice() != b"attr_writer" && !self.loose;
            let arguments = node.arguments();
            for argument in arguments.iter().flat_map(|found| found.arguments().iter()) {
                // The name's own start, inside the colon or the quotes: where rubydex files the
                // method the accessor declares.
                let start = argument.location().start_offset();
                let spelled = argument
                    .as_symbol_node()
                    .map(|symbol| {
                        let at = symbol
                            .value_loc()
                            .map_or(start, |value| value.start_offset());
                        (symbol.unescaped().to_vec(), at)
                    })
                    .or_else(|| {
                        argument.as_string_node().map(|string| {
                            (
                                string.unescaped().to_vec(),
                                string.content_loc().start_offset(),
                            )
                        })
                    });
                if let Some((bytes, at)) = spelled {
                    let declared = Accessor {
                        name: format!("@{}", String::from_utf8_lossy(&bytes)),
                        level: self.this.level - 1,
                        at: at as u32,
                    };
                    if reads {
                        self.readers.push(declared.clone());
                    }
                    if writes {
                        self.setters.push(declared);
                    }
                }
            }
        }
        if name.as_slice() == b"class_variable_set" {
            // Only a literal says which: a local is not followed here, so it names every variable.
            let first = node
                .arguments()
                .and_then(|arguments| arguments.arguments().iter().next());
            self.class_reflections
                .push(first.map_or_else(Spelled::everything, |first| spelled_by(&first)));
        }
        let removes = name.as_slice() == REFLECTIVE_WRITERS[1].as_bytes();
        if removes || name.as_slice() == REFLECTIVE_WRITERS[0].as_bytes() {
            let first = node
                .arguments()
                .and_then(|arguments| arguments.arguments().iter().next());
            if let Some(first) = first {
                let on_self = node
                    .receiver()
                    .is_none_or(|receiver| matches!(receiver, Node::SelfNode { .. }));
                let on = if !on_self || self.this.path.contains('<') {
                    Reflected::Other
                } else if self.this.path.is_empty() {
                    Reflected::Main
                } else if self.loose {
                    Reflected::Own(None)
                } else {
                    Reflected::Own(Some(self.this.level))
                };
                if let Some(local) = first.as_local_variable_read_node() {
                    let variable = Variable::Local {
                        name: spelled(&local.name()),
                        scope: self.scope_at(local.depth()),
                    };
                    self.named_by.push((self.reflections.len(), variable));
                }
                self.reflections.push(Reflection {
                    names: vec![spelled_by(&first)],
                    on,
                    removes,
                    at: node
                        .message_loc()
                        .map_or(node.location().start_offset(), |at| at.start_offset())
                        as u32,
                });
            }
        }
        ruby_prism::visit_call_node(self, node);
    }

    // Locals: every one of these carries the depth that says which scope it belongs to.

    fn visit_local_variable_read_node(&mut self, node: &LocalVariableReadNode<'pr>) {
        self.local(&node.name(), node.depth(), &node.location(), false);
    }

    fn visit_local_variable_write_node(&mut self, node: &LocalVariableWriteNode<'pr>) {
        self.local(&node.name(), node.depth(), &node.name_loc(), true);
        self.holds(&node.name(), node.depth(), spelled_by(&node.value()));
        ruby_prism::visit_local_variable_write_node(self, node);
    }

    fn visit_local_variable_target_node(&mut self, node: &LocalVariableTargetNode<'pr>) {
        self.local(&node.name(), node.depth(), &node.location(), true);
        self.holds(&node.name(), node.depth(), Spelled::everything());
    }

    fn visit_local_variable_and_write_node(&mut self, node: &LocalVariableAndWriteNode<'pr>) {
        self.local(&node.name(), node.depth(), &node.name_loc(), true);
        self.holds(&node.name(), node.depth(), spelled_by(&node.value()));
        ruby_prism::visit_local_variable_and_write_node(self, node);
    }

    fn visit_local_variable_or_write_node(&mut self, node: &LocalVariableOrWriteNode<'pr>) {
        self.local(&node.name(), node.depth(), &node.name_loc(), true);
        self.holds(&node.name(), node.depth(), spelled_by(&node.value()));
        ruby_prism::visit_local_variable_or_write_node(self, node);
    }

    fn visit_local_variable_operator_write_node(
        &mut self,
        node: &LocalVariableOperatorWriteNode<'pr>,
    ) {
        self.local(&node.name(), node.depth(), &node.name_loc(), true);
        self.holds(&node.name(), node.depth(), Spelled::everything());
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
/// The names a reflective call's first argument can spell ([`Spelled`]).
pub(super) fn spelled_by(argument: &Node<'_>) -> Spelled {
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    if let Some(symbol) = argument.as_symbol_node() {
        return Spelled::Exactly(text(symbol.unescaped()));
    }
    if let Some(string) = argument.as_string_node() {
        return Spelled::Exactly(text(string.unescaped()));
    }
    let parts: Vec<Node<'_>> = if let Some(symbol) = argument.as_interpolated_symbol_node() {
        symbol.parts().iter().collect()
    } else if let Some(string) = argument.as_interpolated_string_node() {
        string.parts().iter().collect()
    } else {
        Vec::new()
    };
    let literal = |part: &Node<'_>| part.as_string_node().map(|found| text(found.unescaped()));
    let head: String = parts.iter().map_while(literal).collect();
    let tail: Vec<String> = parts.iter().rev().map_while(literal).collect();
    Spelled::Like {
        head,
        tail: tail.into_iter().rev().collect(),
    }
}

fn spelled(name: &ConstantId<'_>) -> String {
    String::from_utf8_lossy(name.as_slice()).into_owned()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// Which spellings name only writers, and which can name one.
    #[test]
    fn a_writer_s_name_ends_in_an_equals_sign() {
        let exactly = |name: &str| Spelled::Exactly(name.to_owned());
        let like = |head: &str, tail: &str| Spelled::Like {
            head: head.to_owned(),
            tail: tail.to_owned(),
        };
        let argument = Spelled::Argument {
            method: "set".to_owned(),
            index: 0,
        };
        assert!(exactly("name=").names_a_writer());
        assert!(!exactly("name").names_a_writer());
        assert!(Spelled::writer().names_a_writer());
        assert!(!Spelled::everything().names_a_writer());
        assert!(!argument.names_a_writer());

        assert!(Spelled::everything().may_name_a_writer());
        assert!(like("set_", "").may_name_a_writer());
        assert!(argument.may_name_a_writer());
        assert!(like("", "=").may_name_a_writer());
        assert!(!like("", "_id").may_name_a_writer());
        assert!(!exactly("name").may_name_a_writer());
        assert!(Spelled::writer().matches("title="));
    }

    /// Every variable at once, grouped as [`variable`] groups one, with the scope a local lives in.
    ///
    /// The scope is the construct's whole span: a `def`'s includes its parameters, a block's its
    /// `|b|`. The file is a scope too. An instance variable has none, because it belongs to an object.
    #[test]
    fn every_variable_is_grouped_with_the_scope_it_lives_in() {
        let source =
            "x = 1\ndef f(a)\n  a\n  [1].each { |b| b; a }\nend\nclass K\n  def g = @v\nend\n";
        let at = |text: &str| source.find(text).expect("in the fixture") as u32;
        let def = (at("def f"), at("end\nclass") + 3);
        let block = (at("{ |b|"), at("}") + 1);
        type Summary = (String, Option<(u32, u32)>, Vec<u32>);
        let groups: Vec<Summary> = every_variable(source)
            .into_iter()
            .map(|group| {
                let starts = group.occurrences.iter().map(|it| it.start).collect();
                (group.name, group.scope, starts)
            })
            .collect();
        assert_eq!(
            groups,
            vec![
                ("x".to_owned(), Some((0, source.len() as u32)), vec![0]),
                (
                    "a".to_owned(),
                    Some(def),
                    vec![at("a)"), at("a\n"), at("a }")]
                ),
                ("b".to_owned(), Some(block), vec![at("b|"), at("b;")]),
                ("@v".to_owned(), None, vec![at("@v")]),
            ]
        );
    }

    #[test]
    fn an_instance_variable_knows_how_far_above_an_instance_its_self_is() {
        // 0 in an instance method, 1 on the class object, and nothing at the top level or on an
        // island, where no namespace in the file names the object.
        let source = "\
class K
  @body = 1
  def a = @instance
  def self.b = @class_side
  class << self
    def c = @also_class_side
  end
  def obj.d = @island
end
@top = 1
";
        let levels: Vec<(String, Option<i32>)> = every_variable(source)
            .into_iter()
            .map(|group| (group.name, group.level))
            .collect();
        assert_eq!(
            levels,
            vec![
                ("@body".to_owned(), Some(1)),
                ("@instance".to_owned(), Some(0)),
                ("@class_side".to_owned(), Some(1)),
                ("@also_class_side".to_owned(), Some(1)),
                ("@island".to_owned(), None),
                ("@top".to_owned(), None),
            ]
        );
        // A local has no `self` to be above.
        assert!(
            every_variable("x = 1\n")
                .iter()
                .all(|group| group.level.is_none())
        );
    }

    #[test]
    fn a_block_straight_in_a_namespace_body_does_not_know_its_self() {
        // `before_action { }` and `-> { }` in a class body run wherever a DSL runs them. Inside a
        // `def`, a block keeps the method's `self`; a `def` inside the loose block knows its own.
        let source = "\
class K
  before_action { @loose = 1 }
  scope :x, -> { @lambda }
  included do
    def m = @known
  end
  def n
    [1].each { @kept }
  end
end
[1].each { @top }
";
        let at = |text: &str| source.find(text).expect("in the fixture") as u32;
        let loose: Vec<(String, Vec<u32>)> = every_variable(source)
            .into_iter()
            .map(|group| (group.name, group.loose))
            .collect();
        assert_eq!(
            loose,
            vec![
                ("@loose".to_owned(), vec![at("@loose")]),
                ("@lambda".to_owned(), vec![at("@lambda")]),
                ("@known".to_owned(), Vec::new()),
                ("@kept".to_owned(), Vec::new()),
                ("@top".to_owned(), Vec::new()),
            ]
        );
    }

    #[test]
    fn a_setter_declared_on_self_writes_the_instance_variable_it_names() {
        let source = "\
class K
  attr_accessor :name, \"title\"
  self.attr_writer :email
  attr_reader :read_only
  other.attr_writer :elsewhere
  attr_writer NAMES
  class << self
    attr_accessor :config
  end
end
attr_writer :top
";
        // The name's own start, one past the colon or the quote.
        let at = |text: &str| source.find(text).expect("in the fixture") as u32 + 1;
        assert_eq!(
            setters(source),
            vec![
                Accessor {
                    name: "@name".to_owned(),
                    level: 0,
                    at: at(":name"),
                },
                Accessor {
                    name: "@title".to_owned(),
                    level: 0,
                    at: at("\"title\""),
                },
                Accessor {
                    name: "@email".to_owned(),
                    level: 0,
                    at: at(":email"),
                },
                // `class << self` declares on the class object.
                Accessor {
                    name: "@config".to_owned(),
                    level: 1,
                    at: at(":config"),
                },
            ]
        );
    }

    #[test]
    fn a_reader_declared_on_self_reads_the_instance_variable_it_names() {
        // The same visit as the setters: `attr_accessor` is both, `attr_writer` is neither, and
        // `class << self` reads the class object's variable. A reader in a block straight in a
        // namespace body is left out: `included do` and `class_methods do` run on different
        // objects, and the block does not say which. Its setter is still a write.
        let source = "\
class K
  attr_reader :story, \"title\"
  self.attr_accessor :both
  attr_writer :written
  other.attr_reader :elsewhere
  class << self
    attr_reader :config
  end
end

module Concern
  included do
    attr_reader :loose
    attr_accessor :loose_both
  end
end
attr_reader :top
";
        // The name's own start, one past the colon or the quote.
        let at = |text: &str| source.find(text).expect("in the fixture") as u32 + 1;
        let accessor = |name: &str, level: i32, text: &str| Accessor {
            name: name.to_owned(),
            level,
            at: at(text),
        };
        assert_eq!(
            readers(source),
            vec![
                accessor("@story", 0, ":story"),
                accessor("@title", 0, "\"title\""),
                accessor("@both", 0, ":both"),
                accessor("@config", 1, ":config"),
            ]
        );
        assert_eq!(
            setters(source),
            vec![
                accessor("@both", 0, ":both"),
                accessor("@written", 0, ":written"),
                accessor("@loose_both", 0, ":loose_both"),
            ]
        );
    }

    #[test]
    fn a_reflective_name_held_by_a_local_is_every_value_the_local_can_hold() {
        let source = "\
class K
  def a(model, *rest)
    var = :@one
    var = \"@two_#{model}\" if model
    model.instance_variable_set(var, 1)
    rest.each { |name| model.instance_variable_set(name, 1) }
    other ||= compute
    model.instance_variable_set(other, 1)
    (x, y = 1, 2)
    model.instance_variable_set(x, 1)
    z = 1
    z += 1
    model.instance_variable_set(z, 1)
  end
end
";
        let names: Vec<Vec<Spelled>> = reflections(source)
            .into_iter()
            .map(|reflection| reflection.names)
            .collect();
        let everything = || vec![Spelled::everything()];
        assert_eq!(
            names,
            vec![
                vec![
                    Spelled::Exactly("@one".to_owned()),
                    Spelled::Like {
                        head: "@two_".to_owned(),
                        tail: String::new(),
                    },
                ],
                // A block's parameter holds nothing this walk can read.
                everything(),
                everything(),
                everything(),
                // A value that is not a literal name, and an operator write: any name.
                vec![Spelled::everything(), Spelled::everything()],
            ]
        );
    }

    #[test]
    fn a_call_s_argument_is_read_where_the_call_s_name_starts() {
        let source = "fill(record, :@tags)\nfill(*all, :@tags)\nfill(record)\nother\n";
        let at = |text: &str| source.find(text).expect("in the fixture") as u32;
        assert_eq!(
            argument_at(source, at("fill(record, :@tags)"), 1),
            Some(Spelled::Exactly("@tags".to_owned()))
        );
        // A splat before it, too few arguments, and no call there: nothing can be read.
        assert_eq!(argument_at(source, at("fill(*all"), 1), None);
        assert_eq!(argument_at(source, at("fill(record)\n"), 1), None);
        assert_eq!(argument_at(source, at("other"), 0), None);
    }

    #[test]
    fn a_reflective_write_knows_the_names_it_can_reach_and_whose_they_are() {
        let source = "\
class K
  def a(v, key, name)
    instance_variable_set(:@x, v)
    self.instance_variable_set(\"@y\", v)
    instance_variable_set(:\"@#{key}_cache\", v)
    instance_variable_set(\"@pre_#{key}\", v)
    instance_variable_set(name, v)
    other.instance_variable_set(:@z, v)
    remove_instance_variable(:@x)
    instance_variable_set
  end
  included { instance_variable_set(:@loose, 1) }
  def obj.f = instance_variable_set(:@island, 1)
end
instance_variable_set(:@top, 1)
";
        let at = |text: &str| source.find(text).expect("in the fixture") as u32;
        let like = |head: &str, tail: &str| Spelled::Like {
            head: head.to_owned(),
            tail: tail.to_owned(),
        };
        let exactly = |name: &str| Spelled::Exactly(name.to_owned());
        let found: Vec<(Spelled, Reflected, bool)> = reflections(source)
            .into_iter()
            .map(|reflection| {
                (
                    reflection.names[0].clone(),
                    reflection.on,
                    reflection.removes,
                )
            })
            .collect();
        assert_eq!(
            found,
            vec![
                (exactly("@x"), Reflected::Own(Some(0)), false),
                (exactly("@y"), Reflected::Own(Some(0)), false),
                (like("@", "_cache"), Reflected::Own(Some(0)), false),
                (like("@pre_", ""), Reflected::Own(Some(0)), false),
                (
                    Spelled::Argument {
                        method: "a".to_owned(),
                        index: 2,
                    },
                    Reflected::Own(Some(0)),
                    false,
                ),
                (exactly("@z"), Reflected::Other, false),
                (exactly("@x"), Reflected::Own(Some(0)), true),
                (exactly("@loose"), Reflected::Own(None), false),
                (exactly("@island"), Reflected::Other, false),
                (exactly("@top"), Reflected::Main, false),
            ]
        );
        assert_eq!(reflections(source)[0].at, at("instance_variable_set(:@x"));

        // A pattern matches what its literal parts allow, and no shorter name.
        assert!(like("@", "_cache").matches("@warm_cache"));
        assert!(!like("@", "_cache").matches("@count"));
        assert!(!like("@warm", "_cache").matches("@warm"));
        assert!(like("", "").matches("@anything"));
        assert!(!like("@warm", "_cache").matches("@cold_cache"));
        // A parameter's name, asked alone, could be any name: only its callers narrow it.
        assert!(
            Spelled::Argument {
                method: "fill".to_owned(),
                index: 1,
            }
            .matches("@anything")
        );
        assert!(exactly("@x").matches("@x") && !exactly("@x").matches("@y"));
    }

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

        let cold = variable(&cursor::Parsed::new(a), at(a, "@v"));
        assert!(
            cold.is_some(),
            "the fixture has a variable under the cursor"
        );
        assert_eq!(walked(), vec![a.to_owned()]);

        // The same text again is answered from the memo: nothing added, and the answer is the
        // walk's.
        assert_eq!(variable(&cursor::Parsed::new(a), at(a, "@v")), cold);
        assert_eq!(walked(), vec![a.to_owned()]);

        // A second text joins it instead of replacing it: the reason for two slots.
        // Completion reads a repaired copy of the buffer while every other surface reads the
        // buffer, and one slot would hold neither.
        assert_eq!(
            writes_to(b, "B", "@w").len(),
            1,
            "the fixture writes @w once"
        );
        assert_eq!(walked(), vec![b.to_owned(), a.to_owned()]);
        assert_eq!(
            variable(&cursor::Parsed::new(a), at(a, "@v")),
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
            variable(&cursor::Parsed::new(source), offset),
            writes_to(source, "Story", "@views"),
            crossing(source, range.0, range.1),
            accessor_site(source, offset),
        );
        // Cold again, so two walks are compared instead of a walk with itself.
        forget();
        assert_eq!(variable(&cursor::Parsed::new(source), offset), cold.0);
        // Now warm: the three below are answered off the walk the line above stored.
        assert_eq!(writes_to(source, "Story", "@views"), cold.1);
        assert_eq!(crossing(source, range.0, range.1), cold.2);
        assert_eq!(accessor_site(source, offset), cold.3);
        assert_eq!(walked().len(), 1, "one text, one walk");
    }

    #[test]
    fn an_instance_variable_s_family_is_its_name_in_its_own_namespace_at_every_level() {
        // What a caller regroups a loose occurrence within: the cursor's name in the
        // cursor's namespace, at any level, each marked loose or not. The same name in another
        // class is another object's, and another name is another variable.
        let source = "\
class Widget
  hook { @v }
  def a = @v
  def self.b = @v
  def c = @w
end

class Other
  def a = @v
end
";
        let offset = source.find("@v }").expect("the loose read") as u32;
        let (name, cursor, family) =
            instance_family(&cursor::Parsed::new(source), offset).expect("a family");
        assert_eq!(name, "@v");
        let placed: Vec<(i32, bool)> = family
            .iter()
            .map(|placed| (placed.level, placed.loose))
            .collect();
        assert_eq!(placed, [(1, true), (0, false), (1, false)]);
        assert_eq!(family[cursor].occurrence.start, offset);
        // A local, and the top level's variable, have no namespace to be a family in.
        assert_eq!(instance_family(&cursor::Parsed::new("x = 1\nx\n"), 6), None);
        assert_eq!(instance_family(&cursor::Parsed::new("@top = 1\n"), 1), None);
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
