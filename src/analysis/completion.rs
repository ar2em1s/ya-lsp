//! `textDocument/completion` — what can be written where the cursor is.
//!
//! [`cursor`] says what *shape* the cursor is in; this says what the graph has to offer there.
//! The split matters because the shape is pure syntax and the offer is pure semantics, and only
//! one of them needs a project loaded to test.
//!
//! # What is exact and what is a guess
//!
//! Three of the four contexts are exact, because in each of them the receiver is something the
//! graph resolved: `Foo::`, `Foo.`, `self.`, and a bare word (whose receiver is the enclosing
//! `self`). rubydex walks the real ancestor chain, applies real visibility — a `private` method
//! is offered inside the class and not outside it — and for an argument list it hands back the
//! called method's keyword parameters.
//!
//! The fourth is `foo.` where `foo` is a local, an instance variable, or the result of another
//! call. Knowing what that is takes type inference, which ya-lsp does not have, so the list falls
//! back to every method name in the project. That is a guess and it is presented as one: names
//! only, deduplicated, the user's own code first. It is still better than the editor's own
//! word-list, which cannot see a method defined in a file that is not open.
//!
//! # Why the list is always `isIncomplete`
//!
//! Completion is filtered here rather than in the client, because the alternative is shipping a
//! Rails bundle's hundred thousand candidates on the first keystroke and letting the editor sort
//! it out. Filtering server-side means the answer is only correct for the prefix it was asked
//! with, and `isIncomplete` is exactly the flag that tells the client to ask again rather than
//! narrow what it already has.

use std::collections::{HashMap, HashSet, hash_map::Entry};

use rubydex::{
    model::{
        declaration::{Ancestor, Declaration, Namespace},
        definitions::{Definition, Receiver as DefinitionReceiver},
        graph::Graph,
        ids::{DeclarationId, NameId, StringId, UriId},
        name::{Name, ParentScope},
        visibility::Visibility,
    },
    query::{self, CompletionCandidate, CompletionContext, CompletionReceiver, MatchMode},
};

use super::{
    cursor::{self, Context, Receiver},
    locator, render,
};

/// One suggestion, before it is dressed up as an LSP item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// What gets inserted, and what the user reads: `shout`, `Person`, `@name`, `volume:`, `def`.
    pub label: String,
    /// The full spelling, shown beside the label: `HR::Person#shout`.
    pub detail: Option<String>,
    pub kind: Kind,
    /// Filled in immediately for keywords, whose documentation is a constant. For everything
    /// else it is `completionItem/resolve`'s job, so that a list of 300 does not read 300
    /// comment blocks the user will never look at.
    pub documentation: Option<String>,
    pub deprecated: bool,
    /// The declaration this came from, for `completionItem/resolve` to find again.
    pub declaration: Option<DeclarationId>,
}

/// What a suggestion is, in the terms an editor draws icons for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Class,
    Module,
    Constant,
    Method,
    Variable,
    Field,
    Keyword,
}

/// A completion list, and the span it replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub items: Vec<Item>,
    /// Always true. See the module docs.
    pub incomplete: bool,
    pub start: u32,
    pub end: u32,
}

/// What can be written at `offset`.
///
/// `None` where nothing can — inside a comment or a literal, or in a document the graph has
/// never seen.
#[must_use]
pub fn complete(
    graph: &Graph,
    uri_id: UriId,
    source: &str,
    offset: u32,
    limit: usize,
    own: &HashSet<UriId>,
) -> Option<Completion> {
    let cursor = cursor::at(source, offset)?;
    let prefix = &source[cursor.start as usize..cursor.end as usize];

    let scope = Scope::at(graph, uri_id, offset);
    // Pure syntax, and decided before anything is looked up: see `Context::allows_private`.
    let private_ok = cursor.context.allows_private();
    let locality = Locality::at(graph, uri_id, own);
    let items = match receiver_for(graph, uri_id, cursor.context, &scope) {
        Some((receiver, only)) => {
            let ranking = Ranking {
                prefix,
                distance: Distance::from_receiver(graph, &receiver),
                locality,
                private_ok,
            };
            from_graph(graph, receiver, only, limit, &ranking)
        }
        // Only one context arrives here with anything worth saying: a `.` on a receiver whose
        // type is unknown. `foo::` and a receiver that is not a namespace have no honest answer.
        None => match cursor.context {
            Context::MethodCall { .. } => {
                // No receiver means no chain, so `Locality` is the whole of what ranks this
                // list. See the note on `Distance::none`.
                let ranking = Ranking {
                    prefix,
                    distance: Distance::none(),
                    locality,
                    private_ok,
                };
                by_name(graph, limit, &ranking)
            }
            _ => Vec::new(),
        },
    };

    Some(Completion {
        items,
        incomplete: true,
        start: cursor.start,
        end: cursor.end,
    })
}

/// The lexical scope and the `self` type at an offset.
struct Scope {
    /// The innermost `class`/`module`/`class << self` the offset is inside, as rubydex names it.
    /// Top-level code is inside `Object`, which is what Ruby says too.
    nesting: NameId,
    /// Set only where `self` is not the nesting: `def self.build` and `def Foo.build`.
    self_id: Option<DeclarationId>,
}

/// rubydex's name for the top-level scope.
///
/// Ruby's top level *is* `Object`, and rubydex indexes a built-in `class Object` so the name
/// exists in every graph. Building the id rather than looking it up costs a hash and no lookup.
fn object_name() -> NameId {
    Name::new(StringId::from("Object"), ParentScope::None, None).id()
}

