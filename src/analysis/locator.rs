//! What the cursor is on, which declaration that is, and where the declaration lives.
//!
//! Everything navigational goes through here: `hover`, `definition`, and (later) `references`
//! all ask the same three questions in the same order, and answering them once keeps their
//! answers consistent.
//!
//! # Why "narrowest span wins"
//!
//! rubydex records a document's definitions, constant references, and method references
//! independently, and their spans nest freely: the cursor on `name` inside `Person#shout` sits
//! inside a method reference (4 bytes), the `shout` definition (70 bytes), and the `Person`
//! definition (394 bytes) all at once. The innermost one is what the user pointed at.

use std::path::PathBuf;

use rubydex::{
    model::{
        declaration::{Declaration, Namespace},
        definitions::Definition,
        graph::Graph,
        ids::{DeclarationId, StringId, UriId},
        references::{ConstantReference, MethodRef},
    },
    offset::Offset,
    query::{self, FindMemberError, MatchMode},
};

/// The thing the cursor is on.
#[derive(Debug, Clone, Copy)]
pub enum Target<'g> {
    /// A use of a constant: `Person`, `Person::MAX_AGE`.
    Constant(&'g ConstantReference),
    /// A call: `person.shout`, `Person.build`, a bare `name`.
    Call(&'g MethodRef),
    /// The definition itself: the name in `class Person` or `def shout`.
    Definition(&'g Definition),
}

/// A [`Target`] plus the span the cursor actually landed in.
///
/// The span is what an editor underlines for a definition link, so it has to be the span of
/// the *reference* — not of whatever it resolves to.
#[derive(Debug, Clone, Copy)]
pub struct Located<'g> {
    pub start: u32,
    pub end: u32,
    pub target: Target<'g>,
}

/// Where a declaration is written down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// The graph's own spelling of the document URI.
    pub uri: String,
    /// The whole construct: `class Person ... end`.
    pub full: (u32, u32),
    /// Just the name, for the editor to highlight. Falls back to `full` for the definition
    /// kinds rubydex does not record a name span for (constants, `attr_*`, aliases).
    pub selection: (u32, u32),
}

/// Which declarations a target names, and how sure we are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub declarations: Vec<DeclarationId>,
    /// `false` when the receiver's type was unknown and the candidates came from matching the
    /// method name alone. Without type inference `person.shout` can only ever be a guess, and
    /// callers must present it as one.
    pub precise: bool,
    /// `true` when these declarations are not what the name under the cursor declares, but what
    /// the user meant by writing it — `Foo.new` answered with `Foo#initialize`.
    ///
    /// Navigation wants the redirect; `references` must not, because `def initialize` is not a
    /// declaration of `new` and listing it in a work list of `.new` call sites is noise.
    pub redirected: bool,
}

impl Resolution {
    fn precise(declarations: Vec<DeclarationId>) -> Self {
        Self {
            declarations,
            precise: true,
            redirected: false,
        }
    }

    fn redirected(declaration: DeclarationId) -> Self {
        Self {
            declarations: vec![declaration],
            precise: true,
            redirected: true,
        }
    }
}

