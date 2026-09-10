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

use std::{collections::HashSet, path::PathBuf};

use rubydex::{
    model::{
        declaration::{Ancestor, Declaration, Namespace},
        definitions::{Definition, Mixin},
        graph::Graph,
        ids::{DeclarationId, NameId, StringId, UriId},
        name::ParentScope,
        references::{ConstantReference, MethodRef},
    },
    offset::Offset,
    query::{self, FindMemberError, MatchMode},
};

use super::{
    cursor::{self, Context},
    synthesized::{Origin, Synthesized},
    types::{self, Derivation},
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
    /// What ya-lsp followed to type the receiver, when it followed anything.
    ///
    /// Empty for everything that comes through [`resolve`], which has no text to read and so no
    /// way to derive a type. Only [`resolve_typed`] can fill it, and only for a call.
    pub derivation: Derivation,
}

impl Resolution {
    fn precise(declarations: Vec<DeclarationId>) -> Self {
        Self {
            declarations,
            precise: true,
            redirected: false,
            derivation: Derivation::default(),
        }
    }

    fn redirected(declaration: DeclarationId) -> Self {
        Self {
            declarations: vec![declaration],
            precise: true,
            redirected: true,
            derivation: Derivation::default(),
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

/// The declarations a target names, with the receiver types this crate derives and the view
/// context a template gets.
///
/// The entry point for everything that has the document's text: `hover` and `definition`. What
/// it adds over [`resolve`] is two rungs between "rubydex named the receiver" and "matched on
/// the method name alone" — a receiver ya-lsp typed itself, from a signature or an assignment,
/// and a **bare** name in a template, which has no receiver to type at all. The [`Derivation`]
/// on the answer is how the card says which.
///
/// **The two are told apart by the syntax and never both asked.** A call with a receiver
/// written is not an implicit one, so `cursor::at` is run once here and its answer dispatches:
/// the classification the two rungs need is the same classification, and asking it twice would
/// be a second parse of the file on a path that already pays for one.
///
/// Callers with no text — `references`, the type hierarchy — go through [`resolve`] and get
/// exactly what they got before. That is not a gap to close: `references` must not follow a
/// derived type, because a work list is a list of places to *edit* and a derived receiver is
/// the one thing in it that could be wrong.
#[must_use]
pub fn resolve_typed(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    source: &str,
    located: &Located<'_>,
    at_in_source: u32,
) -> Resolution {
    let graph = sources.graph;
    let Target::Call(reference) = located.target else {
        return resolve(graph, located);
    };
    let resolution = resolve_call(graph, reference);
    // Only where rubydex could not name the receiver itself. A derived type is a *worse* answer
    // than a resolved one and must never displace it.
    if resolution.precise {
        return resolution;
    }
    let Some(member) = member_name(graph, *reference.str()) else {
        return resolution;
    };
    // The same classification completion runs, asked about a call the user has finished writing
    // rather than one they are in the middle of. `cursor::at` needs no special case for that:
    // its test is that the cursor sits between the operator and the end of the message, which a
    // cursor resting *on* a method name does.
    // **`source` is the buffer and `located` is the graph's, and those are two different
    // texts** whenever a keystroke has not been indexed yet. `Scope::at` below is asked in the
    // graph's coordinates because it reads the graph; `cursor::at` is asked in the buffer's
    // because it parses the buffer. Passing `located.start` to both reads the token to the left
    // of the one the user is on.
    let Some(cursor) = cursor::at(source, at_in_source) else {
        return resolution;
    };
    match &cursor.context {
        Context::MethodCall { receiver } => {
            let scope = types::Scope::at(graph, uri_id, located.start);
            let Some(typed) = types::method_receiver(sources, uri_id, receiver, &scope) else {
                return resolution;
            };
            match query::find_member_in_ancestors(
                graph,
                typed.declaration,
                StringId::from(&member),
                false,
            ) {
                Ok(found) => Resolution {
                    declarations: vec![found],
                    precise: true,
                    redirected: false,
                    derivation: typed.derivation,
                },
                // The receiver was typed and the method is not on it. The name-based list is
                // still the honest answer: a signature can be incomplete, and this is exactly
                // the case where a wrong "no such method" would be worse than a guess.
                Err(_) => resolution,
            }
        }
        // No receiver written at all, which in a template means the view context.
        // `Argument` is here for the same reason `Context::allows_private` treats the two
        // alike: `link_to "x", story_path(s)` writes `story_path` with an implicit receiver
        // exactly as a statement of its own would.
        Context::Expression | Context::Argument { .. } => sources
            .views
            .reachable(graph, uri_id)
            .and_then(|reachable| reachable.member(graph, &member))
            .map_or(resolution, |found| Resolution {
                declarations: vec![found.declaration],
                precise: true,
                redirected: false,
                derivation: Derivation {
                    view: Some(found.how),
                    ..Derivation::default()
                },
            }),
        Context::NamespaceAccess { .. } => resolution,
    }
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

/// The method a call written at `offset` resolves to, and only when it resolved exactly.
///
/// The one gate on everything that shows a *signature* for a call rather than a place to jump
/// to: completion's keyword arguments and `textDocument/signatureHelp` both fire from here.
/// A name-based match would put another class's parameters under the cursor, and unlike a wrong
/// navigation that is a wrong answer the user cannot see is wrong — it is syntactically valid.
///
/// The redirect is deliberately kept. `Foo.new(` resolves to `Foo#initialize`, whose parameters
/// are the ones the call actually takes; `references` is the caller that must not have it, and
/// it reads [`Resolution`] directly.
#[must_use]
pub fn precise_call(graph: &Graph, uri_id: UriId, offset: u32) -> Option<DeclarationId> {
    locate(graph, uri_id, offset)
        .into_iter()
        .find_map(|located| match located.target {
            Target::Call(_) => {
                let resolution = resolve(graph, &located);
                resolution
                    .precise
                    .then(|| resolution.declarations.into_iter().next())
                    .flatten()
            }
            _ => None,
        })
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

/// Which single definition of a declaration a list should point at.
///
/// The user's own code wins when a name is defined in both: opening a Rails app and searching
/// for `ApplicationRecord` should land in `app/models`, not in whichever gem reopens it.
/// Otherwise it is the first in [`definitions_of`]'s stable order, so the answer never moves
/// between runs.
///
/// Shared by the symbol picker and by the type hierarchy, because "which of the two hundred
/// places `ActiveRecord::Base` is reopened does this row mean" is one question, and two answers
/// to it would put a symbol in a different file depending on which list it was found in.
#[must_use]
pub fn preferred_definition<'g>(
    graph: &'g Graph,
    declaration_id: DeclarationId,
    own: &HashSet<UriId>,
) -> Option<&'g Definition> {
    let definitions = definitions_of(graph, declaration_id);
    definitions
        .iter()
        .find(|definition| own.contains(definition.uri_id()))
        .or(definitions.first())
        .copied()
}

/// Whether any of a declaration's definitions is in the user's own code.
///
/// `any`, not "the first one": a class the project reopens is the project's, even when the gem
/// that first defined it sorts ahead of it. Both callers rank by it, so it decides where a name
/// appears in the picker *and* where it appears in a capped list of subtypes.
#[must_use]
pub fn declared_in(graph: &Graph, declaration: &Declaration, own: &HashSet<UriId>) -> bool {
    declaration.definitions().iter().any(|id| {
        graph
            .definitions()
            .get(id)
            .is_some_and(|definition| own.contains(definition.uri_id()))
    })
}

/// Every place a declaration is written, in the same order as [`definitions_of`].
#[must_use]
pub fn sites(graph: &Graph, synthesized: &Synthesized, declaration_id: DeclarationId) -> Vec<Site> {
    let mut sites: Vec<Site> = definitions_of(graph, declaration_id)
        .into_iter()
        .filter_map(|definition| site(graph, synthesized, definition))
        .collect();
    sites.dedup();
    sites
}

/// Where a single definition is written.
///
/// **The one place a declaration becomes a place**, which is why the side table of everything
/// ya-lsp generated itself is consulted here and nowhere else. A definition in generated text
/// sits at an offset into bytes that are not on disk, so it answers with the line that implied
/// it — or, where nothing recorded one, with nothing at all. Dropping it is the point: the
/// alternative is a link into a document the editor cannot open, and every list that reaches
/// this function already filters the misses out.
#[must_use]
pub fn site(graph: &Graph, synthesized: &Synthesized, definition: &Definition) -> Option<Site> {
    match synthesized.origin(definition.uri_id(), definition.offset().start()) {
        Origin::Declared(declared) => return Some(declared.clone()),
        Origin::Unknown => return None,
        Origin::OnDisk => {}
    }
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
    if nests(full, name) {
        (full, name)
    } else {
        (full, full)
    }
}

/// Whether `inner` sits inside `outer`.
///
/// One predicate rather than two: `spans` above needs it for the pair the protocol requires to
/// nest, and `ranges::selection_chain` needs it for the chain the protocol *defines* as nesting.
/// Both are guarding against the same parser recovery, and a containment test written twice is a
/// containment test that will one day disagree with itself.
pub(super) const fn nests(outer: (u32, u32), inner: (u32, u32)) -> bool {
    inner.0 >= outer.0 && inner.1 <= outer.1
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

    // Whether `self` here is a **class object** — `Foo.bar`, or a bare call written as a
    // statement of a class or module body. rubydex answers it by giving both the singleton
    // class as the receiver, and a bare call inside a `def` the class itself, so this is one
    // question with one answer rather than a syntactic test. The concern edge's fallback filter turns
    // on it.
    let mut on_a_class_object = false;

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
            // A hit on one of the three [`ROOTS`] is the one hit worth asking a second question
            // about, and the reason is **Ruby's own method resolution order**: a module
            // `extend`ed onto a class object sits in the singleton chain above `Class`, `Module`
            // and `Object`, so a concern's `ClassMethods` was always meant to be reached first.
            // Asking it only after the ancestor walk puts the two in the wrong order, which is
            // invisible until something lands on a root.
            //
            // Something does. `Object` is an ancestor of every class object, so a `def` at the
            // top level of any file answers here for every class in the workspace — and a `def`
            // written inside a **block** is recorded identically, because rubydex has no notion
            // of a block: its nesting stack holds lexical scopes, `Class.new` owners and methods,
            // and a `describe "x" do` pushes none of the three. Unrepaired, that makes an RSpec
            // helper named `validate` shadow `ActiveModel::Validations::ClassMethods#validate` in
            // every model in the project.
            //
            // The root answer is kept wherever the concern edge has nothing, so a genuine
            // top-level `def` called from a class body below it resolves unchanged.
            Ok(found) if owned_by_a_root(graph, found) => {
                return Resolution::precise(vec![
                    extended_by_a_concern(graph, owner, member_id).unwrap_or(found),
                ]);
            }
            Ok(found) => return Resolution::precise(vec![found]),
            Err(FindMemberError::MemberNotFound) => {}
            Err(error) => {
                tracing::debug!("receiver {owner} is not searchable: {error:?}");
            }
        }
        if let Some(found) = extended_by_a_concern(graph, owner, member_id) {
            return Resolution::precise(vec![found]);
        }
        on_a_class_object = attached_class(graph, owner).is_some();
    }

    // No receiver, or a receiver that does not have the method: fall back to every declaration
    // whose name ends in this method. `Person#shout()` and `Person::<Person>#shout()` both
    // contain `#shout()`, and nothing else does.
    let query = format!("#{member}");
    let candidates = query::declaration_search(graph, &[&query], &MatchMode::Exact);
    Resolution {
        declarations: if on_a_class_object {
            reachable_on_a_class_object(graph, candidates)
        } else {
            candidates
        },
        precise: false,
        redirected: false,
        derivation: Derivation::default(),
    }
}

/// The three namespaces that are an ancestor of everything.
///
/// A member found on one of them is a member found on *every* receiver, which is what makes a
/// hit there carry so little information — and what makes rubydex's one mis-attribution cost so
/// much. `BasicObject` is included for completeness rather than for evidence: nothing in six
/// corpora reopens it, and a rule about the roots that names two of the three would be a rule
/// with a hole in it.
const ROOTS: [&str; 3] = ["Object", "Kernel", "BasicObject"];

/// Whether the member the ancestor walk found is declared on one of [`ROOTS`].
fn owned_by_a_root(graph: &Graph, found: DeclarationId) -> bool {
    ROOTS.iter().any(|root| owned_by(graph, found, root))
}

/// rubydex's spelling of the nested module a Rails concern installs on its includers.
const CLASS_METHODS: &str = "ClassMethods";

/// The member a concern's nested `ClassMethods` puts on the singleton of every class that
/// includes it — the one edge in Rails that nobody's file writes.
///
/// `ActiveSupport::Concern#append_features` ends with `base.extend const_get(:ClassMethods)`.
/// Rails writes the `include` statically and never writes that `extend`, so ya-lsp's singleton
/// lookup correctly finds nothing and `validates`, `scope`, `belongs_to` and `has_many` in a
/// model body have always answered on the name rung — 4,949 sites across five applications.
/// **The edge is walked here rather than declared**, because the class it would have to be
/// declared on is `ActiveRecord::Base`, which lives in a gem's `lib/` and is not a document any
/// generator may write into.
///
/// **The gate is the nested `module ClassMethods` and deliberately not `extend
/// ActiveSupport::Concern`**, and the corpus is the argument: of the 17 modules across six
/// applications that hold one, 8 extend `ActiveSupport::Concern`, 6 hand-roll
/// `def self.included(base); base.extend(ClassMethods); end` and 3 do neither — so an
/// `ActiveSupport` test would decline more than half of the convention it is named for. Both
/// spellings install the same module on the same singleton, and the nested module is the thing
/// they have in common.
///
/// It is asked **after** the ordinary ancestor search and only when that found nothing, so a
/// method a class really declares can never be displaced by one a concern extends onto it.
fn extended_by_a_concern(
    graph: &Graph,
    singleton: DeclarationId,
    member: StringId,
) -> Option<DeclarationId> {
    extended_class_methods(graph, singleton)
        .into_iter()
        .find_map(|found| own_member(graph, found.class_methods, member))
}

/// What the module itself declares, and deliberately **not** what its ancestors do.
///
/// `extend M` really does install the instance methods of `M`'s ancestors, so an ancestor walk
/// is the right reading of Ruby and the wrong reading of the graph. rubydex records an
/// `include` written **inside a `def`**
/// as a mixin of the enclosing namespace, and that is exactly how Rails writes the ones that
/// matter here:
///
/// ```ruby
/// module ClassMethods
///   def has_secure_password(...)
///     include ActiveModel::Validations   # the *record's*, when the macro is called
///   end
/// end
/// ```
///
/// Over Rails' five core gems and the six corpora, 125 `module ClassMethods` blocks hold **2**
/// mixins written in the module body and **19** written inside a `def`, and every one of the 19
/// means the class the macro was called on. The applications write none of either: their own 20
/// blocks hold no module-body mixin, and the 4 in-`def` ones are in a vendored gem the default
/// include excludes. The idiom is Rails' and its plugins'. Following them made `Category.valid?` — which raises in Ruby — answer
/// with `ActiveModel::Validations#valid?`, and put six of that module's *instance* methods into
/// the completion list for a class object. The two legitimate ones are
/// `ActiveRecord::Callbacks::ClassMethods` and `AbstractController::Helpers::ClassMethods`; what
/// is lost with them is `define_model_callbacks` and two helper-path methods, named here rather
/// than estimated.
fn own_member(graph: &Graph, module: DeclarationId, member: StringId) -> Option<DeclarationId> {
    graph
        .declarations()
        .get(&module)?
        .as_namespace()?
        .member(&member)
        .copied()
}

/// One concern's nested `ClassMethods`, and how far out the class that installed it sits.
pub(super) struct Extends {
    /// The module `base.extend` was handed — where the members themselves are declared.
    pub class_methods: DeclarationId,
    /// How many classes the receiver's chain passes through before reaching the one whose
    /// `include` ran this `extend`, which is where the module sits in Ruby's *singleton* chain.
    ///
    /// It is not the concern's own step in the ancestor list, and that distinction is the whole
    /// of the ranking argument. The two chains have different lengths: a Rails model's instance
    /// ancestors are forty rungs of concerns and its singleton chain is five classes, so
    /// `ActiveModel::Validations` at instance step 12 would score `validates` *below*
    /// `Object`'s own methods. Counting only the classes gives the position of the singleton
    /// the `extend` really landed on — `Story` includes `Countable`, so
    /// `Countable::ClassMethods` is one step out, exactly where `Story.singleton_class
    /// .ancestors` puts it.
    pub step: usize,
}

/// Every nested `ClassMethods` a class object sees, nearest first.
///
/// The walk both halves of the edge share: [`extended_by_a_concern`] takes the first module
/// that answers one member and `completion` collects every member of all of them, so the gate —
/// which is the whole safety argument above — is stated once. An empty answer is the ordinary
/// case in every language but Rails: a receiver that is not a class object, or a chain with no
/// concern in it.
pub(super) fn extended_class_methods(graph: &Graph, singleton: DeclarationId) -> Vec<Extends> {
    let Some(attached) = attached_class(graph, singleton) else {
        return Vec::new();
    };
    let Some(namespace) = graph
        .declarations()
        .get(&attached)
        .and_then(Declaration::as_namespace)
    else {
        return Vec::new();
    };
    let nested = StringId::from(CLASS_METHODS);
    let mut found = Vec::new();
    let mut classes = 0;
    // Ancestor order, which is the linearization of the `include`s — and the order the
    // `extend`s ran in, because each of them happens at the moment its `include` does.
    for ancestor in namespace.ancestors() {
        let Ancestor::Complete(id) = ancestor else {
            continue;
        };
        // The `extend`s this declaration's own bodies write, which is the *general*
        // form of the edge above and is read from the file rather than from a convention.
        //
        // **Only where the receiver's singleton chain really passes through this declaration's
        // singleton**: the attached declaration itself, and the classes above it. An `extend`
        // written in an *included module* lands on that module's own singleton and never on the
        // includer's, so walking it would install methods Ruby does not.
        if *id == attached || !is_module(graph, *id) {
            for extended in extends_written_on(graph, *id) {
                found.push(Extends {
                    class_methods: extended,
                    step: classes,
                });
            }
        }
        // A **module**, because a class is not something an `include` can name. A nested
        // `ClassMethods` that is a constant rather than a module survives this and declines
        // where it is asked for a member, in `own_member` and in `completion`'s `namespace`:
        // both ask for a `Namespace` and a constant is not one.
        let Some(Declaration::Namespace(concern @ Namespace::Module(_))) =
            graph.declarations().get(id)
        else {
            classes += 1;
            continue;
        };
        if let Some(class_methods) = concern.member(&nested) {
            found.push(Extends {
                class_methods: *class_methods,
                step: classes,
            });
        }
    }
    found
}

fn is_module(graph: &Graph, declaration: DeclarationId) -> bool {
    matches!(
        graph.declarations().get(&declaration),
        Some(Declaration::Namespace(Namespace::Module(_)))
    )
}

/// The modules one declaration's own bodies `extend`, resolved by name.
///
/// **`extend` is not unread.** rubydex reads it and attaches it to the singleton class exactly
/// as Ruby does — `extend Formatter` on a module resolves, and so does `extend Ns::Fmt` written
/// in Ruby. What does **not** resolve is one shape, isolated by four probes against one
/// workspace:
///
/// | written | resolves |
/// | --- | --- |
/// | Ruby, `extend Flat` / `extend Ns::Fmt` / `include Ns::Fmt` | yes |
/// | RBS, `extend Flat` | yes |
/// | RBS, `include Ns::Fmt` | yes |
/// | **RBS, `extend Ns::Fmt`** | **no** |
///
/// That last row is the whole gap. `stdlib/securerandom/0/securerandom.rbs:48` is
/// `extend Random::Formatter` and `SecureRandom.hex` is the commonest call it costs; **15 of
/// the 22 `extend`s in the vendored signatures are qualified**, `CGI::Util`, `Minitest::Spec
/// ::DSL` and nine `OpenSSL::Marshal::ClassMethods` among them.
///
/// So this is a repair rather than a second walk, and it can only ever add: it is reached from
/// [`extended_by_a_concern`], which resolution asks **after** the ordinary ancestor search came
/// back empty, and from `completion`'s `Extended`, which drops every member the ordinary walk
/// already offers. An `extend` rubydex did linearize is found by the search and never gets
/// here, so the two cannot double-count.
fn extends_written_on(graph: &Graph, declaration: DeclarationId) -> Vec<DeclarationId> {
    definitions_of(graph, declaration)
        .into_iter()
        .flat_map(|definition| match definition {
            Definition::Class(class) => class.mixins(),
            Definition::Module(module) => module.mixins(),
            Definition::SingletonClass(singleton) => singleton.mixins(),
            _ => &[],
        })
        .filter_map(|mixin| match mixin {
            Mixin::Extend(extend) => Some(*extend.constant_reference_id()),
            Mixin::Include(_) | Mixin::Prepend(_) => None,
        })
        .filter_map(|reference| {
            let reference = graph.constant_references().get(&reference)?;
            let name_id = *reference.name_id();
            let path = constant_path(graph, name_id)?;
            let nesting = graph
                .names()
                .get(&name_id)
                .and_then(|name| *name.nesting())
                .and_then(|id| constant_path(graph, id));
            resolve_outwards(graph, nesting.as_deref(), &path)
        })
        .collect()
}

/// A name's whole path, `A::B::C`, out of the chain of parent scopes rubydex interned it as.
///
/// `Random::Formatter` is two `Name`s and the graph holds only the last segment's string on
/// each, so a lookup by name has to put them back together. A leading `::` is dropped: an
/// absolute path and a top-level one name the same declaration, and the declarations are keyed
/// without it.
fn constant_path(graph: &Graph, name_id: NameId) -> Option<String> {
    let name = graph.names().get(&name_id)?;
    let segment = graph.strings().get(name.str())?.as_str();
    Some(match name.parent_scope() {
        ParentScope::Some(parent) => format!("{}::{segment}", constant_path(graph, *parent)?),
        // `Foo::<Foo>` is rubydex's spelling of a singleton and never something an `extend`
        // names, and `::Foo` and `Foo` are one declaration.
        ParentScope::Attached(_) | ParentScope::None | ParentScope::TopLevel => segment.to_owned(),
    })
}

/// A constant resolved the way Ruby resolves one: outwards through the nesting, then the top
/// level.
///
/// [`types::declared`] is the lookup and this is the lexical half around it, which is the same
/// pair `types`' own guess rung uses. There is no reference to read here — the one rubydex
/// recorded is exactly the one that did not resolve — so what is left is the scoping rule
/// spelled out.
fn resolve_outwards(graph: &Graph, nesting: Option<&str>, name: &str) -> Option<DeclarationId> {
    let mut scopes: Vec<&str> = nesting
        .map(|path| path.split("::").collect())
        .unwrap_or_default();
    while !scopes.is_empty() {
        if let Some(found) = types::declared(graph, &format!("{}::{name}", scopes.join("::"))) {
            return Some(found);
        }
        scopes.pop();
    }
    types::declared(graph, name)
}

/// The candidates a call on a class object could possibly mean, out of everything sharing the
/// name.
///
/// The other half of the concern edge, and the half a user meets first: `scope` in a model
/// body offered 37 candidates headed by a *routing* method and `belongs_to` offered 7 headed by
/// a migration method, because the name-based list is every declaration in the graph whose name
/// ends this way and nothing narrowed it.
///
/// **An instance method of a class is never it.** What a class object answers is its own
/// singleton chain — the singleton classes of its ancestors, plus whatever is `extend`ed onto
/// it, plus the instance methods of `Class`, `Module`, `Object` and `Kernel` — and rubydex puts
/// every one of those in the singleton's own ancestors, which the search above already walked
/// and did not find the member in. So a surviving candidate owned by a `class` is provably
/// unreachable, while one owned by a `module` is exactly the case this list must keep: a
/// concern's `ClassMethods` is a module's *instance* method, and it is the right answer for
/// every site the edge above could not resolve.
///
/// The list is only ever narrowed, never emptied: where the graph genuinely holds nothing but
/// instance methods of that name, a guess is still the honest answer and the unfiltered list is
/// what is returned.
fn reachable_on_a_class_object(
    graph: &Graph,
    candidates: Vec<DeclarationId>,
) -> Vec<DeclarationId> {
    let kept: Vec<DeclarationId> = candidates
        .iter()
        .copied()
        .filter(|id| {
            graph.declarations().get(id).is_some_and(|declaration| {
                !matches!(
                    graph.declarations().get(declaration.owner_id()),
                    Some(Declaration::Namespace(Namespace::Class(_)))
                )
            })
        })
        .collect();
    if kept.is_empty() { candidates } else { kept }
}

/// rubydex's spelling of the two members this file redirects between.
const NEW: &str = "new()";
const INITIALIZE: &str = "initialize()";

/// `Foo#initialize`, for a cursor on the `new` in `Foo.new`.
///
///`Foo.new` really is `Class#new`, so resolving it exactly is correct and useless: the constructor
///a human means by `new` is `Foo#initialize`. Hover has the same problem — `Class#new(*args,
///**kwargs, &block)` and RDoc's prose about `allocate` — and `Foo.new(` completes against the wrong
///signature.
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
        assert!(sites(&graph, &Synthesized::new(), nowhere).is_empty());
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