impl Scope {
    fn at(graph: &Graph, uri_id: UriId, offset: u32) -> Self {
        let object = object_name();
        let Some(document) = graph.documents().get(&uri_id) else {
            return Self {
                nesting: object,
                self_id: None,
            };
        };

        // Definitions span their whole body, so the ones covering the cursor are exactly the
        // constructs it is written inside, and the narrowest is the innermost.
        let mut namespace: Option<&Definition> = None;
        let mut method: Option<&Definition> = None;
        for definition in document
            .definitions()
            .iter()
            .filter_map(|id| graph.definitions().get(id))
        {
            let span = definition.offset();
            if span.start() > offset || offset > span.end() {
                continue;
            }
            let target = match definition {
                Definition::Class(_) | Definition::Module(_) | Definition::SingletonClass(_) => {
                    &mut namespace
                }
                Definition::Method(_) => &mut method,
                _ => continue,
            };
            if target.is_none_or(|held| wider(held.offset(), span)) {
                *target = Some(definition);
            }
        }

        let nesting = namespace
            .and_then(|definition| definition.name_id().copied())
            .unwrap_or(object);

        Self {
            nesting,
            self_id: self_of(graph, namespace.is_some(), method, nesting),
        }
    }

    /// The declaration the nesting names, if the graph resolved it.
    fn nesting_id(&self, graph: &Graph) -> Option<DeclarationId> {
        graph.name_id_to_declaration_id(self.nesting).copied()
    }

    /// Who is calling, for a receiver context to check visibility against.
    ///
    /// `Expression` may leave this `None` — rubydex derives `self` from the nesting there. The
    /// two receiver contexts derive nothing: an unstated `self` is treated as an outsider, and
    /// a class would stop being able to see its own private methods.
    fn caller(&self, graph: &Graph) -> Option<DeclarationId> {
        self.self_id.or_else(|| self.nesting_id(graph))
    }
}

/// `self`, where it is not the enclosing class.
///
/// Two places it is not, and both matter:
///
/// - **A class or module body.** `self` there is the class *object*, so what can be called is
///   `Foo`'s singleton methods — which is the entire Rails DSL. `validates`, `has_many`, `scope`
///   and `belongs_to` are all class methods, and completing a model's body against the instance
///   side offers `valid?` and `validate` while silently omitting every macro anyone writes
///   there. Measured on a real app: 49 suggestions for `valid`, not one of them `validates`.
/// - **`def self.build` and `def Foo.build`.** The lexical scope stays the class while `self`
///   moves to the singleton, and constants follow the first while methods follow the second.
///
/// The top level is not one of them: `self` is `main`, an ordinary `Object`, so what rubydex
/// derives from the nesting is already right and `None` says so.
fn self_of(
    graph: &Graph,
    in_namespace: bool,
    method: Option<&Definition>,
    nesting: NameId,
) -> Option<DeclarationId> {
    let Some(Definition::Method(method)) = method else {
        return in_namespace
            .then(|| singleton_of_name(graph, nesting))
            .flatten();
    };
    match method.receiver().as_ref()? {
        DefinitionReceiver::SelfReceiver(_) => singleton_of_name(graph, nesting),
        DefinitionReceiver::ConstantReceiver(name_id) => singleton_of_name(graph, *name_id),
    }
}

fn singleton_of_name(graph: &Graph, name: NameId) -> Option<DeclarationId> {
    singleton_of(graph, *graph.name_id_to_declaration_id(name)?)
}

/// Which candidates a context can accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Only {
    Everything,
    /// After a `::`, where a method or a keyword would not be legal to write.
    Constants,
}

/// Turn a classified cursor into the question rubydex answers.
///
/// `None` means there is no exact question to ask: an unknown receiver, or a `::` on something
/// that is not a namespace.
fn receiver_for(
    graph: &Graph,
    uri_id: UriId,
    context: Context,
    scope: &Scope,
) -> Option<(CompletionReceiver, Only)> {
    match context {
        Context::Expression => Some((
            CompletionReceiver::Expression {
                self_decl_id: scope.self_id,
                nesting_name_id: scope.nesting,
            },
            Only::Everything,
        )),
        Context::Argument { name } => {
            // Only a receiver rubydex could name gives real keyword arguments. A name-based
            // guess would put another class's parameters into this call, which is worse than
            // offering none: the completion would be syntactically valid and wrong.
            let receiver = match locator::precise_call(graph, uri_id, name) {
                Some(method_decl_id) => CompletionReceiver::MethodArgument {
                    self_decl_id: scope.self_id,
                    nesting_name_id: scope.nesting,
                    method_decl_id,
                },
                None => CompletionReceiver::Expression {
                    self_decl_id: scope.self_id,
                    nesting_name_id: scope.nesting,
                },
            };
            Some((receiver, Only::Everything))
        }
        Context::NamespaceAccess { receiver } => {
            let namespace_decl_id = match receiver {
                Receiver::Constant(offset) => constant_at(graph, uri_id, offset)?,
                // `self::CONST` is legal and rare; the nesting is what it means. `caller` is
                // the singleton in a class or module body, and asking a singleton reaches the
                // class it is attached to — so this answers the same list `HR::` does, from a
                // body and from inside a method alike. Pinned by
                // `every_receiver_that_can_precede_a_double_colon_is_answered`, because the
                // equivalence is rubydex's and not something this line states.
                Receiver::SelfObject => scope.caller(graph)?,
                // `"foo"::Bar` and `Foo.new::Bar` parse, and mean nothing anybody writes on
                // purpose. An instance is not a namespace.
                Receiver::Instance(_) | Receiver::Literal(_) => return None,
                // `::Foo` asks for the top level, which is `Object` — but rubydex's namespace
                // walk deliberately stops *before* Object's own members, to keep `String::` from
                // listing every top-level constant in the project. Asking the same question as
                // an expression at the top level reaches them, and dropping everything that is
                // not a constant leaves what `::` can actually be followed by.
                Receiver::TopLevel => {
                    return Some((
                        CompletionReceiver::Expression {
                            self_decl_id: None,
                            nesting_name_id: object_name(),
                        },
                        Only::Constants,
                    ));
                }
                Receiver::Unknown => return None,
            };
            Some((
                CompletionReceiver::NamespaceAccess {
                    self_decl_id: scope.caller(graph),
                    namespace_decl_id,
                },
                Only::Everything,
            ))
        }
        Context::MethodCall { receiver } => {
            let receiver_decl_id = match receiver {
                // `Foo.bar` calls a *singleton* method, so the receiver is `Foo`'s singleton
                // class. A constant that is not a namespace — `MAX.times` — has a type we
                // cannot name, and falls through to the name-based list.
                Receiver::Constant(offset) => {
                    singleton_of(graph, constant_at(graph, uri_id, offset)?)?
                }
                Receiver::SelfObject => scope.caller(graph)?,
                // An instance, so the receiver is the class itself rather than its singleton.
                Receiver::Instance(offset) => constant_at(graph, uri_id, offset)?,
                // A literal's class is named, not resolved — `String` means `String` in every
                // file. `declared` is what makes turning `[rbs]` off degrade rather than break:
                // with no core signatures in the graph there is no such declaration, and
                // falling through here reaches the name-based list instead of answering with
                // nothing at all.
                Receiver::Literal(class) => declared(graph, class)?,
                // `::Foo.bar` reaches here as an ordinary constant; a bare `::` never does.
                Receiver::TopLevel | Receiver::Unknown => return None,
            };
            Some((
                CompletionReceiver::MethodCall {
                    self_decl_id: scope.caller(graph),
                    receiver_decl_id,
                },
                Only::Everything,
            ))
        }
    }
}