/// Everything the cursor could be pointing at, narrowed to the tightest span.
///
/// More than one target can share that span — `Person.new` records both the `Person` reference
/// and a synthetic reference to its singleton class over the same bytes — so this returns all
/// of them and lets the caller take the first that resolves to something with a location.
#[must_use]
pub fn locate(graph: &Graph, uri_id: UriId, offset: u32) -> Vec<Located<'_>> {
    let Some(document) = graph.documents().get(&uri_id) else {
        return Vec::new();
    };

    let mut found: Vec<Located<'_>> = Vec::new();

    // `filter_map` rather than a `let ... else { continue }` per loop, matching
    // `definitions_of` below: a document lists ids the graph itself filed, so the miss is not a
    // case to handle here, it is a lookup that yields nothing.
    for reference in document
        .constant_references()
        .iter()
        .filter_map(|id| graph.constant_references().get(id))
    {
        if !covers(reference.offset(), offset) || is_synthetic(graph, reference) {
            continue;
        }
        found.push(at(reference.offset(), Target::Constant(reference)));
    }

    for reference in document
        .method_references()
        .iter()
        .filter_map(|id| graph.method_references().get(id))
    {
        if covers(reference.offset(), offset) {
            found.push(at(reference.offset(), Target::Call(reference)));
        }
    }

    for definition in document
        .definitions()
        .iter()
        .filter_map(|id| graph.definitions().get(id))
    {
        // Match the name, not the body: otherwise every click inside a method body would
        // "be on" the method, and hover would fire over whitespace.
        let span = definition
            .name_offset()
            .unwrap_or_else(|| definition.offset());
        if covers(span, offset) {
            found.push(at(span, Target::Definition(definition)));
        }
    }

    let Some(narrowest) = found.iter().map(Located::width).min() else {
        return Vec::new();
    };
    found.retain(|located| located.width() == narrowest);
    found
}

/// The declarations a target names.
#[must_use]
pub fn resolve(graph: &Graph, located: &Located<'_>) -> Resolution {
    match located.target {
        Target::Constant(reference) => Resolution::precise(
            graph
                .name_id_to_declaration_id(*reference.name_id())
                .copied()
                .into_iter()
                .collect(),
        ),
        Target::Call(reference) => resolve_call(graph, reference),
        Target::Definition(definition) => Resolution::precise(
            graph
                .definition_to_declaration_id(definition)
                .copied()
                .into_iter()
                .collect(),
        ),
    }
}

/// Every definition of a declaration, in a stable order.
///
/// The order matters twice over: goto-definition jumps to the first entry, and hover reads the
/// first entry's comments. `Declaration::definitions` is filled as documents are indexed in
/// parallel, so without sorting both would change from run to run.
#[must_use]
pub fn definitions_of(graph: &Graph, declaration_id: DeclarationId) -> Vec<&Definition> {
    let Some(declaration) = graph.declarations().get(&declaration_id) else {
        return Vec::new();
    };

    let mut definitions: Vec<&Definition> = declaration
        .definitions()
        .iter()
        .filter_map(|id| graph.definitions().get(id))
        .collect();

    definitions.sort_by_key(|definition| {
        let uri = graph
            .documents()
            .get(definition.uri_id())
            .map_or("", rubydex::model::document::Document::uri);
        (uri, definition.offset().start(), definition.offset().end())
    });
    definitions
}

/// Every place a declaration is written, in the same order as [`definitions_of`].
#[must_use]
pub fn sites(graph: &Graph, declaration_id: DeclarationId) -> Vec<Site> {
    let mut sites: Vec<Site> = definitions_of(graph, declaration_id)
        .into_iter()
        .filter_map(|definition| site(graph, definition))
        .collect();
    sites.dedup();
    sites
}

/// Where a single definition is written.
#[must_use]
pub fn site(graph: &Graph, definition: &Definition) -> Option<Site> {
    let document = graph.documents().get(definition.uri_id())?;
    let (full, selection) = spans(definition);
    Some(Site {
        uri: document.uri().to_owned(),
        full,
        selection,
    })
}

/// The two spans a definition contributes to a response: the whole construct, and the name
/// inside it.
///
/// **The protocol requires the second to be contained in the first**, in both places one is
/// sent: `DocumentSymbol::selectionRange` and `LocationLink::targetSelectionRange`. VS Code
/// enforces the first by *throwing* — `selectionRange must be contained in fullRange` — which
/// discards the entire outline rather than the one symbol that broke the rule.
///
/// Prism's error recovery produces pairs that are not contained, and half-typed code is the
/// normal state of a buffer rather than an edge case: a bare `def` at the end of a line recovers
/// into a node whose location is the three keyword bytes and whose name location is the
/// whitespace *after* them. There is no name to point at there, so the construct itself is the
/// honest answer — never a span the editor would have to reject.
pub(super) fn spans(definition: &Definition) -> ((u32, u32), (u32, u32)) {
    let offset = definition.offset();
    let full = (offset.start(), offset.end());
    let Some(name) = definition.name_offset() else {
        return (full, full);
    };
    let name = (name.start(), name.end());
    if name.0 >= full.0 && name.1 <= full.1 {
        (full, name)
    } else {
        (full, full)
    }
}

