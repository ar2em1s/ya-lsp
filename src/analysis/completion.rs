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
        declaration::{Declaration, Namespace},
        definitions::{Definition, Receiver as DefinitionReceiver},
        graph::Graph,
        ids::{DeclarationId, NameId, StringId, UriId},
        name::{Name, ParentScope},
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
    let items = match receiver_for(graph, uri_id, cursor.context, &scope) {
        Some((receiver, only)) => from_graph(graph, receiver, only, prefix, limit, own),
        // Only one context arrives here with anything worth saying: a `.` on a receiver whose
        // type is unknown. `foo::` and a receiver that is not a namespace have no honest answer.
        None => match cursor.context {
            Context::MethodCall { .. } => by_name(graph, prefix, limit, own),
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
        for id in document.definitions() {
            let Some(definition) = graph.definitions().get(id) else {
                continue;
            };
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
            let receiver = match precise_call(graph, uri_id, name) {
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
                // `self::CONST` is legal and rare; the nesting is what it means.
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

/// The method a call written at `offset` resolves to, only when the resolution was exact.
fn precise_call(graph: &Graph, uri_id: UriId, offset: u32) -> Option<DeclarationId> {
    locator::locate(graph, uri_id, offset)
        .into_iter()
        .find_map(|located| match located.target {
            locator::Target::Call(_) => {
                let resolution = locator::resolve(graph, &located);
                resolution
                    .precise
                    .then(|| resolution.declarations.into_iter().next())
                    .flatten()
            }
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
    prefix: &str,
    limit: usize,
    own: &HashSet<UriId>,
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
        .filter_map(|(sequence, candidate)| rank(graph, candidate, prefix, sequence, own))
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

/// The degraded list: every method name in the project, for a receiver with no type.
///
/// Deduplicated by name, because `name` is defined by hundreds of classes in a Rails bundle and
/// hundreds of identical rows is not a completion list. The user's own code wins the duplicate,
/// so the detail line names a file they can actually go and read.
///
/// Deduplication happens *before* the sort, and on a hash of the label rather than the label:
/// this runs over every method declaration in the graph on every keystroke, so a copy of each
/// name would be a hundred thousand allocations to throw away.
fn by_name(graph: &Graph, prefix: &str, limit: usize, own: &HashSet<UriId>) -> Vec<Item> {
    // Every method name contains a `#` and no other declaration's does, so this is rubydex's
    // parallel filter doing the "methods only" pass for free.
    let query = format!("#{prefix}");
    let mut best: HashMap<u64, Ranked> = HashMap::new();

    for id in query::declaration_search(graph, &query, &MatchMode::Fuzzy) {
        let Some(declaration) = graph.declarations().get(&id) else {
            continue;
        };
        if !matches!(declaration, Declaration::Method(_)) {
            continue;
        }
        let Some(entry) = ranked_declaration(graph, id, declaration, prefix, own) else {
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

struct Ranked {
    group: u8,
    tier: u8,
    /// Whether the name is spelled as an internal one — `_fork`, `__send`.
    internal: bool,
    /// Where a keyword argument sits in the signature that declares it, and zero for everything
    /// else.
    ///
    /// It is tempting to use the emission order for all candidates — rubydex walks the ancestor
    /// chain outwards, so an earlier one is defined closer to the receiver — but *within* one
    /// namespace the order is a hash map's, and adding a member reshuffles it. A completion list
    /// that reorders itself as the file is edited is worse than one that is merely alphabetical.
    /// A signature is a list, so its order is real and worth keeping.
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
fn order(a: &Ranked, b: &Ranked) -> std::cmp::Ordering {
    a.group
        .cmp(&b.group)
        .then(a.internal.cmp(&b.internal))
        .then(b.tier.cmp(&a.tier))
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
    prefix: &str,
    sequence: usize,
    own: &HashSet<UriId>,
) -> Option<Ranked> {
    match candidate {
        CompletionCandidate::Declaration(id) => {
            let declaration = graph.declarations().get(id)?;
            ranked_declaration(graph, *id, declaration, prefix, own)
        }
        CompletionCandidate::KeywordArgument(str_id) => {
            let name = graph.strings().get(str_id)?.as_str();
            let label = format!("{name}:");
            let tier = tier(prefix, name)?;
            Some(Ranked {
                group: 0,
                tier,
                internal: is_internal(prefix, &label),
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
    prefix: &str,
    own: &HashSet<UriId>,
) -> Option<Ranked> {
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
    let tier = tier(prefix, label)?;

    Some(Ranked {
        group: if declared_in(graph, declaration, own) {
            1
        } else {
            3
        },
        tier,
        internal: is_internal(prefix, label),
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

/// The first character of a name that is not its sigil.
fn significant(name: &str) -> Option<char> {
    name.trim_start_matches(['@', '$']).chars().next()
}

fn sort_length(prefix: &str, label: &str) -> usize {
    if prefix.is_empty() { 0 } else { label.len() }
}

/// How well `prefix` matches `label`, or `None` when it does not match at all.
///
/// The floor is a case-insensitive subsequence, which is what editors fuzzy-match with — being
/// stricter here would drop rows the client would have been happy to show.
fn tier(prefix: &str, label: &str) -> Option<u8> {
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

fn declared_in(graph: &Graph, declaration: &Declaration, own: &HashSet<UriId>) -> bool {
    declaration.definitions().iter().any(|id| {
        graph
            .definitions()
            .get(id)
            .is_some_and(|definition| own.contains(definition.uri_id()))
    })
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