/// The declaration a constant written at `offset` resolves to.
///
/// This goes through the locator rather than re-deriving Ruby's constant lookup: the reference
/// under the cursor was resolved by rubydex against the real nesting and the real ancestors,
/// which is a great deal more than a name match would be.
fn constant_at(graph: &Graph, uri_id: UriId, offset: u32) -> Option<DeclarationId> {
    locator::locate(graph, uri_id, offset)
        .into_iter()
        .find_map(|located| match located.target {
            locator::Target::Constant(_) => locator::resolve(graph, &located)
                .declarations
                .into_iter()
                .next(),
            _ => None,
        })
}

/// The declaration a name refers to, when the graph holds one.
///
/// A `DeclarationId` is a hash of the name, so this builds the key without a lookup — but the
/// lookup still has to happen, because an id for a declaration that was never indexed is a
/// perfectly well-formed id that answers nothing.
fn declared(graph: &Graph, name: &str) -> Option<DeclarationId> {
    let id = DeclarationId::from(name);
    graph.declarations().contains_key(&id).then_some(id)
}

fn singleton_of(graph: &Graph, id: DeclarationId) -> Option<DeclarationId> {
    match graph.declarations().get(&id)? {
        Declaration::Namespace(namespace) => namespace.singleton_class().copied(),
        _ => None,
    }
}

/// Everything rubydex offers for a receiver, filtered to the prefix and capped.
fn from_graph(
    graph: &Graph,
    receiver: CompletionReceiver,
    only: Only,
    limit: usize,
    ranking: &Ranking,
) -> Vec<Item> {
    let candidates = match query::completion_candidates(graph, CompletionContext::new(receiver)) {
        Ok(candidates) => candidates,
        Err(error) => {
            // A receiver that is not a namespace after all. Nothing to say, and nothing broken.
            tracing::debug!("no completion candidates: {error}");
            return Vec::new();
        }
    };

    let mut ranked: Vec<Ranked> = candidates
        .iter()
        .filter(|candidate| only.accepts(graph, candidate))
        .enumerate()
        .filter_map(|(sequence, candidate)| rank(graph, candidate, sequence, ranking))
        .collect();
    take_best(&mut ranked, limit);
    ranked.into_iter().map(|entry| entry.item).collect()
}

impl Only {
    fn accepts(self, graph: &Graph, candidate: &CompletionCandidate) -> bool {
        match self {
            Only::Everything => true,
            Only::Constants => match candidate {
                CompletionCandidate::Declaration(id) => matches!(
                    graph.declarations().get(id),
                    Some(
                        Declaration::Namespace(_)
                            | Declaration::Constant(_)
                            | Declaration::ConstantAlias(_)
                    )
                ),
                _ => false,
            },
        }
    }
}

/// How far a namespace sits from the receiver, in steps along the chains rubydex walked.
///
/// This is the ranking's only term for *relevance*, and without it a list that nothing has been
/// typed into is not ranked at all: `tier` is 1 for every row, `length` is 0 for every row, and
/// what decides is the label, alphabetically. Measured, `"hello".` opened on `DelegateClass,
/// Digest, append_as_bytes, ascii_only?, b, begin, …` — two of the first three are not `String`'s
/// and one of those two is not a method.
///
/// Zero is the receiver's own members, one an included module's, and `Object`, `Kernel` and
/// `BasicObject` come last because they are the end of every chain. That is also the argument for
/// doing this before anything about `Object` being the drain that everything rubydex cannot
/// attribute falls into: whatever is misfiled there is already as far away as a name can be.
struct Distance {
    steps: HashMap<DeclarationId, u16>,
}

/// What an owner that none of the chains reached is worth.
///
/// Last, and equally last, so the rest of the key still separates them. Every candidate rubydex
/// collects comes off one of the chains seeded below, so landing here means the graph disagrees
/// with itself about who owns a name — not a case to give the benefit of the doubt to.
const NO_DISTANCE: u16 = u16::MAX;

impl Distance {
    /// The name-based list has no receiver, so no chain, so nothing to measure. Every row ties
    /// and the rest of the key decides it, exactly as before.
    fn none() -> Self {
        Self {
            steps: HashMap::new(),
        }
    }