/// The file a `require "..."` names, if the graph has indexed it.
///
/// Jumping lands at the top of the file: a required file has no single "definition" to point
/// at, and the top is what every editor's own file navigation would have shown.
#[must_use]
pub fn require_site(graph: &Graph, path: &str, load_paths: &[PathBuf]) -> Option<Site> {
    let uri_id = query::resolve_require_path(graph, path, load_paths)?;
    let document = graph.documents().get(&uri_id)?;
    Some(Site {
        uri: document.uri().to_owned(),
        full: (0, 0),
        selection: (0, 0),
    })
}

/// rubydex's spelling of a method member: `shout()`, parentheses and all.
///
/// Declarations key their members by this string, but call sites are recorded under the bare
/// name — except for `alias`, which records the parenthesised form. Both have to arrive at the
/// same key or every method lookup silently misses.
fn member_name(graph: &Graph, str_id: StringId) -> Option<String> {
    let raw = graph.strings().get(&str_id)?.as_str();
    Some(if raw.ends_with(')') {
        raw.to_owned()
    } else {
        format!("{raw}()")
    })
}

fn resolve_call(graph: &Graph, reference: &MethodRef) -> Resolution {
    let Some(member) = member_name(graph, *reference.str()) else {
        return Resolution::precise(Vec::new());
    };
    let member_id = StringId::from(&member);

    // A receiver rubydex could name is the only precise path we have. Note that for `Foo.bar`
    // and for an implicit `self` in a class body the receiver resolves to the *singleton*
    // class, which is exactly where the singleton method lives.
    if let Some(receiver) = reference.receiver()
        && let Some(owner) = graph.name_id_to_declaration_id(receiver).copied()
    {
        if member == NEW
            && let Some(constructor) = constructor(graph, owner)
        {
            return Resolution::redirected(constructor);
        }

        match query::find_member_in_ancestors(graph, owner, member_id, false) {
            Ok(found) => return Resolution::precise(vec![found]),
            Err(FindMemberError::MemberNotFound) => {}
            Err(error) => {
                tracing::debug!("receiver {owner} is not searchable: {error:?}");
            }
        }
    }

    // No receiver, or a receiver that does not have the method: fall back to every declaration
    // whose name ends in this method. `Person#shout()` and `Person::<Person>#shout()` both
    // contain `#shout()`, and nothing else does.
    Resolution {
        declarations: query::declaration_search(graph, &format!("#{member}"), &MatchMode::Exact),
        precise: false,
        redirected: false,
    }
}

/// rubydex's spelling of the two members this file redirects between.
const NEW: &str = "new()";
const INITIALIZE: &str = "initialize()";

/// `Foo#initialize`, for a cursor on the `new` in `Foo.new`.
///
/// `Foo.new` really is `Class#new`, so resolving it exactly is correct and useless: the
/// constructor a human means by `new` is `Foo#initialize`, which is where ruby-lsp and
/// solargraph both go. Hover has the same problem — `Class#new(*args, **kwargs, &block)` and
/// RDoc's prose about `allocate` — and `Foo.new(` completes against the wrong signature.
///
/// It stays out of the way in the two cases where the honest answer is the better one: a class
/// that writes its own `def self.new` is reached by that method and not by `initialize`, and a
/// class whose only `initialize` is the empty one every object inherits has no constructor to
/// show, so `Class#new` may as well stand.
fn constructor(graph: &Graph, receiver: DeclarationId) -> Option<DeclarationId> {
    let attached = attached_class(graph, receiver)?;

    // Note that this finds nothing at all when rbs is not indexed — there is no `Class#new` in
    // rubydex's own built-ins — which is the same "nobody wrote one" answer.
    if let Some(new) = member_of(graph, receiver, NEW)
        && !owned_by(graph, new, "Class")
    {
        return None;
    }

    let initialize = member_of(graph, attached, INITIALIZE)?;
    (!owned_by(graph, initialize, "BasicObject")).then_some(initialize)
}