    /// Seed from the same walks `query::completion_candidates` is about to make.
    ///
    /// Each walk is numbered from zero rather than end to end, because they are different kinds
    /// of nearness sharing one scale: a sibling constant in the enclosing module is close to the
    /// cursor and is on nobody's ancestor chain, and `Foo::` reaches `Foo::Bar` and `Foo.build`
    /// by two routes that both start at `Foo`. Numbering them end to end would sink every method
    /// in an expression below every constant, or the reverse.
    ///
    /// Ancestor chains are seeded first and the nearest of them wins. The lexical walk only
    /// fills in what they never reached, because it is a shortcut to the same place and taking
    /// it would be a lie about a method: `Object` is the last rung of every ancestor chain *and*
    /// the outermost lexical scope, so letting it be scored as the latter would put `Object`'s
    /// members — which is everything rubydex could not attribute — one step from the cursor.
    fn from_receiver(graph: &Graph, receiver: &CompletionReceiver) -> Self {
        let mut distance = Self::none();
        let mut lexical = None;
        match receiver {
            CompletionReceiver::MethodCall {
                receiver_decl_id, ..
            } => distance.chain(graph, *receiver_decl_id),
            CompletionReceiver::NamespaceAccess {
                namespace_decl_id, ..
            } => {
                distance.chain(graph, *namespace_decl_id);
                if let Some(namespace) = namespace_id(graph, *namespace_decl_id)
                    && let Some(singleton) = singleton_of(graph, namespace)
                {
                    distance.chain(graph, singleton);
                }
            }
            CompletionReceiver::Expression {
                self_decl_id,
                nesting_name_id,
            }
            | CompletionReceiver::MethodArgument {
                self_decl_id,
                nesting_name_id,
                ..
            } => {
                let nesting = graph.name_id_to_declaration_id(*nesting_name_id).copied();
                // Methods and instance variables come off `self`'s ancestors; constants off the
                // lexical nesting, which is a walk outwards through owners and not through
                // ancestors. Both are seeded, so a class's own methods and the constants sitting
                // beside it in its module are both near.
                if let Some(id) = self_decl_id.or(nesting) {
                    distance.chain(graph, id);
                }
                if let Some(id) = nesting {
                    distance.chain(graph, id);
                    lexical = Some(id);
                }
            }
        }
        // After every ancestor chain, never before one: see above.
        if let Some(id) = lexical {
            distance.lexical(graph, id);
        }
        distance
    }

    /// Number one linearized ancestor chain, outwards from the receiver.
    fn chain(&mut self, graph: &Graph, id: DeclarationId) {
        let Some((_, namespace)) = namespace(graph, id) else {
            return;
        };
        for (step, ancestor) in namespace.ancestors().iter().enumerate() {
            // A rung rubydex could not linearize is still a rung: whatever sits past it is
            // further away whether or not this one can be named.
            if let Ancestor::Complete(ancestor_id) = ancestor {
                self.record(*ancestor_id, step);
            }
        }
    }

    /// Number the lexical nesting outwards: `Billing::Invoice`, then `Billing`, then `Object`.
    ///
    /// rubydex's own invariant is that `Object` and `BasicObject` are the only declarations that
    /// own themselves, so self-ownership is what ends the walk. The bound is there because an
    /// invariant that is checked is not an invariant that is enforced, and rubydex bounds its
    /// own owner walks the same way.
    fn lexical(&mut self, graph: &Graph, id: DeclarationId) {
        const MAX_NESTING: usize = 64;

        let mut current = id;
        for step in 0..MAX_NESTING {
            self.fill(current, step);
            let Some(declaration) = graph.declarations().get(&current) else {
                return;
            };
            let owner = *declaration.owner_id();
            if owner == current {
                return;
            }
            current = owner;
        }
    }

    /// The nearest of the ancestor chains is the one that counts.
    fn record(&mut self, id: DeclarationId, step: usize) {
        let step = Self::bounded(step);
        self.steps
            .entry(id)
            .and_modify(|held| *held = (*held).min(step))
            .or_insert(step);
    }

    /// A number for a namespace no ancestor chain reached, and only for one.
    fn fill(&mut self, id: DeclarationId, step: usize) {
        self.steps.entry(id).or_insert(Self::bounded(step));
    }

    fn bounded(step: usize) -> u16 {
        u16::try_from(step).unwrap_or(NO_DISTANCE)
    }

    /// How far the namespace that declares this sits from the receiver.
    ///
    /// `owner_id` is rubydex's own back-pointer to the namespace a member was collected from,
    /// which is why nothing here parses a name: `Foo::<Foo>#build()` and a top-level constant
    /// both answer without a special case.
    fn of(&self, declaration: &Declaration) -> u16 {
        self.steps
            .get(declaration.owner_id())
            .copied()
            .unwrap_or(NO_DISTANCE)
    }
}

/// How near a declaration sits to the cursor itself, counted in directories.
///
/// [`Distance`] measures nearness along a receiver's ancestor chain, and there is no chain when
/// the receiver is an instance variable or another call's return value. That list is every method
/// name in the project, and on a Rails app it opened on `account_type, add_row, amount,
/// attributes, balance` — 512 rows chosen by the alphabet, in 50 ms, telling the user nothing.
///
/// So this is the same question asked of what *is* known: not what the receiver is, but where the
/// cursor is. It ranks the typed list too, where it breaks ties `Distance` leaves — every
/// top-level constant in a project sits at the same depth on the same chain, and the alphabet was
/// deciding between them.
///
/// **The measure is the path, not the namespace, and that was decided by trying both.** A walk
/// outwards through the cursor's lexical nesting reads like the more principled answer and works
/// well on namespaced code — inside `Finance::BankDetails::BankAccounts::Decorator` it found the
/// sibling service, then the cousins under `Finance::BankDetails`. But a Rails model is
/// `class Message < ApplicationRecord` at the top level, so its nesting is empty, and half of a
/// real app got nothing at all. Ruby projects put related code in the same directory whether or
/// not they nest it, and Zeitwerk makes the directory *be* the namespace, so the path carries
/// everything the nesting carried and answers for flat code as well.
struct Locality {
    /// Every document of the user's own code, and how far its directory sits from the cursor's:
    /// 0 the file itself, 1 its directory or below, 2 the parent, and so on outwards.
    ///
    /// Gems are absent rather than far. They are already below the user's own code on `group`,
    /// and scoring six thousand documents nobody asked about is work for no answer.
    documents: HashMap<UriId, u8>,
}

/// What a declaration in none of those documents is worth: nothing, and equally nothing, so the
/// rest of the key still separates them.
const NO_LOCALITY: u8 = u8::MAX;

impl Locality {
    /// Built once per request, from the user's own documents rather than from the whole graph —
    /// 161 path comparisons on a real Rails app, against 7,000.
    fn at(graph: &Graph, here: UriId, own: &HashSet<UriId>) -> Self {
        let mut documents = HashMap::new();
        let Some(cursor) = graph.documents().get(&here).map(|document| document.uri()) else {
            return Self { documents };
        };
        let cursor = directory_of(cursor);
        let depth = segments(cursor);

        // `filter_map`, the way `of` below reads the same table: `own` is built from the
        // graph's own documents, so a miss has nothing to be done about it.
        for (id, document) in own
            .iter()
            .filter_map(|id| Some((*id, graph.documents().get(id)?)))
        {
            let step = if id == here {
                0
            } else {
                let shared = shared_segments(cursor, directory_of(document.uri()));
                u8::try_from(depth - shared + 1).unwrap_or(NO_LOCALITY)
            };
            documents.insert(id, step);
        }
        Self { documents }
    }

    /// The nearest document the declaration was written in.
    ///
    /// The nearest, because a class reopened in two places — a Rails concern, a monkey patch —
    /// should be scored by the copy the cursor can see, not by whichever definition rubydex
    /// happened to record first.
    fn of(&self, graph: &Graph, declaration: &Declaration) -> u8 {
        declaration
            .definitions()
            .iter()
            .filter_map(|id| graph.definitions().get(id))
            .filter_map(|definition| self.documents.get(definition.uri_id()))
            .copied()
            .min()
            .unwrap_or(NO_LOCALITY)
    }
}

/// A URI without its last segment. Both sides come from `Url::from_file_path`, so they are
/// already canonical and comparing them as text is comparing paths.
fn directory_of(uri: &str) -> &str {
    uri.rsplit_once('/').map_or(uri, |(directory, _)| directory)
}

fn segments(uri: &str) -> usize {
    uri.split('/').count()
}

/// How many leading path segments two directories share.
///
/// Segment-wise rather than byte-wise, or `app/model` would count as sharing all of `app/models`.
fn shared_segments(left: &str, right: &str) -> usize {
    left.split('/')
        .zip(right.split('/'))
        .take_while(|(left, right)| left == right)
        .count()
}

/// The namespace a receiver id names, following a constant alias the way rubydex's own walk
/// does — `Money = Billing::Money` offers what it points at.
fn namespace_id(graph: &Graph, id: DeclarationId) -> Option<DeclarationId> {
    namespace(graph, id).map(|(id, _)| id)
}

/// The same, with the declaration it had to look up anyway.
///
/// `chain` runs this once per ancestor of every candidate's receiver, and looking the id back up
/// to re-establish what this already proved was a second hash lookup on that path — and a
/// `Declaration` that was known to be a `Namespace` re-checked as though it might not be.
fn namespace(graph: &Graph, id: DeclarationId) -> Option<(DeclarationId, &Namespace)> {
    match graph.declarations().get(&id)? {
        Declaration::Namespace(namespace) => Some((id, namespace)),
        _ => {
            let target = graph.resolve_alias(&id)?;
            match graph.declarations().get(&target) {
                Some(Declaration::Namespace(namespace)) => Some((target, namespace)),
                _ => None,
            }
        }
    }
}

/// The degraded list: every method name in the project, for a receiver with no type.
///
/// Deduplicated by name, because `name` is defined by hundreds of classes in a Rails bundle and
/// hundreds of identical rows is not a completion list. The user's own code wins the duplicate,
/// so the detail line names a file they can actually go and read.
///
/// Deduplication happens *before* the sort, and on a hash of the label rather than the label:
/// this runs over every method declaration in the graph on every keystroke, so a copy of each
/// name would be a hundred thousand allocations to throw away.
fn by_name(graph: &Graph, limit: usize, ranking: &Ranking) -> Vec<Item> {
    // Every method name contains a `#` and no other declaration's does, so this is rubydex's
    // parallel filter doing the "methods only" pass for free.
    let query = format!("#{}", ranking.prefix);
    let mut best: HashMap<u64, Ranked> = HashMap::new();

    for id in query::declaration_search(graph, &query, &MatchMode::Fuzzy) {
        // The query leads with `#`, which only a method name contains, so this is rubydex's
        // parallel filter having already done the "methods only" pass — bound rather than
        // re-tested, since a second `matches!` over every method in the graph proved nothing.
        let Some(declaration @ Declaration::Method(_)) = graph.declarations().get(&id) else {
            continue;
        };
        let Some(entry) = ranked_declaration(graph, id, declaration, ranking) else {
            continue;
        };
        let key = StringId::from(&entry.item.label).get();
        match best.entry(key) {
            Entry::Occupied(mut held) => {
                if order(&entry, held.get()) == std::cmp::Ordering::Less {
                    held.insert(entry);
                }
            }
            Entry::Vacant(slot) => {
                slot.insert(entry);
            }
        }
    }

    let mut ranked: Vec<Ranked> = best.into_values().collect();
    take_best(&mut ranked, limit);
    ranked.into_iter().map(|entry| entry.item).collect()
}

/// Everything the ranking needs that is not the candidate itself.
///
/// A struct rather than another parameter each time, because `ranked_declaration` is the one
/// point both the receiver path and the name-based path pass through: every term the ranking
/// grows lands here, and the two paths cannot drift apart on one.
struct Ranking<'a> {
    prefix: &'a str,
    distance: Distance,
    locality: Locality,
    /// Whether Ruby would let a private method be written at the cursor. Decided from the
    /// syntax alone by [`cursor::Context::allows_private`].
    private_ok: bool,
}