/// The class a singleton class is the singleton *of*, and nothing for anything else.
///
/// rubydex records it as the singleton's owner, so `<Foo>` reaches `Foo` without anyone having
/// to take a name apart.
fn attached_class(graph: &Graph, receiver: DeclarationId) -> Option<DeclarationId> {
    let declaration = graph.declarations().get(&receiver)?;
    matches!(
        declaration,
        Declaration::Namespace(Namespace::SingletonClass(_))
    )
    .then(|| *declaration.owner_id())
}

fn member_of(graph: &Graph, owner: DeclarationId, member: &str) -> Option<DeclarationId> {
    query::find_member_in_ancestors(graph, owner, StringId::from(member), false).ok()
}

/// Whether a declaration belongs to a named class, by rubydex's own back-pointer.
///
/// `Class` and `BasicObject` are the two that mean "nobody wrote this": they are part of Ruby's
/// object model rather than of any project, and rubydex names them identically whether they came
/// from rbs or from its own built-ins. Asking the owner rather than matching the method's own
/// name keeps this independent of how rubydex spells a member.
fn owned_by(graph: &Graph, declaration_id: DeclarationId, owner: &str) -> bool {
    graph
        .declarations()
        .get(&declaration_id)
        .is_some_and(|declaration| *declaration.owner_id() == DeclarationId::from(owner))
}

/// rubydex fabricates a constant reference to `<Foo>` for every call with an implicit or
/// constant receiver, so that `Foo.bar` can be resolved against `Foo`'s singleton class.
///
/// The user never wrote those bytes. Worse, for an implicit receiver the reference is given the
/// span of the *whole call*, so `alias_method :a, :b` would otherwise navigate to `class <<
/// self`. Singleton names are the only ones rubydex spells with angle brackets, which makes
/// them safe to recognise and drop.
fn is_synthetic(graph: &Graph, reference: &ConstantReference) -> bool {
    graph
        .names()
        .get(reference.name_id())
        .and_then(|name| graph.strings().get(name.str()))
        .is_some_and(|string| string.starts_with('<'))
}

fn covers(offset: &Offset, at: u32) -> bool {
    // Inclusive of the end so that a cursor parked just past the last character of an
    // identifier — where editors put it when you double-click the word — still counts.
    offset.start() <= at && at <= offset.end()
}

fn at<'g>(offset: &Offset, target: Target<'g>) -> Located<'g> {
    Located {
        start: offset.start(),
        end: offset.end(),
        target,
    }
}

impl Located<'_> {
    fn width(&self) -> u32 {
        self.end - self.start
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_the_graph_does_not_hold_is_answered_with_nothing() {
        // Not a hypothetical: `completionItem/resolve` takes its `DeclarationId` from the
        // client, which echoes back whatever the list it is looking at carried — and a config
        // reload drops the graph that list was built from. A rubydex id is a 64-bit hash, so
        // there is nothing about a stale one that distinguishes it from a live one, and the
        // whole chain from an id to a place in a file has to answer "nowhere" rather than
        // resolve against whatever the hash happens to collide with.
        let graph = Graph::new();
        let nowhere = DeclarationId::new(1_234_567_890_123_456_789);

        assert!(definitions_of(&graph, nowhere).is_empty());
        assert!(sites(&graph, nowhere).is_empty());
    }

    #[test]
    fn a_member_is_looked_up_under_the_spelling_rubydex_filed_it_by() {
        // rubydex keys a declaration's *members* by the parenthesised `shout()` but records a
        // call as the bare `shout`, so every method lookup goes through here. An empty graph
        // holds neither spelling, which is the case that has to answer `None` rather than
        // fabricate the parenthesised form out of an interned string that is not there.
        let graph = Graph::new();
        assert_eq!(member_name(&graph, StringId::from("shout")), None);
    }
}