/// The five method names Ruby keeps private however they were declared.
///
/// `rb_add_method` privatises them by name at the point of definition, so a `def initialize` is
/// private whatever its class did or did not say — and neither rbs nor rubydex records that.
/// Measured against Ruby 4.0.1: these five and nothing else. `method_missing` and
/// `singleton_method_added` look like they belong here and do not; they are private on
/// `BasicObject` because that is how *those* copies were written, which the graph already knows.
///
/// Instance methods only. The same code leaves `def self.initialize` public, and `Bar.initialize`
/// really does call it — which is why [`reachable`] checks who owns the method before applying
/// this. `Class#initialize` is caught anyway, because it is an instance method of `Class`.
const ALWAYS_PRIVATE: [&str; 5] = [
    "initialize",
    "initialize_clone",
    "initialize_copy",
    "initialize_dup",
    "respond_to_missing?",
];

/// Whether Ruby would let this method be called where the cursor is.
///
/// Everything reaching here has already passed rubydex's visibility filter, which answers a
/// slightly different question than Ruby asks. Two gaps follow, and they close together:
///
/// - **rubydex passes a private method whenever the caller's `self` is the same class as the
///   receiver.** Ruby's exemption is for a receiver *written* `self`, so `Vault.new.secret` was
///   offered from inside `Vault`, where a real interpreter raises `NoMethodError`.
/// - **The graph believes rbs about the five names above**, and rbs is not consistent about
///   them: `Kernel#initialize_copy` is marked private and `String#initialize_copy` public, so
///   `"hi".` offered `initialize` and `initialize_copy` while `count.` offered only the first.
///
/// Both are the same question — may this be written *here* — and `private_ok` is the answer.
fn reachable(graph: &Graph, id: DeclarationId, declaration: &Declaration, label: &str) -> bool {
    if !matches!(declaration, Declaration::Method(_)) {
        return true;
    }
    if matches!(
        graph.visibility(&id),
        // rubydex treats `module_function`'s instance copy as private, and so does Ruby.
        Some(Visibility::Private | Visibility::ModuleFunction)
    ) {
        return false;
    }
    !ALWAYS_PRIVATE.contains(&label) || singleton_owned(graph, declaration)
}

/// Whether a method hangs off a singleton class, which is how rubydex spells `def self.foo`.
fn singleton_owned(graph: &Graph, declaration: &Declaration) -> bool {
    matches!(
        graph.declarations().get(declaration.owner_id()),
        Some(Declaration::Namespace(Namespace::SingletonClass(_)))
    )
}

struct Ranked {
    group: u8,
    tier: u8,
    /// Whether the name is spelled as an internal one — `_fork`, `__send`.
    internal: bool,
    /// How far the namespace declaring this sits from the receiver. See [`Distance`].
    distance: u16,
    /// How near the declaration itself sits to the cursor. See [`Locality`].
    locality: u8,
    /// Where a keyword argument sits in the signature that declares it, and zero for everything
    /// else.
    ///
    /// It is only ever a signature's own order. rubydex's emission order looks like it would
    /// serve every candidate — the ancestor chain is walked outwards, so an earlier row is
    /// declared nearer — but *within* one namespace that order is a hash map's, and adding a
    /// member reshuffles it. A completion list that rearranges itself as the file is edited is
    /// worse than one that is merely alphabetical. [`Distance`] takes the half of the emission
    /// order that is stable and leaves the half that is not. A signature is a list, so its order
    /// is real and worth keeping.
    sequence: usize,
    /// The label's length, or zero when nothing has been typed yet.
    ///
    /// Shortest-first is a good tiebreak among rows that all matched a prefix and a poor one
    /// among rows that matched nothing in particular: `Foo::` should read alphabetically, not
    /// shortest-name-first.
    length: usize,
    item: Item,
}

/// The user's own code above a gem's, an exact prefix above a fuzzy one, the shorter name first.
///
/// A name beginning with an underscore sinks: Ruby spells "you were not meant to call this" that
/// way, and without the rule `User.` opens on `__send`, `_fork` and `_load_from_sql` because
/// underscores sort before letters. Typing one lifts them back — someone who writes `_` means it.
///
/// Keyword arguments lead because they are the only suggestion that can be *wrong* to leave out:
/// in `build(` the parameter names are the answer to the question and everything else is
/// background. Ruby keywords sit above a gem's declarations for the same reason `end` is more
/// likely than `Encoding` — but below the user's own code, which is the same call the symbol
/// picker makes.
///
/// [`Distance`] sits below match quality and above everything else. What the user typed is what
/// they asked for, so it outranks where a name lives; among rows that match it equally, the
/// nearer owner wins. With nothing typed yet every row ties on quality, and distance is then the
/// only thing ranking the list at all.
///
/// [`Locality`] follows it, and the pair is one question — how near is this — asked of two
/// different things. Distance is the stronger answer and goes first, but it is silent exactly
/// twice: where there is no receiver to measure a chain from, and among the rows of one chain
/// that tie. Locality speaks in both.
fn order(a: &Ranked, b: &Ranked) -> std::cmp::Ordering {
    a.group
        .cmp(&b.group)
        .then(a.internal.cmp(&b.internal))
        .then(b.tier.cmp(&a.tier))
        .then(a.distance.cmp(&b.distance))
        .then(a.locality.cmp(&b.locality))
        .then(a.length.cmp(&b.length))
        .then(a.sequence.cmp(&b.sequence))
        .then(a.item.label.cmp(&b.item.label))
}

fn take_best(ranked: &mut Vec<Ranked>, limit: usize) {
    if ranked.len() > limit {
        ranked.select_nth_unstable_by(limit, order);
        ranked.truncate(limit);
    }
    ranked.sort_unstable_by(order);
}

fn rank(
    graph: &Graph,
    candidate: &CompletionCandidate,
    sequence: usize,
    ranking: &Ranking,
) -> Option<Ranked> {
    let prefix = ranking.prefix;
    match candidate {
        CompletionCandidate::Declaration(id) => {
            let declaration = graph.declarations().get(id)?;
            ranked_declaration(graph, *id, declaration, ranking)
        }
        CompletionCandidate::KeywordArgument(str_id) => {
            let name = graph.strings().get(str_id)?.as_str();
            let label = format!("{name}:");
            let tier = tier(prefix, name)?;
            Some(Ranked {
                group: 0,
                tier,
                internal: is_internal(prefix, &label),
                // Neither of these is anywhere in particular, and `group` keeps both out of
                // any comparison where that would matter: keyword arguments lead the list and
                // Ruby's keywords are a band of their own.
                distance: 0,
                locality: 0,
                sequence,
                length: sort_length(prefix, &label),
                item: Item {
                    label,
                    detail: Some("keyword argument".to_owned()),
                    kind: Kind::Field,
                    documentation: None,
                    deprecated: false,
                    declaration: None,
                },
            })
        }
        CompletionCandidate::Keyword(keyword) => Some(Ranked {
            group: 2,
            tier: tier(prefix, keyword.name())?,
            internal: false,
            distance: 0,
            locality: 0,
            sequence: 0,
            length: sort_length(prefix, keyword.name()),
            item: Item {
                label: keyword.name().to_owned(),
                detail: Some("Ruby keyword".to_owned()),
                kind: Kind::Keyword,
                documentation: Some(keyword.documentation().to_owned()),
                deprecated: false,
                declaration: None,
            },
        }),
    }
}

fn ranked_declaration(
    graph: &Graph,
    id: DeclarationId,
    declaration: &Declaration,
    ranking: &Ranking,
) -> Option<Ranked> {
    let prefix = ranking.prefix;
    // A `Todo` namespace is a placeholder the resolver invented for a parent it never saw, so
    // it has no definition to jump to and no members to offer.
    if matches!(declaration, Declaration::Namespace(Namespace::Todo(_))) {
        return None;
    }
    let name = declaration.name();
    // A singleton class and an anonymous `Class.new` both have names rubydex invented. The
    // singleton's *methods* are still offered, spelled the way its class calls them.
    if !render::is_nameable(name) {
        return None;
    }
    let label = render::last_segment(name);
    // After `tier`, deliberately. This runs once per candidate and there can be a hundred
    // thousand of them, and `reachable` costs a visibility lookup where the prefix test costs a
    // string compare — so the cheap filter goes first and most rows never reach the expensive
    // one. `private_ok` short-circuits the whole thing wherever a receiver was not written.
    let tier = tier(prefix, label)?;
    if !ranking.private_ok && !reachable(graph, id, declaration, label) {
        return None;
    }

    // One walk over the definitions, not two. `Locality` is built from exactly the documents
    // that are the user's own code, so "is this theirs" is "did any document score at all" —
    // and this runs over every method declaration in the graph on the name-based path.
    let locality = ranking.locality.of(graph, declaration);

    Some(Ranked {
        group: if locality == NO_LOCALITY { 3 } else { 1 },
        tier,
        internal: is_internal(prefix, label),
        distance: ranking.distance.of(declaration),
        locality,
        sequence: 0,
        length: sort_length(prefix, label),
        item: Item {
            label: label.to_owned(),
            detail: Some(render::qualified_name(name)),
            kind: kind_of(declaration),
            documentation: None,
            deprecated: false,
            declaration: Some(id),
        },
    })
}

/// A name nobody reaches by typing its first character: `_internal`, `!`, `<=>`, `[]`, `$0`.
///
/// Punctuation and underscores sort before letters, so without this the first thing a large
/// project offers after a `.` is `!`, `%`, `&` and `__send`. A sigil does not count as
/// punctuation — `@name` is a name — so it is stepped over before the test.
///
/// Typing one of these characters lifts them all back: somebody who writes `_` means it.
fn is_internal(prefix: &str, label: &str) -> bool {
    let Some(first) = significant(label) else {
        return true;
    };
    if first.is_alphabetic() {
        return false;
    }
    significant(prefix).is_none_or(char::is_alphabetic)
}

/// The leading run of sigil characters: `@`, `@@`, `$`, or nothing at all.
///
/// `$@` is the awkward one and the reason this is a run rather than a first character: every
/// character in it is a sigil character, so the whole name comes back — which is the right
/// answer for the only question asked of it, since nothing but a `$` prefix should reach it.
fn sigils(name: &str) -> &str {
    &name[..name.len() - name.trim_start_matches(['@', '$']).len()]
}

/// The first character of a name that is not its sigil.
fn significant(name: &str) -> Option<char> {
    name[sigils(name).len()..].chars().next()
}

fn sort_length(prefix: &str, label: &str) -> usize {
    if prefix.is_empty() { 0 } else { label.len() }
}

/// How well `prefix` matches `label`, or `None` when it does not match at all.
///
/// The floor is a case-insensitive subsequence, which is what editors fuzzy-match with — being
/// stricter here would drop rows the client would have been happy to show.
fn tier(prefix: &str, label: &str) -> Option<u8> {
    // Except for the sigil, which is not a letter to fuzzy-match on. `@foo`, `@@foo` and `$foo`
    // are three different namespaces in Ruby, and a name in one of them is not a candidate for
    // a prefix in another — so what the prefix asked for has to be what the label carries.
    //
    // `starts_with` rather than equality, because `@` is genuinely on the way to `@@`: someone
    // who has typed one `@` may be about to type the second, and dropping the class variables
    // there would be the same mistake in the other direction. It does not run backwards — a
    // prefix of `@@` admits no `@name`.
    //
    // Without this, `@` offers `$@`: the subsequence match below reads the sigil as one more
    // character, and `$@` contains an `@`. Carried through two releases because it is a row at
    // the bottom of a list rather than a wrong answer at the top.
    if !sigils(label).starts_with(sigils(prefix)) {
        return None;
    }
    if prefix.is_empty() {
        return Some(1);
    }
    if equal_ci(label, prefix) {
        return Some(4);
    }
    if starts_with_ci(label, prefix) {
        return Some(3);
    }
    if contains_ci(label, prefix) {
        return Some(2);
    }
    subsequence_ci(label, prefix).then_some(0)
}

/// What icon the editor draws.
///
/// Read off the *declaration*, not a definition: the outline can tell an `attr_reader` from a
/// `def` because it is looking at one definition, but here there are hundreds of rows on every
/// keystroke and rubydex files both as methods anyway. Paying a definition lookup per row to
/// change one icon is not a trade worth making.
fn kind_of(declaration: &Declaration) -> Kind {
    match declaration {
        Declaration::Namespace(Namespace::Module(_)) => Kind::Module,
        Declaration::Namespace(_) => Kind::Class,
        Declaration::Constant(_) | Declaration::ConstantAlias(_) => Kind::Constant,
        Declaration::Method(_) => Kind::Method,
        Declaration::InstanceVariable(_) | Declaration::ClassVariable(_) => Kind::Field,
        Declaration::GlobalVariable(_) => Kind::Variable,
    }
}

fn wider(held: &rubydex::offset::Offset, candidate: &rubydex::offset::Offset) -> bool {
    held.end() - held.start() > candidate.end() - candidate.start()
}

fn equal_ci(left: &str, right: &str) -> bool {
    left.len() == right.len() && starts_with_ci(left, right)
}

/// Case-insensitive `starts_with` without allocating a lowercased copy, because this runs once
/// per candidate and there can be a hundred thousand of them.
fn starts_with_ci(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|wanted| chars.next().is_some_and(|found| eq_ci(found, wanted)))
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack
        .char_indices()
        .any(|(index, _)| starts_with_ci(&haystack[index..], needle))
}

fn subsequence_ci(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|wanted| chars.any(|found| eq_ci(found, wanted)))
}

fn eq_ci(left: char, right: char) -> bool {
    if left.is_ascii() || right.is_ascii() {
        return left.eq_ignore_ascii_case(&right);
    }
    left == right || left.to_lowercase().eq(right.to_lowercase())
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// An id, uri or name that no graph holds — what a client sends after a config reload
    /// dropped the graph the list it is looking at was built from.
    #[test]
    fn nothing_in_an_empty_graph_is_near_the_cursor_or_encloses_it() {
        // Every one of these takes an id from outside — the request's uri, the `data` a client
        // echoes back on `completionItem/resolve` — and rubydex ids are hashes, so "the graph
        // does not hold this" is a state the server is handed rather than one it creates. The
        // whole ranking has to degrade to "nothing" rather than to a panic or a wrong answer.
        let graph = Graph::new();
        let missing = UriId::from("file:///nowhere/gone.rb");

        let scope = Scope::at(&graph, missing, 0);
        assert_eq!(
            scope.nesting,
            object_name(),
            "the top level, for want of one"
        );
        assert!(scope.self_id.is_none());

        let locality = Locality::at(&graph, missing, &HashSet::new());
        assert!(
            locality.documents.is_empty(),
            "no cursor document, so nothing to measure nearness against"
        );

        // `Object` is in every real graph — rubydex indexes a built-in one — so this is the
        // only way to reach a nesting that resolves to no declaration at all.
        let distance = Distance::from_receiver(
            &graph,
            &CompletionReceiver::Expression {
                self_decl_id: None,
                nesting_name_id: object_name(),
            },
        );
        assert!(
            distance.steps.is_empty(),
            "no nesting to resolve, so no chain to number"
        );
    }

    #[test]
    fn a_name_nobody_reaches_by_typing_its_first_letter_sinks() {
        // Punctuation and underscores sort before letters, so without this a Rails app opens
        // `User.` on `__send`, `_fork`, `!` and `%`.
        for label in ["_internal", "__send", "!", "<=>", "[]", "%"] {
            assert!(
                is_internal("", label),
                "{label} should sink at an empty prefix"
            );
        }
        // A sigil is not punctuation — `@name` is a name.
        for label in ["@name", "$stdout", "shout", "Person"] {
            assert!(!is_internal("", label), "{label} should not sink");
        }
        // Typing one of these characters lifts them all back: somebody who writes `_` means it.
        assert!(!is_internal("_", "_internal"));
        assert!(!is_internal("<", "<=>"));
        assert!(
            !is_internal("@_", "@_hidden"),
            "the sigil is stepped over first"
        );
        // A label with nothing left after the sigils has no first character to judge.
        assert!(is_internal("", ""));
        assert!(is_internal("", "@"));
    }

    #[test]
    fn a_prefix_matches_as_a_subsequence_case_insensitively() {
        // What a client filters on between keystrokes, and why every list is `isIncomplete`.
        assert!(subsequence_ci("ApplicationRecord", "aprec"));
        assert!(subsequence_ci("shout", "SHOUT"));
        assert!(
            subsequence_ci("anything", ""),
            "an empty prefix matches all"
        );
        assert!(!subsequence_ci("shout", "shouted"));
        assert!(!subsequence_ci("shout", "tuohs"), "order matters");
    }

    #[test]
    fn non_ascii_identifiers_fold_by_unicode_rather_than_by_byte() {
        // Ruby allows them. ASCII on either side takes the fast path; two non-ASCII characters
        // need the full lowercase mapping, which is the only route through `eq_ci`'s last line.
        assert!(eq_ci('Ä', 'ä'));
        assert!(
            eq_ci('Ä', 'Ä'),
            "the same character needs no mapping at all"
        );
        assert!(eq_ci('П', 'п'));
        assert!(eq_ci('A', 'a'));
        assert!(!eq_ci('Ä', 'a'), "not a transliteration");
        assert!(!eq_ci('ä', 'a'));
        assert!(!eq_ci('П', 'р'));
    }
}
