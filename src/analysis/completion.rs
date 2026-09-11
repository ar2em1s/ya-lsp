//! `textDocument/completion` — what can be written where the cursor is.
//!
//! [`cursor`] says what *shape* the cursor is in; this says what the graph has to offer there.
//! The split matters because the shape is pure syntax and the offer is pure semantics, and only
//! one of them needs a project loaded to test.
//!
//! # What is exact and what is a guess
//!
//! Three of the four contexts are exact, because in each the receiver is something the graph
//! resolved: `Foo::`, `Foo.`, `self.`, and a bare word (whose receiver is the enclosing `self`).
//! rubydex walks the real ancestor chain, applies real visibility — a `private` method is offered
//! inside the class and not outside it — and for an argument list hands back the called method's
//! keyword parameters.
//!
//! The fourth is `foo.` where `foo` is a local, an instance variable, or the result of another
//! call. [`types`](super::types) answers what it can there, and where every rung comes back empty
//! one is left: the receiver's own spelling, read as a class name. That list is a real class's
//! members and is offered as one, with [`Completion::guess`] saying which letters it came from.
//!
//! When even that finds nothing, the list falls back to every method name in the project — a
//! guess of a different kind, presented as one: names only, deduplicated, the user's own code
//! first. It still beats the editor's word-list, which cannot see a method defined in a file that
//! is not open.
//!
//! **The two guesses are not the same and a row must not conflate them.** `precise` says whether
//! the rows are one class's members; `guess` says whether that class was inferred from a name. A
//! guessed receiver is `precise` *and* guessed — a better list than the name-based one and a
//! worse answer than a resolved type — and the card has to be able to say both.
//!
//! # When the list is `isIncomplete`
//!
//! Completion is filtered here rather than in the client, because the alternative is shipping a
//! Rails bundle's hundred thousand candidates on the first keystroke. Filtering server-side means
//! the answer is only correct for the prefix it was asked with, and `isIncomplete` is what tells
//! the client to ask again rather than narrow what it already has.
//!
//! **It is set when the cap dropped rows, and not on every list.** Dropping a row is the only way
//! this answer can fail to hold something a longer prefix would reach, because every filter on
//! the way here is a *subsequence* match — [`tier`], and rubydex's `MatchMode::Fuzzy` under
//! [`by_name`]. A longer prefix therefore admits a subset of what a shorter one admitted, so an
//! untruncated list is a superset of what a fresh query would return and the client can narrow it
//! safely. What the client does not keep is this module's *ranking*: it scores rows against the
//! longer prefix itself and falls back to `sort_text` only as a tiebreak, which is the right way
//! round for a list it is holding — that score is the one that knows what has been typed since.
//!
//! The distinction is worth drawing because the flag costs a request per keystroke and each
//! repeats the whole lookup.
//!
//! **An empty list stays incomplete**, which is a separate statement rather than the same one
//! read twice. Every route that answers with no rows does so because there was nothing to say — a
//! receiver that turned out not to be a namespace, a `::` on something that cannot hold one — and
//! "the complete answer is nothing" would have the client stop asking as the word grows.

use std::collections::{HashMap, HashSet, hash_map::Entry};

use rubydex::{
    model::{
        declaration::{Ancestor, Declaration, Namespace},
        graph::Graph,
        ids::{DeclarationId, NameId, StringId, UriId},
        visibility::Visibility,
    },
    query::{self, CompletionCandidate, CompletionContext, CompletionReceiver, MatchMode},
};

use super::{
    cursor::{self, Context, Receiver},
    environment::{self, Tally},
    locator,
    position::Rebase,
    render,
    synthesized::{self, GENERATED_SCHEME},
    types::{self, Derivation, Scope, Sources, constant_at, object_name, singleton_of},
    views,
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
    /// Whether the cap dropped rows, so the client has to ask again rather than narrow this.
    ///
    /// Below the cap the list is complete and a client may filter it itself; see the module
    /// docs for why that is sound. An empty list is always incomplete — it means there was
    /// nothing to say, not that the answer is nothing.
    pub incomplete: bool,
    pub start: u32,
    pub end: u32,
    /// Whether the receiver these were offered for was one ya-lsp could name.
    ///
    /// A property of the *list* rather than of a row, because it is a property of the receiver
    /// and every row was offered for the same one. `false` is the name-based fallback: every
    /// method in the project, matched on its name, and the card for any of them has to say so.
    pub precise: bool,
    /// The receiver's own spelling, when the class these were offered for was guessed from it.
    ///
    /// The third tier, on a list rather than on a card: `precise` says the rows are a real
    /// class's members and this says the class itself was a guess. Both, together, are the only
    /// way a row can be honest about `@user.` — the methods are `User`'s, and `User` is six
    /// letters of inference.
    pub guess: Option<String>,
}

/// The three ceilings a completion answer is bounded by, which are three different questions.
///
/// A struct rather than three adjacent `usize` parameters, because they are transposable and mean
/// different things. `items` is how much JSON a keystroke may cost, and is reached by a list this
/// server believes in. The other two both bound a **guess**, and they are split because the
/// measurement behind them says the ranking is trustworthy where the row count is not:
/// `untyped_candidates` decides whether the guess is worth making at all, and `untyped` how much
/// of it is worth reading. A typed receiver never reads either, so nothing is bounded twice.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// `MAX_COMPLETION_ITEMS`: the response ceiling, applied to every list that is offered.
    pub items: usize,
    /// `MAX_UNTYPED_COMPLETION_ITEMS`: how many rows of the name-based list are sent. See
    /// [`by_name`].
    pub untyped: usize,
    /// `MAX_UNTYPED_CANDIDATES`: above this many candidates the name-based list is not offered
    /// at all, however few rows would have been sent.
    pub untyped_candidates: usize,
}

/// What can be written at `offset`.
///
/// `None` where nothing can — inside a comment or a literal, or in a document the graph has
/// never seen.
#[must_use]
pub fn complete(
    sources: &Sources<'_>,
    uri_id: UriId,
    source: &str,
    offset: u32,
    limits: Limits,
    own: &HashSet<UriId>,
    rebase: &Rebase,
) -> Option<Completion> {
    let graph = sources.graph;
    let cursor = cursor::at(source, offset)?;
    let prefix = &source[cursor.start as usize..cursor.end as usize];

    // **The coordinate change, and this is the only place completion needs one.** Everything
    // above reads `source`, which is the buffer; everything below keys the graph, whose offsets
    // index the text the indexer was last handed. On a document nobody is typing in the two
    // are one string and `rebase` is the identity — see `Rebase`.
    //
    // Completion is deferrable precisely because it never hands a graph span back: `Completion::start`/`end` come off the cursor and stay in the buffer's
    // coordinates. `hover` and `definition` answer with spans that came *out* of the graph and
    // need `Rebase::span_to_buffer` as well, which is why they are not deferred here.
    let (lo, hi) = match rebase.to_graph(offset) {
        Some(at) => (at, at),
        // The cursor is inside what was typed since the index, which is the ordinary case while
        // typing rather than an edge one. A scope survives it — see `Scope::covering` — and a
        // receiver lookup may not, which the `rebased` below decides.
        None => rebase.changed_in_graph(),
    };
    // `None` where the changed region runs out of a body: the graph can no longer say which
    // `class` or `def` the caret is in, so the request declines and is retried against a settled
    // graph rather than answered from the wrong side of `self`.
    //
    // Logged rather than counted on a field: how often this fires during real editing is a
    // property of how somebody edits, so the number worth having is one a probe collects over a
    // session rather than one the server carries.
    let Some(scope) = Scope::covering(graph, uri_id, lo, hi) else {
        tracing::debug!("completion declined: the changed region leaves the body the caret is in");
        return None;
    };
    // A receiver written in text the graph has never held cannot be looked up in it. Refusing
    // is the whole safety argument: answering anyway offers another class's members, which is a
    // wrong answer and not a missing one — reproduced as `Alpha.new.` offering `Gamma`'s.
    let Some(context) = cursor.context.rebased(rebase) else {
        tracing::debug!("completion declined: the receiver is inside text the graph has not seen");
        return None;
    };
    // Pure syntax, and decided before anything is looked up: see `Context::allows_private`.
    let private_ok = context.allows_private();
    // Empty until a candidate rubydex calls private arrives, and most lists hold none — so the
    // list that pays for this is the list the repair is about.
    let modifiers = locator::Modifiers::new(sources.read);
    // Read off the layout rather than taken as a ninth argument: `Sources` already carries the
    // one value every surface that fences takes, and the halves of a fence coming apart is the
    // defect this crate has already had once.
    let locality = Locality::at(graph, uri_id, own, sources.layout.names);
    let mut precise = true;
    let mut guess = None;
    let (items, incomplete) = match receiver_for(sources, uri_id, &context, &scope) {
        Some((receiver, only, derivation)) => {
            // The tier travels with the list, because it is a property of the *receiver* and
            // every row was offered for the same one. A guessed receiver still offers a real
            // class's members — which is a better list than matching every method in the
            // project by name — and the card on any of them still has to say where the class
            // came from.
            guess = derivation.guess;
            // Collected before the ranking because two of the three are read by it, and
            // merged after it: see [`Beside`]. `Extended` is built once and read twice,
            // because its walk answers both questions at once — what a concern extends onto a
            // class object, and how far out it sits.
            let beside = Beside {
                extended: Extended::at(graph, &receiver),
                view: in_view(sources, uri_id, &cursor.context),
                closure: InClosure::at(graph, &scope, cursor.in_a_closure),
            };
            let ranking = Ranking {
                prefix,
                distance: Distance::from_receiver(
                    graph,
                    &receiver,
                    beside.extended.as_ref(),
                    &beside.view,
                ),
                locality,
                private_ok,
                modifiers: Some(&modifiers),
            };
            from_graph(graph, receiver, only, limits.items, &ranking, &beside)
        }
        // Only one context arrives here with anything worth saying: a `.` on a receiver whose
        // type is unknown. `foo::` and a receiver that is not a namespace have no honest answer.
        //
        // **And `sources.guess` decides whether even that one may answer.** The rows below are
        // the name-based guess wearing a different hat — matched on the word alone, attached to
        // no class — so `[types] guess_from_names = false` has to reach here or the setting
        // silences the tier everywhere a card is drawn and nowhere a list is. It gates this arm
        // only: a typed receiver is not a guess and is left alone.
        None => match &cursor.context {
            Context::MethodCall { .. } if sources.guess => {
                // No receiver means no chain, so `Distance` is silent and `Locality` is the
                // only nearness this list has. It is not what ranks it, and neither is
                // anything else below `tier`. Measured over 385 untyped cursors on six
                // corpora, nineteen orderings of the eight terms behind `tier` — including
                // every one that deletes a term outright — return the same lists: the word is
                // at median rank 1 from the third character on, and inside the first 128 rows
                // for 474 of the 492 lists that came back at all. `tier` is the whole ranking
                // here because a guess still broad enough for the terms below it to matter is
                // *declined* by `admitted` rather than ordered — nothing is offered at a bare
                // `.` at all, and only 30% of cursors are answered at the fourth character.
                // The twentieth ordering is the one that shows: lifting `Locality` above
                // `tier` takes the median from 1 to 10. Which is to say the only ranking risk
                // this arm has is putting nearness in front of what the user typed. See the
                // note on `Distance::none`.
                let ranking = Ranking {
                    prefix,
                    distance: Distance::none(),
                    locality,
                    private_ok,
                    modifiers: None,
                };
                precise = false;
                by_name(graph, limits.untyped, limits.untyped_candidates, &ranking)
            }
            // Nothing to say, which is not the same as an answer that is empty: telling the
            // client this list is complete would have it stop asking as the word grows.
            _ => (Vec::new(), true),
        },
    };

    Some(Completion {
        items,
        incomplete,
        start: cursor.start,
        end: cursor.end,
        precise,
        guess,
    })
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
    sources: &Sources<'_>,
    uri_id: UriId,
    context: &Context,
    scope: &Scope,
) -> Option<(CompletionReceiver, Only, Derivation)> {
    let graph = sources.graph;
    match context {
        // `caller` and not `scope.self_id`, and it is not a choice: rubydex's
        // `expression_completion` requires a `self` type, so a `None` there collects no methods
        // and no instance variables at all — the commonest completion there is. The nesting's
        // own declaration is what has to be passed.
        Context::Expression => Some((
            CompletionReceiver::Expression {
                self_decl_id: scope.caller(graph),
                nesting_name_id: scope.nesting,
            },
            Only::Everything,
            Derivation::default(),
        )),
        Context::Argument { name } => {
            // Only a receiver rubydex could name gives real keyword arguments. A name-based
            // guess would put another class's parameters into this call, which is worse than
            // offering none: the completion would be syntactically valid and wrong.
            // `Context::Argument` *is* the implicit-receiver case — the cursor is inside a
            // call's parentheses, not after its operator — so a private callee is reachable here
            // exactly as `Context::allows_private` says.
            let receiver = match locator::precise_call(
                graph,
                uri_id,
                *name,
                sources.layout,
                locator::Privacy::Allowed,
            ) {
                Some(method_decl_id) => CompletionReceiver::MethodArgument {
                    self_decl_id: scope.caller(graph),
                    nesting_name_id: scope.nesting,
                    method_decl_id,
                },
                None => CompletionReceiver::Expression {
                    self_decl_id: scope.caller(graph),
                    nesting_name_id: scope.nesting,
                },
            };
            Some((receiver, Only::Everything, Derivation::default()))
        }
        Context::NamespaceAccess { receiver } => {
            let namespace_decl_id = match receiver {
                Receiver::Constant(offset) => constant_at(graph, uri_id, *offset)?,
                // `self::CONST` is legal and rare; the nesting is what it means. `caller` is
                // the singleton in a class or module body, and asking a singleton reaches the
                // class it is attached to — so this answers the same list `HR::` does, from a
                // body and from inside a method alike. Pinned by
                // `every_receiver_that_can_precede_a_double_colon_is_answered`, because the
                // equivalence is rubydex's and not something this line states.
                Receiver::SelfObject(_) => scope.caller(graph)?,
                // `"foo"::Bar`, `Foo.new::Bar` and `foo.bar::Baz` all parse, and mean nothing
                // anybody writes on purpose. An instance is not a namespace.
                //
                // `Receiver::Named` joins them: a name is at best an instance, and the two
                // rungs that read one both answer with a class rather than with a namespace.
                Receiver::Instance(_)
                | Receiver::Literal(_)
                | Receiver::Returned { .. }
                | Receiver::Yielded { .. }
                | Receiver::Assigned { .. }
                | Receiver::Spelled { .. }
                | Receiver::Named(_) => return None,
                // `::Foo` asks for the top level, which is `Object` — but rubydex's namespace
                // walk deliberately stops *before* Object's own members, to keep `String::` from
                // listing every top-level constant in the project. Asking the same question as
                // an expression at the top level reaches them, and dropping everything that is
                // not a constant leaves what `::` can actually be followed by.
                Receiver::TopLevel => {
                    return Some((
                        CompletionReceiver::Expression {
                            self_decl_id: None,
                            nesting_name_id: object_name(graph),
                        },
                        Only::Constants,
                        Derivation::default(),
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
                Derivation::default(),
            ))
        }
        // Every arm of this one lives in `types::method_receiver`, because navigation asks the
        // same question and the two must not answer it differently: what `person.` is cannot
        // depend on whether the user pressed a key or hovered.
        Context::MethodCall { receiver } => {
            let typed = types::method_receiver(sources, uri_id, receiver, scope)?;
            Some((
                CompletionReceiver::MethodCall {
                    self_decl_id: scope.caller(graph),
                    receiver_decl_id: typed.declaration,
                },
                Only::Everything,
                typed.derivation,
            ))
        }
    }
}

/// Everything rubydex offers for a receiver, filtered to the prefix and capped.
///
/// The flag is `Completion::incomplete`: true where the cap dropped rows, and true where there
/// was nothing to say at all, which is not the same statement as "the answer is empty".
fn from_graph(
    graph: &Graph,
    receiver: CompletionReceiver,
    only: Only,
    limit: usize,
    ranking: &Ranking,
    beside: &Beside,
) -> (Vec<Item>, bool) {
    let candidates = match query::completion_candidates(graph, CompletionContext::new(receiver)) {
        Ok(candidates) => candidates,
        Err(error) => {
            // A receiver that is not a namespace after all. Nothing to say, and nothing broken.
            tracing::debug!("no completion candidates: {error}");
            return (Vec::new(), true);
        }
    };

    let mut ranked: Vec<Ranked> = candidates
        .iter()
        .filter(|candidate| only.accepts(graph, candidate))
        .enumerate()
        .filter_map(|(sequence, candidate)| rank(graph, candidate, sequence, ranking))
        .collect();
    // Before the cap and never after it: these rows compete with the graph's own on one key,
    // and appending past `take_best` would answer with `limit` + however many a concern holds.
    if let Some(extended) = &beside.extended {
        ranked.extend(
            extended
                .candidates(graph, ranking.prefix)
                .into_iter()
                .filter_map(|id| {
                    ranked_declaration(graph, id, graph.declarations().get(&id)?, ranking)
                }),
        );
    }
    // The view context's rows, added the same way and in the same place for the same
    // reason: before the cap, so that they compete with the graph's own on one key rather than
    // being appended past it.
    add_view(graph, &mut ranked, ranking, &beside.view);
    // Last of the three, and the order is only half a statement: a template has no class body
    // to write a block in, so this and the view context cannot both have rows. What it does
    // state is the one that matters — the concern edge above has already been merged, so a
    // name a concern extends onto the class object is held against this the same way a name the
    // graph's own walk found is.
    add_closure(graph, &mut ranked, ranking, beside.closure.as_ref());
    let truncated = take_best(&mut ranked, limit);
    (
        ranked.into_iter().map(|entry| entry.item).collect(),
        truncated,
    )
}

/// The lists that join the graph's own answer before the cap.
///
/// One value rather than three parameters, because they are one idea. rubydex answers for a
/// *receiver*; each of these is a name reachable from the cursor that no receiver names — what
/// a Rails concern extends onto a class object, what Rails puts into a template's view context,
/// and what a block written straight into a class body may be run against. Resolution can ask
/// each of them as a second question after the first fails, because it takes one answer;
/// completion has to **collect**, so they are gathered here and merged into the ranked rows
/// before `take_best` rather than appended past it.
struct Beside {
    extended: Option<Extended>,
    view: Vec<views::Reached>,
    closure: Option<InClosure>,
}

/// What a bare word in a **template** can complete to.
///
/// `None` for every other document and for every context with a receiver written in it, which
/// is [`views::Views::reachable`]'s own gate plus one syntactic test: a template's view context
/// is what an *implicit* receiver answers, and `person.` in a template is still `person`'s.
///
/// The walk is [`views`]', shared with [`locator::resolve_typed`] so that a jump and a list
/// cannot disagree about what a template can call — the same seam the concern edge shares
/// through [`locator::extended_modules`], and for the same reason: resolution takes the
/// first answer and completion collects all of them.
fn in_view(sources: &Sources<'_>, uri_id: UriId, context: &Context) -> Vec<views::Reached> {
    if !matches!(context, Context::Expression | Context::Argument { .. }) {
        return Vec::new();
    }
    sources
        .views
        .reachable(sources.graph, uri_id)
        .map(|reachable| reachable.members(sources.graph))
        .unwrap_or_default()
}

/// Put the view context's rows into the list, taking a name away from `Object` where they share
/// one.
///
/// **The shadowing is Ruby's rather than a preference.** A helper module is `include`d into the
/// view class and `Kernel` is at the end of every chain, so an application that writes
/// `def format` in `ApplicationHelper` really has replaced `Kernel#format` for every template —
/// and a row that kept the graph's answer would complete the word and then jump to the wrong
/// one. It costs a pass over the ranked rows and is only paid where a template offered a name
/// the view context also holds.
fn add_view(graph: &Graph, ranked: &mut Vec<Ranked>, ranking: &Ranking, view: &[views::Reached]) {
    if view.is_empty() {
        return;
    }
    let rows: Vec<Ranked> = view
        .iter()
        .filter_map(|reached| {
            let declaration = graph.declarations().get(&reached.declaration)?;
            ranked_declaration(graph, reached.declaration, declaration, ranking)
        })
        .collect();
    if rows.is_empty() {
        return;
    }
    let shadowed: HashSet<u64> = rows
        .iter()
        .map(|entry| StringId::from(&entry.item.label).get())
        .collect();
    ranked.retain(|entry| !shadowed.contains(&StringId::from(&entry.item.label).get()));
    ranked.extend(rows);
}

/// The instance side a **block written straight into a class body** can reach.
///
/// # The list beside the rung that answers from two scopes
///
/// `locator`'s closure rung is the resolution half of this, and its docs hold the argument: a
/// bare name in `rule(:colon) { … }` may be on the class object, which is what `self` is until
/// somebody re-binds it, and may equally be on an instance, because re-binding it is what every
/// DSL that takes a block does. That rung answers one name. This offers every name that rung
/// would answer — the same rule read as a list rather than as a lookup, which is the whole of
/// why it exists: `hover` in one of these blocks named a member the list beside it did not hold.
///
/// # The two agree per member, and that is what makes the order safe
///
/// `locator` reaches its closure rung only once `resolve_call` has come back imprecise, so the
/// class object's own answer — including the one a concern extends onto it — always wins.
/// [`add_closure`] holds that same order by adding a row only where the list does not already
/// carry the name, which is why it runs after the concern edge and not before it. What `hover`
/// says of any row offered here is therefore what the row was offered for.
///
/// # A class, never a module, and never a receiver somebody wrote
///
/// Both refusals are `locator`'s, restated where each is cheapest. A module has no instances for
/// the claim to be about, and the blocks a module body holds — `included do`, `class_methods do`
/// — re-bind `self` to the *including* class, which is not reachable from the module at all;
/// that one is tested here. A receiver written down says what `self` is not — `Foo.` and
/// `person.` are answered by the receiver, whatever block they are written inside — and that one
/// is [`cursor::at`](cursor::at)'s, which is why `Cursor::in_a_closure` is `false` on those
/// cursors rather than true and discarded.
struct InClosure {
    /// The class the block's `self` may be an instance of.
    class: DeclarationId,
    /// The lexical nesting, which rubydex's expression query wants and this does not read —
    /// every row is filtered to a method, and the constants it reaches are the ones the class
    /// object's list already holds from the same nesting.
    nesting: NameId,
}

impl InClosure {
    /// `None` wherever either half of the evidence is missing.
    ///
    /// **The scope and not the receiver**, although the two hold the same declaration: every
    /// receiver that can reach here was built from `Scope::caller`, and the ones that were not
    /// — a `.` or a `::` — never reach here at all, because `Cursor::in_a_closure` is `false`
    /// on any cursor with a receiver written in front of it. Reading the scope is that fact
    /// stated once rather than a match with two arms nothing can take.
    fn at(graph: &Graph, scope: &Scope, in_a_closure: bool) -> Option<Self> {
        if !in_a_closure {
            return None;
        }
        // "rubydex called this a class object", which is the same test the closure rung makes
        // of the same declaration — a bare call in a class body is recorded on the singleton,
        // and one inside a `def` is not, so this is also what keeps a method body out.
        let class = locator::attached_class(graph, scope.caller(graph)?)?;
        matches!(
            graph.declarations().get(&class),
            Some(Declaration::Namespace(Namespace::Class(_)))
        )
        .then_some(Self {
            class,
            nesting: scope.nesting,
        })
    }

    /// Every instance member of that class the list does not already hold, ranked.
    ///
    /// The question asked is the one a receiverless call in an instance method asks — `self` is
    /// an instance of this class — so rubydex walks the ancestors and a superclass's `def` is
    /// reached, which is the half the name rung was throwing away. Filtered to methods and
    /// nothing else: the constants it also collects are the class object's from the same
    /// nesting and are already on the list, and an instance variable is a different claim
    /// entirely — `@x` in a class body is the class object's, whatever the block does with
    /// `self`, because rebinding the receiver does not rebind the file's lexical scope.
    ///
    /// **`held` is tested before the row is built**, which is [`Extended::candidates`]' ordering
    /// and the same reasoning: `last_segment` is a slice and the hash lookup is a hash lookup,
    /// where [`ranked_declaration`] walks a declaration's definitions to score its locality. On
    /// a controller's chain most of these names are the class object's already.
    ///
    /// Visibility is [`ranked_declaration`]'s as for every other row, and `private_ok` is
    /// already true here: no receiver was written, so a private method is exactly what may be
    /// called.
    fn rows(&self, graph: &Graph, ranking: &Ranking, held: &HashSet<u64>) -> Vec<Ranked> {
        let receiver = CompletionReceiver::Expression {
            self_decl_id: Some(self.class),
            nesting_name_id: self.nesting,
        };
        // `unwrap_or_default` where [`from_graph`] logs, because the one error this call
        // returns is *the receiver is not a namespace* and [`InClosure::at`] has already
        // looked: the class exists and is a `Namespace::Class`, or this value does not exist.
        let candidates = query::completion_candidates(graph, CompletionContext::new(receiver))
            .unwrap_or_default();
        candidates
            .iter()
            .filter_map(|candidate| {
                let CompletionCandidate::Declaration(id) = candidate else {
                    return None;
                };
                let declaration = graph.declarations().get(id)?;
                if !matches!(declaration, Declaration::Method(_)) {
                    return None;
                }
                let label = render::last_segment(declaration.name());
                if held.contains(&StringId::from(label).get()) {
                    return None;
                }
                ranked_declaration(graph, *id, declaration, ranking)
            })
            .collect()
    }
}

/// Put the block's instance rows into the list, keeping every name the class object answered.
///
/// The opposite of [`add_view`]'s shadowing, and deliberately: a helper module really does
/// replace `Kernel#format` for a template, where a block's `self` is an **inference** about what
/// somebody's DSL does and the class object is what Ruby will use if the inference is wrong. So
/// the existing row stands and this fills the gaps around it — which is also the rule that keeps
/// `hover` and this list saying the same thing about every row in it.
fn add_closure(
    graph: &Graph,
    ranked: &mut Vec<Ranked>,
    ranking: &Ranking,
    closure: Option<&InClosure>,
) {
    let Some(closure) = closure else {
        return;
    };
    let held: HashSet<u64> = ranked
        .iter()
        .map(|entry| StringId::from(&entry.item.label).get())
        .collect();
    ranked.extend(closure.rows(graph, ranking, &held));
}

/// The members of a module a class object `extend`s and rubydex did not linearize — the
/// collecting half of the repair [`locator`] resolves.
///
/// Resolution can ask a second question after the first fails, because it takes one answer;
/// completion cannot, because it *collects*. So the same walk runs here and the rows join the
/// list before it is ranked and capped.
///
/// The walk itself is [`locator::extended_modules`], shared so that the one shape it is about is
/// stated once. **It is not the Rails concern edge**, which used to be read here and is now
/// declared: `workspace/rails/concerns.rs` writes a concern's class methods onto every class that
/// includes it, so `query::completion_candidates` offers them like any other member of the
/// receiver.
struct Extended {
    /// The singleton the concerns were found from, and the receiver rubydex answered for.
    ///
    /// Kept because it is also the dedup question: a name this already offers must not be
    /// offered a second time by the edge, and asking it is the same ancestor walk resolution
    /// makes.
    on: DeclarationId,
    modules: Vec<locator::Extension>,
}

impl Extended {
    /// `None` where there is no edge to walk: a receiver that is not a class object — every
    /// instance method call, and every cursor inside a `def` — or a chain with no concern in it.
    fn at(graph: &Graph, receiver: &CompletionReceiver) -> Option<Self> {
        let on = class_object(graph, receiver)?;
        let modules = locator::extended_modules(graph, on);
        (!modules.is_empty()).then_some(Self { on, modules })
    }

    /// Every member the edge adds, nearest concern first.
    ///
    /// Deduplicated the way rubydex's own walk deduplicates and for the same reason: the
    /// nearest declaration of a name is the one that answers, so a second concern spelling a
    /// name the first already spelled is not a second row. **And a name the ordinary walk
    /// already offers is not added at all** — the resolution's own rule, per member rather than
    /// per request: a class that writes its own `def self.validates` keeps it, and what a
    /// concern extends is only ever what nothing else answered.
    ///
    /// The prefix is tested **before** that question and not after, which is the rule
    /// [`ranked_declaration`] already holds between `tier` and `reachable`: the ancestor walk
    /// costs a hundred hash lookups on a Rails model's singleton chain and the prefix test
    /// costs a string compare, so the cheap filter goes first and a keystroke pays for the
    /// handful of rows it could actually offer rather than for all 476 of them.
    fn candidates(&self, graph: &Graph, prefix: &str) -> Vec<DeclarationId> {
        let mut seen: HashSet<StringId> = HashSet::new();
        let mut found = Vec::new();
        for extends in &self.modules {
            let Some((_, extended)) = namespace(graph, extends.module) else {
                continue;
            };
            for (name, member) in extended.members() {
                // `extend` installs methods and nothing else: a constant nested in a
                // `ClassMethods` module is not reachable through the singleton at all.
                let Some(declaration @ Declaration::Method(_)) = graph.declarations().get(member)
                else {
                    continue;
                };
                if tier(prefix, render::last_segment(declaration.name())).is_none() {
                    continue;
                }
                if !seen.insert(*name)
                    || query::find_member_in_ancestors(graph, self.on, *name, false).is_ok()
                {
                    continue;
                }
                found.push(*member);
            }
        }
        found
    }
}

/// The receiver's own declaration, where the receiver is a **class object**.
///
/// The one shape the concern edge applies to, and it is rubydex's answer rather than a
/// syntactic test: a bare call in a class body and an explicit `Foo.` both arrive here as the
/// singleton class, and the same call inside an instance `def` arrives as the class — which is
/// exactly the discriminator the concern edge turns on, reached from the other side.
///
/// `extended_modules` declines anything that is not a singleton, so this hands over what
/// the receiver holds rather than testing it twice. `NamespaceAccess` is the one arm that has
/// to look: `Foo::` names the class and rubydex collects its *singleton's* methods, which is
/// also why [`Distance::from_receiver`] seeds that chain there.
fn class_object(graph: &Graph, receiver: &CompletionReceiver) -> Option<DeclarationId> {
    match receiver {
        CompletionReceiver::MethodCall {
            receiver_decl_id, ..
        } => Some(*receiver_decl_id),
        CompletionReceiver::Expression { self_decl_id, .. }
        | CompletionReceiver::MethodArgument { self_decl_id, .. } => *self_decl_id,
        CompletionReceiver::NamespaceAccess {
            namespace_decl_id, ..
        } => singleton_of(graph, *namespace_decl_id),
    }
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
    fn from_receiver(
        graph: &Graph,
        receiver: &CompletionReceiver,
        extended: Option<&Extended>,
        view: &[views::Reached],
    ) -> Self {
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
        // The concern edge, at the step the class that installed it sits at. Before the
        // lexical walk for the same reason every ancestor chain is: these are members, and
        // `Object` must not be able to claim them at one step from the cursor.
        if let Some(extended) = extended {
            distance.extended(extended);
        }
        // The view context's rows, at the step [`views::Reached`] carries — which is the chain Rails
        // builds `_helpers` from and not one of rubydex's. Before the lexical walk for the
        // reason the concern edge is: these are members, and `Object` must not be able to claim
        // them at one step from the cursor.
        for reached in view {
            if let Some(declaration) = graph.declarations().get(&reached.declaration) {
                distance.record(*declaration.owner_id(), reached.step as usize);
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

    /// The one seed that is not a walk rubydex is about to make.
    ///
    /// A concern's `ClassMethods` is on **none** of those chains — that is the whole reason its
    /// members were missing from the list in the first place — so every one of them would land
    /// on [`NO_DISTANCE`]: last, equally last, and *behind `Object`'s own methods*, which is
    /// backwards for the name a model body is most likely to be typing.
    ///
    /// One `record` and not a chain, because [`Extended::candidates`] offers a module's own
    /// members and never its ancestors': what those ancestors declare is not on this list, and
    /// numbering them here could only make some *other* row look nearer than it is.
    /// [`locator::Extends`] carries the step, which counts the classes the receiver's chain
    /// passes through — where Ruby's own singleton chain puts the module the `extend` landed on.
    fn extended(&mut self, extended: &Extended) {
        for extends in &extended.modules {
            self.record(extends.module, extends.step);
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
    ///
    /// **A generated document is in here beside the file that implied it, at the same step**,
    /// carrying `true` for *this step came from a generated document*. It has no path of its own
    /// — `synthesized::generated_uri` is a scheme in front of the source's URI, deliberately not
    /// a path — so the walk below would score it as far as a name can be, and `group` reads
    /// *that* as "not the user's code". The declarations a Rails application cares about most
    /// are every one of them generated, so without this a model's own columns sit behind every
    /// method any file in the project declares anywhere on the chain, `Object`'s included.
    ///
    /// The third field is *the application never loads this document* —
    /// [`environment::in_a_test_tree`], [`environment::in_a_generator_template`] or
    /// [`environment::in_a_migration`] — and it travels here because [`Locality::of`] is already
    /// the one walk over a declaration's definitions. It is packed into the entry rather than read from `environment::Trees` for
    /// the same reason the step is: the point of use runs once per definition of every method
    /// declaration in the graph, and one lookup there is the budget. See [`Written::loadable`].
    ///
    /// **The tags are read of different sets**, which is [`environment::Trees`]' rule arriving
    /// here: the test tree off `own`, because a gem's `lib/rack/test/` is a published library and
    /// a miss there has to mean *not a spec*; the template and the migration off the whole graph,
    /// because a tree `rails generate` copies out of — or that `db:migrate` runs one file of — is
    /// the same thing wherever it ships from, and pundit's and an engine's are the common cases.
    documents: HashMap<UriId, (u8, bool, bool)>,
    /// Whether the fence is on at all: false wherever the cursor is itself under a test tree.
    ///
    /// A developer editing a spec is exactly who the `def` in the next spec file over is the
    /// answer for — `environment::fenced_from` is where that is decided, for this and for the
    /// name rung alike. It is a field rather than a test at the point of use because the
    /// question is about the *cursor*, which does not change between candidates, and the point
    /// of use runs once per method declaration in the graph.
    fenced: bool,
}

/// What a declaration in none of those documents is worth: nothing, and equally nothing, so the
/// rest of the key still separates them.
const NO_LOCALITY: u8 = u8::MAX;

impl Locality {
    /// Built once per request, from the user's own documents rather than from the whole graph —
    /// 161 path comparisons on a real Rails app, against 7,000.
    fn at(graph: &Graph, here: UriId, own: &HashSet<UriId>, names: environment::Names<'_>) -> Self {
        let mut documents = HashMap::new();
        let here_uri = graph.documents().get(&here).map(|document| document.uri());
        // The same gate `locator::loadable_from` asks, so the two dropping surfaces cannot
        // disagree about which cursors are fenced. A cursor in a document the graph has never
        // held scores nothing and fences nothing: both halves fail in the direction that offers
        // more rather than fewer.
        let fenced = environment::fenced_from(here_uri, names);
        let Some(cursor) = here_uri else {
            return Self { documents, fenced };
        };
        let cursor = directory_of(cursor);
        let depth = segments(cursor);

        // **The two tags that are read of the whole graph and not of `own`.** A gem's generator
        // template is unloadable for the same reason the project's is — nobody requires the tree
        // `rails generate` copies out of — and pundit's is the common case, so `own` cannot be
        // the set. An engine's `db/migrate/` is the same shape: a tree run one file at a time by
        // a rake task, wherever it was shipped from. It costs one pass over the documents, which is the pass `own` itself was
        // built with, and it is the loop below that then decides the step: a template of the
        // project's own is overwritten there with its real distance and the same flag, because
        // `insert` is last-writer-wins and `own` comes second.
        //
        // An entry at `NO_LOCALITY` changes no ranking: `of` takes a `min` over the entries it
        // finds and falls back to exactly this value when it finds none.
        //
        // The generated documents are collected in the same pass and resolved after the loop
        // below, because what each of them is worth is what its **source** is worth and that is
        // not known until the source has been given a step. They cannot be computed from a
        // source's URI any more: one source writes one generated document per body, so the set
        // is read off the graph and mapped back by the naming rather than guessed at.
        let mut generated: Vec<(UriId, &str)> = Vec::new();
        for (id, document) in graph.documents() {
            let uri = document.uri();
            if environment::in_a_generator_template(uri) || names.in_a_migration(uri) {
                documents.insert(*id, (NO_LOCALITY, false, true));
            }
            if uri.starts_with(GENERATED_SCHEME) {
                generated.push((*id, synthesized::source_of(uri)));
            }
        }

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
            // The test flag is the source document's and is carried onto the generated entry
            // below with it: what `workspace/rails/` writes down for a file under `spec/` is
            // reachable exactly where that file is.
            let testing = names.in_a_test_tree(document.uri())
                || environment::in_a_generator_template(document.uri())
                || names.in_a_migration(document.uri());
            documents.insert(id, (step, false, testing));
        }
        // Every document this pass wrote, at its source's own step and with its source's own
        // test flag: what `workspace/rails/` writes down for a file under `spec/` is reachable
        // exactly where that file is. A generated document whose source is in none of the
        // entries above — a gem's, say — is left out, exactly as it was when this was computed
        // per own document.
        for (id, source) in generated {
            if let Some(&(step, _, testing)) = documents.get(&UriId::from(source)) {
                documents.insert(id, (step, true, testing));
            }
        }
        Self { documents, fenced }
    }

    /// The nearest document the declaration was written in, whether ya-lsp wrote it, and
    /// whether the application loads it at all.
    ///
    /// The nearest, because a class reopened in two places — a Rails concern, a monkey patch —
    /// should be scored by the copy the cursor can see, not by whichever definition rubydex
    /// happened to record first. **One walk for all three**, because this runs over every method
    /// declaration in the graph on the name-based path: the second is a `bool` already sitting
    /// in the entry the first reads, and the third is two counters over the same entries.
    ///
    /// A tie between a real file and a generated one goes to the real file: `(step, false)`
    /// sorts before `(step, true)`, which is the ordering the caller then spends. The test flag
    /// is deliberately **not** in that comparison — it decides whether the row exists, not where
    /// it sits, so folding it into the key would tiebreak on something already answered.
    fn of(&self, graph: &Graph, declaration: &Declaration) -> Written {
        let mut nearest: Option<(u8, bool)> = None;
        let mut tally = Tally::default();
        for definition in declaration
            .definitions()
            .iter()
            .filter_map(|id| graph.definitions().get(id))
        {
            // A definition in none of the user's own documents is a gem's, Ruby's own
            // signatures', or one the walk never scored — far, and loadable. `own` is the only
            // set that knows where *this project's* test trees are, so a miss here is not
            // evidence of a spec: a gem with a `test/` directory inside its `lib/` is not this
            // rule's business.
            let entry = self.documents.get(definition.uri_id());
            if let Some(&(step, generated, _)) = entry {
                nearest = Some(nearest.map_or((step, generated), |held| {
                    std::cmp::min(held, (step, generated))
                }));
            }
            // Outside the `if`: a definition in none of the user's own documents is still a
            // definition, and it is the count of them that makes the "any" rule an "any". See
            // [`Tally::loadable`], which is also where a declaration with no definitions at all
            // is decided.
            tally.saw(entry.is_some_and(|&(_, _, in_a_test)| in_a_test));
        }
        let (step, generated) = nearest.unwrap_or((NO_LOCALITY, false));
        Written {
            step,
            generated,
            loadable: !self.fenced || tally.loadable(),
        }
    }
}

/// Where a declaration was written, as the ranking and the fence both need it.
///
/// Three answers from [`Locality::of`]'s single walk. Two of them order the list and the third
/// decides whether the row is on it.
struct Written {
    /// How near the nearest of the user's own documents holding it sits to the cursor.
    step: u8,
    /// Whether that nearest document is one ya-lsp generated rather than one the user wrote.
    generated: bool,
    /// Whether **any** definition of it is in code the application loads.
    ///
    /// *Any*, and one with no definitions at all is loadable: both are [`Tally::loadable`]'s,
    /// which is the only place either is written down. What this removes is the declaration
    /// whose *every* definition is under a test tree — a name that cannot be called from where
    /// the cursor is, which is the same thing `reachable` refuses a private method for.
    ///
    /// **True for everything when the cursor is in a test tree**, so the term is inert exactly
    /// where it would be wrong. See [`Locality::fenced`].
    loadable: bool,
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
///
/// **A guess is refused on its candidate count and truncated on its row count**, which are
/// `admitted` and `shown`, and the split is measured rather than tidy.
///
/// Over `admitted` candidates this answers with **no rows at all**. That is a decision and not an
/// absence: the list comes back on its own as soon as what has been typed narrows the candidates
/// under the line. At an empty prefix it never does — the set there is the project's entire name
/// universe, 26,073 to 45,953 over five corpora, with the word the file actually wrote at rank
/// 4,070 at the median — which is the case the ceiling exists for.
///
/// Under it, the rows are cut to `shown`, and that is honest here where it would not be at an
/// empty prefix. With nothing typed, `tier` is 1 for every row and `length` is 0, leaving
/// `Locality` — which directory a name lives in — as the only live term, so a truncation would
/// keep rows chosen by nothing. From three characters on both are live, and it shows: in every
/// measured list of up to 512 candidates the word sits inside the first 128 rows, 430 of 430.
///
/// `isIncomplete` is true in both cases, and it is what makes the refusal self-correcting — see
/// the module docs. Saying the empty answer is *complete* would have the client filter it locally
/// and never ask again, so the list could not reappear inside the word it was declined for.
fn by_name(graph: &Graph, shown: usize, admitted: usize, ranking: &Ranking) -> (Vec<Item>, bool) {
    // Every method name contains a `#` and no other declaration's does, so this is rubydex's
    // parallel filter doing the "methods only" pass for free.
    let query = format!("#{}", ranking.prefix);
    let mut best: HashMap<u64, Ranked> = HashMap::new();

    for id in query::declaration_search(graph, &[&query], &MatchMode::Fuzzy) {
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

    // Counted after the dedup, because a duplicate is not a candidate a person could pick and
    // the ceiling is about what the ranking can be trusted to order. Nothing above this point is
    // wasted when it fires: the walk is what decides whether the guess is narrow enough to make.
    if best.len() > admitted {
        return (Vec::new(), true);
    }

    let mut ranked: Vec<Ranked> = best.into_values().collect();
    let truncated = take_best(&mut ranked, shown);
    (
        ranked.into_iter().map(|entry| entry.item).collect(),
        truncated,
    )
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
    /// The second opinion on rubydex's visibility record, for the one shape it gets wrong.
    ///
    /// Here for the reason it is in `locator`: a bare `private` written inside a block is
    /// recorded against every `def` below the block, so an ordinary Rails concern's public
    /// methods were never offered on any receiver. See [`locator::Modifiers`].
    ///
    /// **`None` on the name-based path, and that is a bound rather than an oversight.** The
    /// repair costs a read of the declaring document, memoised per document, and the receiver
    /// path's candidates are one ancestry's members — tens of documents. [`by_name`] walks every
    /// method in the graph before it decides whether to answer at all, so the same repair there
    /// would read thousands of files at a keystroke. Nothing is lost at any one cursor: the two
    /// paths are alternatives, the name-based one runs only where no receiver resolved, and what
    /// it produces is the *Guessed* tier — where a list that came back one name short is what
    /// the tier already warns about, and a **Resolved** card contradicted by its own list is
    /// what this repair exists to prevent.
    modifiers: Option<&'a locator::Modifiers<'a>>,
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
fn reachable(
    graph: &Graph,
    modifiers: Option<&locator::Modifiers<'_>>,
    id: DeclarationId,
    declaration: &Declaration,
    label: &str,
) -> bool {
    if !matches!(declaration, Declaration::Method(_)) {
        return true;
    }
    if matches!(
        graph.visibility(&id),
        // rubydex treats `module_function`'s instance copy as private, and so does Ruby.
        Some(Visibility::Private | Visibility::ModuleFunction)
    ) && modifiers.is_none_or(|modifiers| modifiers.confirm(graph, id))
    {
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
    /// Whether ya-lsp wrote the document this was declared in, rather than the user.
    ///
    /// **Below `distance` and above `locality`, and it has to be its own term.** *What the class
    /// was written to do, then what its table says it holds* is a property of the **kind** of
    /// declaration, not of where it lives: a column is declared in `db/schema.rb` and the method
    /// beside it in `app/models/story.rb`, so leaving the two to `locality` decides them by which
    /// of those directories the cursor happens to be nearer — the order holds from `app/` and
    /// inverts from `db/`, which the test named for it fails without.
    ///
    /// **It is here for that invariant and the corpora argue against it.** Over the audit's
    /// 1,452 typed cursors it moves 18 words out of rank 1 into the top ten, 124 to 106, while
    /// tightening every band below — median 30 to 29, p90 199 to 192. A developer does reach a
    /// table's members slightly more often than the `def`s beside them. It is kept because the
    /// alternative is an order that changes with the cursor's directory.
    generated: bool,
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
    /// shortest-name-first. The zero is what lets the term rank high without cost — it is
    /// silent at exactly the cursor where it would be wrong, so the terms it sits above keep
    /// the list they had. See [`order`].
    length: usize,
    item: Item,
}

/// What the user typed first, then how near the name is, then the shorter one.
///
/// **Match quality leads, and for two releases it did not.** `group` — is this the user's code —
/// sat above `tier`, so a name in the project matching `wh` as a *subsequence* beat `where`
/// matching it as a *prefix*. Measured over 1,452 typed cursors on six corpora, moving `tier`
/// to the front takes the words sitting past rank 128 one character into the word from 21 to
/// **2**, and past 256 from 9 to **0**. Everything below it is a tiebreak among rows the user
/// has equally asked for.
///
/// A name beginning with an underscore sinks: Ruby spells "you were not meant to call this" that
/// way, and without the rule `User.` opens on `__send`, `_fork` and `_load_from_sql` because
/// underscores sort before letters. Typing one lifts them back — someone who writes `_` means it.
///
/// **`group` is three bands, not four, and that is the other half of the same correction.**
/// Keyword arguments lead because they are the only suggestion that can be *wrong* to leave out:
/// in `build(` the parameter names are the answer to the question and everything else is
/// background. Then every declaration, the user's own and a gem's **together** — the distinction
/// used to be this term's whole job and it belongs to `distance`, which knows that a method on
/// the receiver beats one on `Object` whoever wrote either. Ruby's keywords come last, measured:
/// putting them above declarations empties rank 1 and the entire top ten at a bare-word cursor,
/// 34 and 183 rows to **0**, because forty keywords sit in front of everything before a
/// character is typed. What that costs is `end` a few ranks mid-word — rank 4 to 10 at `en` — and
/// it is handed back by the first rule on this list, since `end` typed in full is an exact match
/// and outranks every prefix: rank 4 to **2**.
///
/// [`Distance`] is what ranks a list nothing has been typed into at all, where `tier` is 1 for
/// every row: the nearer owner wins, and `Object` — the end of every chain and the drain for
/// everything rubydex could not attribute — is as far as a name can be.
///
/// **`length` sits above both nearness terms, and for two releases it sat below them.** It is
/// the one term on this list that cannot speak at a bare cursor — `sort_length` is zero when
/// nothing has been typed — and it is the strongest tiebreak the moment a character arrives:
/// among rows that all matched `ren`, `render` is the answer and `rendered_format` is not.
/// Asking `generated` and `locality` first let a long name from the nearer file sit above the
/// short one the user was spelling. Measured over the same 1,452 typed cursors, lifting it two
/// places takes the right word to rank 1 for 543 -> **586** cursors one character in, 727 ->
/// **759** at two and 887 -> **919** at three, and into the top ten for 1,096 -> **1,122**,
/// 1,164 -> **1,169** and 1,154 -> **1,157**. A bare cursor is untouched — 108 and 392, both
/// unchanged — which is the whole of why the move is free: the term is inert exactly where the
/// two below it do their work.
///
/// [`Locality`] answers the same question as [`Distance`] asked of a different thing. Distance
/// is the stronger answer and goes first, but it is silent exactly twice: where there is no
/// receiver to measure a chain from, and among the rows of one chain that tie. Locality speaks
/// in both.
fn order(a: &Ranked, b: &Ranked) -> std::cmp::Ordering {
    b.tier
        .cmp(&a.tier)
        .then(a.internal.cmp(&b.internal))
        .then(a.group.cmp(&b.group))
        .then(a.distance.cmp(&b.distance))
        .then(a.length.cmp(&b.length))
        .then(a.generated.cmp(&b.generated))
        .then(a.locality.cmp(&b.locality))
        .then(a.sequence.cmp(&b.sequence))
        .then(a.item.label.cmp(&b.item.label))
}

/// Keep the `limit` best rows in order, and answer whether any were dropped.
///
/// The return value is the whole of `Completion::incomplete`: dropping a row is the only way
/// this list can fail to hold something a longer prefix would reach. See the module docs.
fn take_best(ranked: &mut Vec<Ranked>, limit: usize) -> bool {
    let truncated = ranked.len() > limit;
    if truncated {
        ranked.select_nth_unstable_by(limit, order);
        ranked.truncate(limit);
    }
    ranked.sort_unstable_by(order);
    truncated
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
                generated: false,
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
            generated: false,
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
    if !ranking.private_ok && !reachable(graph, ranking.modifiers, id, declaration, label) {
        return None;
    }

    // One walk over the definitions, not three. `Locality` is built from exactly the documents
    // that are the user's own code, so "is this theirs" is "did any document score at all" —
    // and this runs over every method declaration in the graph on the name-based path.
    let written = ranking.locality.of(graph, declaration);
    // **A row the application cannot load is not offered, and that is a drop rather than a
    // rank.** Ruby will not find this name from where the cursor is — the file it is written in
    // is loaded by RSpec and by nothing else — so offering it is offering a suggestion that
    // cannot run, which is the judgement `reachable` already makes about a private method.
    // Sinking it instead would leave it holding a slot under `MAX_COMPLETION_ITEMS` and leave
    // the name in `by_name`'s candidate count, where it decides whether a guess is offered at
    // all. See [`Written::loadable`] for why the cursor's own tree turns this off.
    if !written.loadable {
        return None;
    }

    Some(Ranked {
        group: 1,
        tier,
        internal: is_internal(prefix, label),
        distance: ranking.distance.of(declaration),
        generated: written.generated,
        locality: written.step,
        sequence: 0,
        length: sort_length(prefix, label),
        item: Item {
            label: label.to_owned(),
            detail: Some(render::qualified_name(graph, name)),
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
    use crate::analysis::testing::*;
    use crate::analysis::{
        MAX_COMPLETION_ITEMS, MAX_UNTYPED_CANDIDATES, MAX_UNTYPED_COMPLETION_ITEMS,
    };

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
            object_name(&graph),
            "the top level, for want of one"
        );
        assert!(scope.self_id.is_none());

        let locality = Locality::at(
            &graph,
            missing,
            &HashSet::new(),
            environment::Names::default(),
        );
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
                nesting_name_id: object_name(&graph),
            },
            None,
            &[],
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

    #[test]
    fn a_completion_row_says_which_tier_its_list_came_from() {
        // The tiers reaching a completion row. A row's card is the same card a hover draws,
        // and a `completionItem/resolve` that built it with `precise: true` unconditionally
        // would present every row off the name-based list — every method in the project matched
        // on its name — as certain.
        //
        // The tier is a property of the *list* rather than of the row: every row was offered
        // for the same receiver. It travels on `data`, because by the time the client asks to
        // resolve one, that is all either side still knows about where the list came from.
        let (mut harness, uri) = with_types("");

        let derived = harness.complete(&uri, "\"hi\".upcase.len~");
        let guessed = harness.complete(&uri, "thing.len~");

        let resolve = |harness: &mut Harness, list: &serde_json::Value| {
            let item = list["items"]
                .as_array()
                .and_then(|items| items.first())
                .cloned()
                .expect("a row");
            harness.ask("completionItem/resolve", serde_json::json!(item))["documentation"]["value"]
                .as_str()
                .unwrap_or("(nothing)")
                .to_owned()
        };

        let derived = resolve(&mut harness, &derived);
        assert!(
            derived.contains("String#length"),
            "a derived row still gets its card: {derived}"
        );
        assert!(
            !derived.contains("Matched on the method name"),
            "and must not be presented as a guess: {derived}"
        );

        let guessed = resolve(&mut harness, &guessed);
        assert!(
            guessed.contains("Matched on the method name alone"),
            "a name-matched row has to say so: {guessed}"
        );
    }

    #[test]
    fn a_constructors_keyword_arguments_complete_at_the_call() {
        // The redirect goes through `locator::resolve`, which is also where `Context::Argument`
        // gets the method whose parameters it offers. Before it, `Foo.new(` completed against
        // `Class#new`'s `(*untyped, **untyped)` — a signature with no keywords in it at all.
        let mut harness = Harness::new();
        harness.write(
            "lib/order.rb",
            "class Order\n  def initialize(total:, currency: \"USD\")\n  end\nend\n",
        );
        let caller = harness.write("lib/main.rb", "");
        harness.index();

        let offered = harness.declarations_at(&caller, "Order.new(~)\n");

        assert!(offered.contains(&"total:".to_owned()), "{offered:?}");
        assert!(offered.contains(&"currency:".to_owned()), "{offered:?}");
    }

    #[test]
    fn a_constant_that_is_not_a_namespace_offers_nothing_after_its_colons() {
        // `MAX::` is legal to type and means nothing: an `Integer` has no members to write
        // there. rubydex resolves the receiver to a declaration all the same, and every step
        // that follows — the ancestor walk, the member list — has to answer "not a namespace"
        // rather than assume the id it was handed names one.
        let mut harness = Harness::new();
        let uri = harness.write("app/main.rb", "MAX = 10\n");
        harness.index();

        let offered = harness.suggestions(&uri, "MAX = 10\nMAX::~\n");
        assert!(offered.is_empty(), "{offered:?}");
    }

    #[test]
    fn a_singleton_method_written_on_a_constant_is_scoped_to_that_class() {
        // `def Person.build` is the same method as `def self.build` written from outside the
        // class body, and rubydex records the receiver differently for each. Only the `self`
        // form had a test, so the constant form's `self` could have been anything at all —
        // and inside it `self` is `Person`, which is what decides whether the class's own
        // singleton methods are callable without a receiver.
        let mut harness = Harness::new();
        harness.write(
            "app/person.rb",
            "class Person\n  def self.find\n  end\n  def self.all\n  end\nend\n",
        );
        harness.index();
        let uri = harness.write("app/patch.rb", "");

        let offered = harness.suggestions(&uri, "def Person.build\n  fin~\nend\n");
        assert!(
            offered.contains(&"find".to_owned()),
            "`self` inside `def Person.build` is Person: {offered:?}"
        );
    }

    #[test]
    fn every_receiver_that_can_precede_a_double_colon_is_answered() {
        // `::` after something that is not a namespace is legal to type and means nothing, and
        // each shape reaches a different arm. Left unanswered they are not silence but a
        // *wrong* list — the fall-through would offer whatever the enclosing scope had.
        let mut harness = Harness::new();
        harness.write(
            "app/hr.rb",
            "module HR\n  MAX = 1\n  class Person\nend\nend\n",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // `self::` is legal and rare: the nesting is what it means, so a module's own
        // constants are what it can be followed by.
        // The answer has to be the same one `HR::` gives, from inside the module and from
        // inside one of its methods alike: `::` asks about a namespace, and the namespace is
        // the same either way.
        let named = harness.suggestions(&uri, "HR::~\n");
        assert_eq!(named, vec!["MAX".to_owned(), "Person".to_owned()]);
        assert_eq!(
            // Something has to follow the line: `self::` with only an `end` after it is a
            // *method* call in Prism's recovery, with `::` read as the call operator.
            harness.suggestions(&uri, "module HR\n  self::~\n  X = 1\nend\n"),
            named,
            "in a module body, `self` is the module"
        );
        assert_eq!(
            harness.suggestions(&uri, "module HR\n  def y\n    self::P~\n  end\nend\n"),
            vec!["Person".to_owned()],
            "and inside a method it is still the module that `::` asks about"
        );

        // An instance is not a namespace, and neither is a literal. Both parse.
        for marked in ["\"foo\"::~\n", "HR::Person.new::~\n", "whatever::~\n"] {
            let offered = harness.suggestions(&uri, marked);
            assert!(offered.is_empty(), "{marked:?} offered {offered:?}");
        }
    }

    /// A project whose shape exercises every completion context.
    const OFFICE: &str = "\
module HR
  MAX_STAFF = 50

  class Person
    NAME_LIMIT = 40

    def self.build(name:, age: 1)
      new
    end

    def shout(volume)
      volume
    end

    private

    def secret
    end
  end

  class Manager < Person
    def delegate
    end
  end
end
";

    #[test]
    fn a_namespace_access_offers_what_is_inside_it() {
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // Alphabetical, because nothing has been typed for a length or a match quality to
        // separate them by.
        let found = harness.declarations_at(&uri, "HR::~\n");
        assert_eq!(found, vec!["MAX_STAFF", "Manager", "Person"], "{found:?}");
    }

    #[test]
    fn a_namespace_access_is_narrowed_by_what_has_been_typed() {
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        assert_eq!(harness.declarations_at(&uri, "HR::Pe~\n"), vec!["Person"]);
        // And the nested one, which is where a bare name lookup would have stopped.
        assert_eq!(
            harness.declarations_at(&uri, "HR::Person::NAME~\n"),
            vec!["NAME_LIMIT"]
        );
    }

    #[test]
    fn a_constant_receiver_offers_singleton_methods_and_not_instance_ones() {
        // The distinction rubydex models with a synthetic singleton class, and the reason
        // `Foo.` resolves to `Foo::<Foo>` rather than to `Foo`.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.declarations_at(&uri, "HR::Person.~\n");
        assert!(found.contains(&"build".to_owned()), "{found:?}");
        assert!(!found.contains(&"shout".to_owned()), "{found:?}");
    }

    #[test]
    fn an_expression_sees_the_lexical_scope_and_the_ancestor_chain() {
        let mut harness = Harness::new();
        let uri = harness.write("app/hr.rb", OFFICE);
        harness.index();

        let found = harness.declarations_at(
            &uri,
            &OFFICE.replace("    def delegate\n", "    def delegate\n      ~\n"),
        );
        // Its own method, its parent's, the constants either scope reaches, and the class.
        for expected in [
            "delegate",
            "shout",
            "secret",
            "MAX_STAFF",
            "NAME_LIMIT",
            "Person",
            "Manager",
        ] {
            assert!(
                found.contains(&expected.to_owned()),
                "{expected}: {found:?}"
            );
        }
        // `build` is a singleton method: not callable on an instance, so not offered.
        assert!(!found.contains(&"build".to_owned()), "{found:?}");
    }

    #[test]
    fn an_instance_receiver_is_ranked_by_ancestor_distance() {
        let mut harness = Harness::new();
        harness.write("app/store.rb", ANCESTRY);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // One method per rung, in the order Ruby resolves them: the class, the module it
        // includes, the class it inherits, and `Object` past all three. Alphabetically this
        // reads `audit, global_helper, price, save`, which is what shipped.
        assert_eq!(
            harness.first_rows(&uri, "Store::Item.new.~\n", 10),
            [
                "price  Store::Item#price",
                "audit  Store::Auditable#audit",
                "save  Store::Record#save",
                "global_helper  Object#global_helper",
            ]
        );
    }

    #[test]
    fn a_literal_receiver_leads_with_its_own_class() {
        let mut harness = Harness::new();
        harness.write("app/store.rb", ANCESTRY);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // The finding, in the small: alphabetical order puts `global_helper` first, and it is
        // the one row here that `String` does not declare.
        assert_eq!(
            harness.first_rows(&uri, "\"hi\".~\n", 10),
            ["shout  String#shout", "global_helper  Object#global_helper"]
        );
    }

    #[test]
    fn a_singleton_receiver_leads_with_the_class_own_methods() {
        let mut harness = Harness::new();
        harness.write("app/store.rb", ANCESTRY);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        assert_eq!(
            harness.first_rows(&uri, "Store::Item.~\n", 10),
            [
                "build  Store::Item.build",
                "global_helper  Object#global_helper"
            ]
        );
    }

    #[test]
    fn a_namespace_receiver_leads_with_what_is_nested_in_it() {
        let mut harness = Harness::new();
        harness.write("app/store.rb", ANCESTRY);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // A module's `::` is its own contents and stops there. Alphabetical, because nothing has
        // been typed and a namespace has no ancestor chain to measure — the same seam the
        // untyped receiver sits on, for the same reason.
        //
        // What matters more than the order is the last row: there isn't one from `Object`.
        // rubydex's namespace walk deliberately stops before `Object`'s own members, or `String::`
        // would list every top-level constant in the project — and `Object` is where everything
        // it could not attribute ends up.
        assert_eq!(
            harness.first_rows(&uri, "Store::~\n", 10),
            [
                "Auditable  Store::Auditable",
                "DEFAULT_CURRENCY  Store::DEFAULT_CURRENCY",
                "Item  Store::Item",
                "Record  Store::Record",
            ]
        );

        // A *class* under `::` carries its singleton chain as well, because `Store::Item.build`
        // may also be written `Store::Item::build`. So the nested constant leads, the class's own
        // singleton method follows, and `Object` sits at the bottom where distance puts it.
        //
        // Which makes the two lists above and below asymmetric: `Store.` offers `global_helper`
        // and `Store::` does not, though both name the same module object. That is rubydex's
        // namespace walk rather than a rule stated here, and it is finding B's territory — the
        // fixture's job is to make the seam visible, not to close it.
        assert_eq!(
            harness.first_rows(&uri, "Store::Item::~\n", 10),
            [
                "LIMIT  Store::Item::LIMIT",
                "build  Store::Item.build",
                "global_helper  Object#global_helper",
            ]
        );
    }

    #[test]
    fn a_namespace_receiver_is_ranked_by_how_well_the_name_matches() {
        let mut harness = Harness::new();
        harness.write("app/store.rb", ANCESTRY);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // One letter is enough to separate them, and it has to be the right way round: `Item`
        // starts with it, `Auditable` merely contains it (`aud-i-table`, folded). Alphabetically
        // `Auditable` leads, so this is the one namespace list whose order is a claim rather
        // than the alphabet — and the row a user typed `I` for is the one they get.
        assert_eq!(
            harness.first_rows(&uri, "Store::I~\n", 10),
            ["Item  Store::Item", "Auditable  Store::Auditable"]
        );
    }

    #[test]
    fn an_expression_is_ranked_outwards_from_the_cursor() {
        let mut harness = Harness::new();
        let uri = harness.write("app/store.rb", ANCESTRY);
        harness.index();

        // Two scales meeting on one number. Constants are reached through the lexical nesting
        // and methods through the ancestor chain, so each walk is counted from the cursor
        // rather than laid end to end — otherwise every method in the list would sit below
        // every constant, or the reverse.
        //
        // `Object` is where they touch: it is the last rung of the ancestor chain *and* the
        // outermost lexical scope. It has to be scored as the former, which is why `save` on
        // `Record` outranks `global_helper` here. Score it as the latter and everything rubydex
        // could not attribute — the whole of finding B — lands one step from the cursor.
        assert_eq!(
            harness.first_rows(&uri, &ANCESTRY.replace("      audit\n", "      ~\n"), 12),
            [
                "LIMIT  Store::Item::LIMIT",
                // The receiver is implicit here, so Ruby permits both of these and the three
                // receiver lists above must not carry either. That they do not is what makes
                // this pair a guard rather than decoration.
                "initialize  Store::Item#initialize",
                "price  Store::Item#price",
                "stash  Store::Item#stash",
                "Auditable  Store::Auditable",
                "DEFAULT_CURRENCY  Store::DEFAULT_CURRENCY",
                "Item  Store::Item",
                "Record  Store::Record",
                "audit  Store::Auditable#audit",
                "save  Store::Record#save",
                "Object  Object",
                "Store  Store",
            ]
        );
    }

    #[test]
    fn a_class_body_is_ranked_from_the_class_the_cursor_is_writing() {
        let mut harness = Harness::new();
        let uri = harness.write("app/store.rb", ANCESTRY);
        harness.index();

        // The receiver is implicit and `self` is the *class*, not an instance of it — so this
        // list is the mirror of the one above. `build` is offered without a receiver, because
        // that is where `def self.build` can be called from; `initialize`, `price` and `stash`
        // are gone, because none of the three can be written here at all. The pair is the whole
        // point of both assertions: the same fixture, the same names, two cursors, and Ruby
        // permits a different set at each.
        assert_eq!(
            harness.first_rows(
                &uri,
                &ANCESTRY.replace("    LIMIT = 10\n", "    LIMIT = 10\n    ~\n"),
                10
            ),
            [
                "LIMIT  Store::Item::LIMIT",
                "build  Store::Item.build",
                "Auditable  Store::Auditable",
                "DEFAULT_CURRENCY  Store::DEFAULT_CURRENCY",
                "Item  Store::Item",
                "Record  Store::Record",
                "Object  Object",
                "Store  Store",
                "String  String",
                // Last of the declarations, which is where `Object` belongs: everything rubydex
                // could not attribute lands there, and a class body is one keystroke from being
                // the list finding B is about.
                "global_helper  Object#global_helper",
            ]
        );
    }

    #[test]
    fn an_untyped_receiver_falls_back_to_the_alphabet_when_nothing_is_nearer() {
        let mut harness = Harness::new();
        let uri = harness.write("app/store.rb", ANCESTRY);
        harness.index();

        // The seam, pinned deliberately. The typed and untyped paths share a ranking
        // constructor and nothing else: one walks a receiver's ancestor chain, the other is a
        // flat name search over the graph with no receiver in it at all. There is no distance
        // where there is no chain, so this list is exactly what it was — which is the case
        // against leaving it as a list, not an argument that it is fine.
        //
        // Every candidate is in the one file this fixture has, so `Locality` scores them all
        // alike and says nothing — which is the point. A ranking term that invents an order
        // where there is no information would be worse than the alphabet, not better.
        //
        // The receiver is written after the last `end` on purpose. Put it inside the method and
        // Prism's recovery eats that `end` instead, refiling every later top-level class one
        // level deeper: the answer is the same six names, spelled `Store::String#shout`.
        assert_eq!(
            harness.first_rows(&uri, &format!("{ANCESTRY}@foo.~\n"), 12),
            [
                "audit  Store::Auditable#audit",
                "build  Store::Item.build",
                "global_helper  Object#global_helper",
                "price  Store::Item#price",
                "save  Store::Record#save",
                "shout  String#shout",
            ]
        );
    }

    #[test]
    fn an_untyped_receiver_is_ranked_by_how_near_the_file_is() {
        let mut harness = Harness::new();
        let uri = harness.write("app/store.rb", ANCESTRY);
        harness.write("app/near.rb", "class Near\n  def zzz_near\n  end\nend\n");
        harness.write("lib/far.rb", "class Far\n  def aaa_far\n  end\nend\n");
        harness.index();

        // The two names are spelled to sort the wrong way round on purpose: `aaa_far` is the
        // alphabetically first method in the whole project and it belongs last, `zzz_near` is
        // the last and belongs above it. Nothing else here can tell them apart — both are the
        // user's own code, neither matches a prefix, and there is no receiver to measure a
        // chain from.
        assert_eq!(
            harness.first_rows(&uri, &format!("{ANCESTRY}@foo.~\n"), 12),
            [
                "audit  Store::Auditable#audit",
                "build  Store::Item.build",
                "global_helper  Object#global_helper",
                "price  Store::Item#price",
                "save  Store::Record#save",
                "shout  String#shout",
                "zzz_near  Near#zzz_near",
                "aaa_far  Far#aaa_far",
            ]
        );
    }

    /// A project with a spec tree that declares three names nothing else does, and one the
    /// application declares too.
    ///
    /// `spec_only_helper` is the shape the corpora are full of: a `def` at the top level of a
    /// spec file, or inside an `RSpec.describe … do` block, which rubydex files on `Object`
    /// because a block body is not a namespace. `Object` is the end of every ancestor chain, so
    /// one of these is on the list for every receiver in the project.
    fn a_project_with_a_spec_tree() -> (Harness, DocUri, DocUri) {
        let mut harness = Harness::new();
        let app = harness.write("app/store.rb", ANCESTRY);
        harness.write(
            "app/extra.rb",
            "class Object\n  def spec_shared\n  end\nend\n",
        );
        harness.write(
            "spec/support/helpers.rb",
            "def spec_only_helper\nend\n\n\
             module SpecOnly\n  def spec_module_method\n  end\nend\n\n\
             class SpecOnlyDouble\nend\n\n\
             class Object\n  def spec_shared\n  end\nend\n",
        );
        let spec = harness.write("spec/store_spec.rb", "1\n");
        harness.index();
        (harness, app, spec)
    }

    #[test]
    fn a_name_only_a_spec_declares_is_not_offered_to_the_application() {
        // Ruby will not find any of these from a model: the file they are written in is loaded
        // by RSpec and by nothing else. `spec_shared` is the control — the application declares
        // it too, and a spec reopening a class does not take the class away.
        //
        // **Both paths, because they reach a spec by different routes.** A bare word asks
        // `self`'s ancestors, and every one of these projects ends that chain at an `Object`
        // the spec tree has written on; an untyped receiver asks [`by_name`], which matches the
        // whole graph on letters and needs no chain at all.
        let (mut harness, app, _) = a_project_with_a_spec_tree();

        assert_eq!(
            harness.first_rows(&app, &format!("{ANCESTRY}spec_~\n"), 8),
            ["spec_shared  Object#spec_shared"]
        );
        assert_eq!(
            harness.first_rows(&app, &format!("{ANCESTRY}@foo.spec_~\n"), 8),
            ["spec_shared  Object#spec_shared"]
        );
    }

    #[test]
    fn a_class_only_a_spec_declares_is_not_offered_either() {
        // The fence is a fact about where a declaration was written, not about what kind it is,
        // so it reaches the constant list by the same line. A double nobody outside the suite
        // can construct is not a name to offer in `app/`.
        let (mut harness, app, _) = a_project_with_a_spec_tree();

        let offered = harness.declarations_at(&app, &format!("{ANCESTRY}Spec~\n"));
        assert!(
            !offered.iter().any(|label| label == "SpecOnlyDouble"),
            "{offered:?}"
        );
    }

    #[test]
    fn the_same_names_are_offered_to_a_cursor_inside_the_test_tree() {
        // The other half, and the reason this is a fence rather than a filter: a developer
        // editing a spec is exactly who a helper in the next spec file over is the answer for.
        // `environment::fenced_from` settles it for the name rung too; the list follows.
        let (mut harness, _, spec) = a_project_with_a_spec_tree();

        assert_eq!(
            harness.first_rows(&spec, "spec_~\n", 8),
            [
                // `length` decides between two rows nothing else separates, which is why the
                // shorter name leads. See `order`.
                "spec_shared  Object#spec_shared",
                "spec_only_helper  Object#spec_only_helper",
            ]
        );
        assert_eq!(
            harness.first_rows(&spec, "@foo.spec_~\n", 8),
            [
                "spec_shared  Object#spec_shared",
                "spec_module_method  SpecOnly#spec_module_method",
            ]
        );
        let offered = harness.declarations_at(&spec, "Spec~\n");
        assert!(
            offered.iter().any(|label| label == "SpecOnlyDouble"),
            "{offered:?}"
        );
    }

    #[test]
    fn a_declaration_with_no_definitions_is_loadable_rather_than_unproven() {
        // The fence fires on **evidence** — every definition under a test tree — and rubydex's
        // own built-ins have no definitions at all: `Object` and `Module` are in the graph
        // without a line of anybody's Ruby behind them. Written as a count rather than as a
        // running `bool` for exactly this: the first spelling started at *not loadable* and had
        // nothing to flip it, so a built-in fell out of every list drawn outside a test tree.
        let (harness, app, _) = a_project_with_a_spec_tree();
        let graph = &harness.analysis.graph;
        let own = harness.analysis.own_documents();
        let locality = Locality::at(
            graph,
            UriId::from(app.as_str()),
            &own,
            environment::Names::default(),
        );
        assert!(locality.fenced, "a cursor in app/ is what the fence is for");

        let mut undefined = 0;
        for (_, declaration) in graph.declarations().iter() {
            if !declaration.definitions().is_empty() {
                continue;
            }
            undefined += 1;
            assert!(
                locality.of(graph, declaration).loadable,
                "{} was fenced on no evidence",
                declaration.name()
            );
        }
        assert!(undefined > 0, "no built-in to check the rule against");
    }

    #[test]
    fn a_gem_is_never_fenced_by_the_test_tree_rule() {
        // The deny-list reads path segments, and a gem's path is not the project's to reason
        // about — `rspec-core` and `minitest` live under directories nobody here named. What
        // keeps them is that `Locality` is built from the *workspace's* documents only, so a
        // definition it has never scored is loadable rather than suspicious.
        let (dir, _elsewhere, env) = project_with_gem(
            "module Shouty\n  class Horn\n    def spec_from_a_gem\n    end\n  end\nend\n",
        );
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let uri = harness.write("app/main.rb", "1\n");
        harness.write("spec/support/helpers.rb", "def spec_only_helper\nend\n");
        harness.index();
        harness.index_gems();

        let offered = harness.suggestions(&uri, "@foo.spec_~\n");
        assert!(
            offered.iter().any(|label| label == "spec_from_a_gem"),
            "a gem's own method was fenced: {offered:?}"
        );
        assert!(
            !offered.iter().any(|label| label == "spec_only_helper"),
            "{offered:?}"
        );
    }

    #[test]
    fn a_migration_s_own_names_are_offered_to_nobody_outside_it() {
        // The project's own tree, so this is the `own` loop's flag rather than the whole-graph
        // pass — the two halves of the same tag, and a test written against a gem's engine
        // would pass without this one. `db/migrate` is on no autoload path: the task loads one
        // file, by path, and the helper somebody wrote above `def change` is reachable from
        // that file and nowhere else.
        let mut harness = Harness::new();
        harness.write(
            "db/migrate/20180101000000_backfill_stories.rb",
            "def shouty_from_a_migration\nend\n",
        );
        let uri = harness.write("app/main.rb", "1\n");
        harness.index();

        let offered = harness.suggestions(&uri, "shouty_~\n");
        assert!(
            !offered
                .iter()
                .any(|label| label == "shouty_from_a_migration"),
            "a tree the migration task runs one file of is offered to nobody: {offered:?}"
        );
    }

    #[test]
    fn a_gem_s_generator_template_is_fenced_and_the_rest_of_the_gem_is_not() {
        // The other half of the test above, and the one place `own` is the wrong set. A gem's
        // `lib/rack/test/` is a published library, so a document `Locality` never scored has to
        // be loadable — but the tree `rails generate` copies **out** of is not a library in a
        // gem any more than it is in the project, and fabrication's
        // `.../cucumber_steps/templates/fabrication_steps.rb` puts a top-level `def with_ivars`
        // on `Object`, which is an ancestor of every receiver in the workspace. So the template
        // tag is read of the whole graph and the test tag stays read of `own`.
        //
        // **The cursor is bare on purpose**: that is the path `Locality` decides on its own. A
        // receiver reaches the same name through `locator`, which reads the path directly and
        // was already fenced, so a test written with one would pass without this.
        let (dir, _elsewhere, env) = project_with_gem_file(
            "lib/generators/shouty/cucumber_steps/templates/shouty_steps.rb",
            "def shouty_from_a_template\nend\n",
        );
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let uri = harness.write("app/main.rb", "1\n");
        harness.index();
        harness.index_gems();

        let offered = harness.suggestions(&uri, "shouty_~\n");
        assert!(
            !offered
                .iter()
                .any(|label| label == "shouty_from_a_template"),
            "a tree the generator copies out of is offered to nobody: {offered:?}"
        );
    }

    /// A constant that holds an *object* is not a class object.
    ///
    /// Upstream promotes a constant used as a receiver into a `Namespace::Todo` — rubydex's
    /// spelling for "a namespace I never saw a definition of" — which has a singleton class
    /// whose ancestors are `Class`, `Module` and `Object`. `ENV: RBS::Unnamed::ENVClass` and
    /// `URI::RFC2396_PARSER: URI::RFC2396_Parser` are both that shape, and completing against
    /// the singleton answered `alias_method` and `attr_accessor` for `ENV.` — precisely, wrongly,
    /// and *instead of* the name-based list, which had been answering. Fifteen lobsters
    /// positions, fourteen of them `ENV.fetch`.
    ///
    /// The fixture is `ENV`'s shape written out, and what it pins is both halves: the members
    /// of the class the signature says it holds are offered, and the singleton's own are not.
    ///
    /// **The first half used to be the name rung and is now the type**, which is the whole of
    /// what `types::held_by` added: the declaration `HOLDER: Vault::Store` says what the object
    /// is, so the decoy `unlock` below — which the name rung would have offered beside the real
    /// one — is gone. The second half is unchanged and is what the `Todo` costs if nobody asks
    /// the signature.
    #[test]
    fn a_constant_that_holds_an_object_is_not_a_class_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(
            signatures.join("core/s.rbs"),
            "module Vault
  module Store
    def unlock: () -> String
  end
end

             HOLDER: Vault::Store
",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let uri = harness.write(
            "app/main.rb",
            "class Decoy
  def unlock
  end
end

HOLDER.unlock
",
        );
        harness.index();
        harness.index_gems();

        let offered = harness.complete(&uri, "HOLDER.unl~");
        let labels: Vec<&str> = offered["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|item| item["label"].as_str())
            .collect();
        assert!(
            labels.contains(&"unlock"),
            "the class the signature names answers: {labels:?}"
        );
        let owners: Vec<&str> = offered["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter(|item| item["label"].as_str() == Some("unlock"))
            .filter_map(|item| item["detail"].as_str())
            .collect();
        assert_eq!(
            owners,
            ["Vault::Store#unlock"],
            "and it answers from the type rather than from every `unlock` in the graph"
        );
        let singleton_only = harness.complete(&uri, "HOLDER.attr_acc~");
        let wrong: Vec<&str> = singleton_only["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|item| item["label"].as_str())
            .filter(|label| *label == "attr_accessor")
            .collect();
        assert!(
            wrong.is_empty(),
            "a constant holding an object is not a Module: {wrong:?}"
        );
    }

    /// The same constant, typed by the Ruby that assigns it rather than by a signature.
    ///
    /// The sibling of the test above and the reason both exist: no signature will ever declare
    /// an application's own configuration object, and the line that builds it says the same.
    /// Completion is the surface where the `Todo` singleton cost the most — an empty list or
    /// `attr_accessor` — so it is where the second half of the rung is pinned too.
    #[test]
    fn a_constant_the_ruby_assigns_is_not_a_class_object_either() {
        let mut harness = Harness::new();
        harness.write(
            "app/vault.rb",
            "module Vault
  class Store
    def unlock
    end
  end
end

class Decoy
  def unlock
  end
end
",
        );
        let uri = harness.write("app/main.rb", "HOLDER = Vault::Store.new\nHOLDER.unlock\n");
        harness.index();

        let offered = harness.complete(&uri, "HOLDER.unl~");
        let owners: Vec<&str> = offered["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter(|item| item["label"].as_str() == Some("unlock"))
            .filter_map(|item| item["detail"].as_str())
            .collect();
        assert_eq!(
            owners,
            ["Vault::Store#unlock"],
            "the class the assignment names answers, and the decoy the name rung would have \
             offered beside it is gone"
        );

        let singleton_only = harness.complete(&uri, "HOLDER.attr_acc~");
        let wrong: Vec<&str> = singleton_only["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|item| item["label"].as_str())
            .filter(|label| *label == "attr_accessor")
            .collect();
        assert!(wrong.is_empty(), "and it is still not a Module: {wrong:?}");
    }

    #[test]
    fn an_extend_is_read_wherever_it_is_written() {
        // `extend` is not unread: rubydex indexes it and attaches it to the singleton class
        // exactly as Ruby does. What
        // it loses is **one shape** — an `extend` in an `.rbs` whose module name is qualified —
        // and four probes over one workspace are what isolated it: Ruby resolves `extend Flat`,
        // `extend Ns::Fmt` and `include Ns::Fmt`; RBS resolves `extend Flat` and
        // `include Ns::Fmt`; only RBS's `extend Ns::Fmt` does not.
        //
        // The fixture is `stdlib/securerandom/0/securerandom.rbs` written out, because that is
        // the call the report made — and 15 of the 22 `extend`s in the vendored signatures are
        // qualified, so this is the commonest of them rather than the only one.
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(
            signatures.join("core/s.rbs"),
            "module Random\n  module Formatter\n    def hex: (?Integer) -> String\n  end\nend\n\n\
             module Joined::Deep\n  def joined: () -> String\nend\n\n\
             module Flat\n  def flat: () -> String\nend\n\n\
             module SecureRandom\n  extend Random::Formatter\nend\n\n\
             module JoinExt\n  extend Joined::Deep\nend\n\n\
             module FlatExt\n  extend Flat\nend\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        // The same edge written in Ruby, which rubydex resolves on its own and must keep doing:
        // the repair must never be the only thing answering a shape rubydex already handles.
        harness.write(
            "app/rb.rb",
            "module Ns\n  module Fmt\n    def in_ruby\n    end\n  end\nend\n\n\
             class Written\n  extend Ns::Fmt\nend\n",
        );
        let source = "SecureRandom.hex\nJoinExt.joined\nFlatExt.flat\nWritten.in_ruby\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();

        for (needle, owner) in [
            ("hex", "Random::Formatter#hex"),
            ("joined", "Joined::Deep#joined"),
            ("flat", "Flat#flat"),
            ("in_ruby", "Ns::Fmt#in_ruby"),
        ] {
            let found = card(&mut harness, &uri, source, needle);
            assert!(found.contains(owner), "{needle}: {found}");
            assert!(
                !found.contains("possible definitions"),
                "{needle} is still a candidate list: {found}"
            );
            assert!(
                !found.contains("Matched on the method name alone"),
                "{needle} resolves rather than guessing: {found}"
            );
        }

        // The completion side of the same edge. Resolution takes the first answer and
        // completion collects them all, so the walk is shared and
        // the list is asked here as well as the card.
        let offered = harness.complete(&uri, "SecureRandom.he~");
        let labels: Vec<&str> = offered["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|item| item["label"].as_str())
            .collect();
        assert!(labels.contains(&"hex"), "{labels:?}");
    }

    #[test]
    fn a_call_on_a_class_object_is_never_offered_an_instance_method_of_an_unrelated_class() {
        // The fallback filter, and it is the half a user meets first. The name-based list
        // was every declaration in the graph ending in this name; a class object answers on its
        // singleton chain, which the search above already walked, so a candidate owned by a
        // `class` is provably unreachable and one owned by a `module` is exactly what has to
        // stay — a concern's `ClassMethods` is a module's instance method.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "app/models/widget.rb",
            "class Widget\n  def spin\n  end\n\n  def whirl\n  end\nend\n",
        );
        harness.write(
            "app/lib/spinner.rb",
            "module Spinner\n  def spin\n  end\nend\n",
        );
        harness.write(
            "app/models/gadget.rb",
            "class Gadget\n  def self.spin\n  end\nend\n",
        );
        let source = "\
class Story
  spin
  whirl
end
";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        // Two of the three candidates survive: a module's instance method, because that is what
        // an `extend` installs, and another class's **singleton** method, because a class object
        // is what it would be called on. Only `Widget#spin` is dropped.
        let narrowed = card(&mut harness, &uri, source, "spin\n");
        assert!(narrowed.contains("2 possible definitions"), "{narrowed}");
        assert!(narrowed.contains("Gadget.spin"), "{narrowed}");
        assert!(narrowed.contains("Spinner#spin"), "{narrowed}");
        assert!(
            !narrowed.contains("Widget#spin"),
            "an instance method of an unrelated class is not reachable here: {narrowed}"
        );

        // Never emptied. `whirl` is an instance method of a class and nothing else, and a guess
        // is still the honest answer where the graph holds only those.
        let kept = card(&mut harness, &uri, source, "whirl");
        assert!(kept.contains("Widget#whirl"), "{kept}");
        assert!(kept.contains("Matched on the method name alone"), "{kept}");
    }

    #[test]
    fn a_class_body_completes_the_class_methods_of_every_concern_it_includes() {
        // The same cursor that *resolves* `validates` completes to an empty list without this,
        // because resolution takes one answer and completion collects: `query::completion_candidates` walks the singleton's
        // ancestors, the `extend` is on none of them, and nothing was there to be offered.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        let uri = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        harness.index();

        for (typed, expected) in [
            ("valid", "validates"),
            ("sco", "scope"),
            ("belongs", "belongs_to"),
            ("counts", "counts_by"),
        ] {
            let offered = harness.declarations_at(
                &uri,
                &format!("class Story < ApplicationRecord\n  {typed}~\nend\n"),
            );
            assert!(
                offered.contains(&expected.to_owned()),
                "{typed} offers no {expected}: {offered:?}"
            );
        }

        // The decline, which is the same gate read from the collecting side: `Plain` has no
        // nested `ClassMethods`, so nothing of it is extended onto anything — its `helper` is an
        // instance method of every *record*, and this cursor is a class object. `Odd` is the
        // other shape and it has nothing observable to assert: its `ClassMethods` is a constant
        // rather than a module, so it is declined where a module would have been walked, and
        // the list is what it would be if the constant were not there at all. `ClassMethods`
        // itself *is* offered, as a constant — Ruby resolves one through the cref's ancestors
        // and rubydex says so, which has nothing to do with this edge.
        let body = harness.declarations_at(&uri, "class Story < ApplicationRecord\n  ~\nend\n");
        assert!(!body.contains(&"helper".to_owned()), "{body:?}");
    }

    /// Rails' own idiom, and the one worth measuring before believing: an `include` written
    /// **inside a `def`** in a `ClassMethods` module is for the class the macro is called on,
    /// and rubydex records it as a mixin of the enclosing module. `ActiveModel::SecurePassword`
    /// is where this is really written; the shape is copied exactly.
    const SECURABLE: &str = "\
module Validatable
  def valid?
  end
end

module Securable
  module ClassMethods
    def has_secure_password(attribute = :password)
      include Validatable
    end
  end
end

class ApplicationRecord
  include Securable
end
";

    #[test]
    fn what_a_class_methods_def_includes_is_the_records_and_not_the_class_objects() {
        // Why the walk takes the module's own members rather than its ancestors'. `extend M`
        // really does
        // install `M`'s ancestors' methods, so the ancestor walk is the right reading of Ruby —
        // and over Rails' five core gems 105 `module ClassMethods` blocks hold 2 mixins written
        // in the module body against **15 written inside a `def`**, every one of the 15 meaning
        // the class the macro was called on. `Story.valid?` raises in Ruby.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", SECURABLE);
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        // Written rather than opened, because `hover_at` asks about the file the workspace
        // indexed. It is also asked **before** any completion, which opens a buffer over this
        // one: two requests about one document are two requests about whatever text it holds
        // now, and asking them the other way round measures the last thing typed.
        let source = "Story.valid?\n";
        let uri = harness.write("app/models/probe.rb", source);
        harness.index();

        // Resolution reads the same walk, which is the whole reason the walk is shared: before
        // this rule `Story.valid?` resolved, precisely, to a method Ruby raises on.
        let found = card(&mut harness, &uri, source, "valid?");
        assert!(
            found.contains("Matched on the method name alone"),
            "the name rung is the honest answer here: {found}"
        );

        let offered = harness.declarations_at(&uri, "Story.vali~\n");
        assert!(
            !offered.contains(&"valid?".to_owned()),
            "an instance method of a module a macro includes into the record: {offered:?}"
        );

        // The macro itself is the member the module really declares, and it still answers.
        let macros = harness.declarations_at(&uri, "Story.has_secure~\n");
        assert!(
            macros.contains(&"has_secure_password".to_owned()),
            "{macros:?}"
        );
    }

    #[test]
    fn a_class_object_completes_what_a_concern_extends_onto_it_however_it_is_written() {
        // Three spellings of one receiver, and rubydex answers all three with the singleton —
        // which is why `class_object` hands over what the receiver already holds rather than
        // testing the syntax. `Story::validates` is legal Ruby and rare; it is here because
        // `NamespaceAccess` is the one arm that has to find the singleton for itself.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        // A second document, because a buffer is what the cursor is completing *in*: writing
        // `Story.valid` into `story.rb` replaces the `class Story` that file holds, and the
        // receiver then resolves to nothing and reaches the name-based list — which offers
        // `validates` too, and would have passed this test for the wrong reason.
        let uri = harness.write("app/models/probe.rb", "");
        harness.index();

        for marked in [
            "Story.valid~\n",
            "Story::valid~\n",
            "class Story\n  self.valid~\nend\n",
        ] {
            let offered = harness.declarations_at(&uri, marked);
            assert!(
                offered.contains(&"validates".to_owned()),
                "{marked:?} offers nothing a concern extends: {offered:?}"
            );
        }

        // That the receiver really was resolved, and not fallen back on: `Plain#helper` is an
        // instance method of every record and is on no class object's chain, so the name-based
        // list is the only thing that would offer it here.
        let precise = harness.declarations_at(&uri, "Story.help~\n");
        assert!(!precise.contains(&"helper".to_owned()), "{precise:?}");
    }

    /// `class_methods do` is the majority spelling of the concern edge, and the walk that reads
    /// the minority one reads it unchanged.
    ///
    /// **Nothing in `locator` moved for this.** The gate has always been the nested `module
    /// ClassMethods`, and it was right — what was missing is that `ActiveSupport::Concern` builds
    /// that module from a block, so no file declares it and rubydex, which has no namespace for a
    /// block body, files the `def`s as *instance* members of the concern instead.
    /// `workspace/rails/concerns.rs` writes the module down and the existing walk finds it.
    ///
    /// The six corpora write this spelling **120** times against 17 files holding the other, so
    /// the gate was testing for the minority case.
    /// What the fix does **not** reach, pinned so that it is a fact rather than an assumption.
    ///
    /// rubydex has no namespace for a block body, so a `def` inside `class_methods do` is filed on
    /// the enclosing scope — an *instance* member of the concern — and every including class gets
    /// it on its instance side through the `include` the user wrote. That is wrong in Ruby:
    /// `ActiveSupport::Concern` `module_eval`s the block on a module it then **extends**, so the
    /// method is on the class object and nowhere else.
    ///
    /// Declaring the `ClassMethods` module adds the right answer; it cannot take the wrong one
    /// away, because the wrong one is a real definition in the graph and no generated declaration
    /// overwrites a definition. What a `def` in a block is owned by is rubydex's to record, and
    /// nothing here can supply it. **The class side does not double up**, which is the thing that
    /// could have gone wrong here and did not: `Ledger.tally_by` is the generated member alone.
    #[test]
    fn a_class_methods_def_is_still_on_the_instance_side_where_rubydex_filed_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/tallyable.rb", BLOCK_CONCERN);
        let uri = harness.write("app/models/probe.rb", "");
        harness.index();

        assert_eq!(
            harness.first_rows(&uri, "Ledger.new.tally~\n", 8),
            vec![
                "tally_by  Tallyable#tally_by".to_owned(),
                "tally_all  Tallyable#tally_all".to_owned(),
            ],
            "the upstream filing, unchanged — see the docstring"
        );
    }

    #[test]
    fn a_class_methods_block_is_the_same_edge_as_a_nested_class_methods_module() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/tallyable.rb", BLOCK_CONCERN);
        let uri = harness.write("app/models/probe.rb", "");
        harness.index();

        // The **container** is the assertion, not the label: a label alone would pass on the
        // name rung, where `Tallyable#tally_all` — rubydex's own filing of the same `def` — is
        // sitting one module away. `Ledger.tally_by` is the class side, which only the fan-out
        // onto the includer puts there.
        let rows = harness.first_rows(&uri, "Ledger.tally~\n", 8);
        assert_eq!(
            rows,
            vec![
                "tally_by  Ledger.tally_by".to_owned(),
                "tally_all  Ledger.tally_all".to_owned(),
            ],
            "both public defs of the block, on the class that includes the concern"
        );

        // The three declines, each a different sentence of Ruby. `named_private` and
        // `after_private` are the file's own visibility, both spellings. `not_extended` is a
        // `def self.` — a singleton method of the `ClassMethods` module, which is the thing being
        // extended rather than a thing extending, so `extend` installs it nowhere.
        for declined in ["named_private", "after_private", "not_extended"] {
            let offered = harness.declarations_at(&uri, &format!("Ledger.{declined}~\n"));
            assert!(
                !offered.contains(&declined.to_owned()),
                "{declined} is not extended onto the includer: {offered:?}"
            );
        }

        // A `class_methods do` written in a **class** raises `NoMethodError` in Ruby —
        // `class_methods` is defined on `ActiveSupport::Concern`, which is extended onto modules.
        // `Ledger` writes one and it declares nothing.
        let never = harness.declarations_at(&uri, "Ledger.never~\n");
        assert!(!never.contains(&"never_reached".to_owned()), "{never:?}");
    }

    /// The member the block declares is a place, and the place is the `def` the user wrote.
    ///
    /// This is the half a generated declaration usually cannot have: `synthesized.rs` maps one
    /// back to a real source span only where a generator recorded one, and here the `def` is
    /// really in the file — so the jump lands on the line itself rather than on a macro that
    /// implied it.
    #[test]
    fn a_class_methods_def_is_a_place_and_the_place_is_the_def() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        let concern = harness.write("app/models/concerns/tallyable.rb", BLOCK_CONCERN);
        let uri = harness.write(
            "app/models/probe.rb",
            "class Probe\n  Ledger.tally_all\nend\n",
        );
        harness.index();

        let jump =
            harness.definition_at(&uri, "class Probe\n  Ledger.tally_all\nend\n", "tally_al");
        let spelled = serde_json::to_string(&jump).expect("json");
        assert!(
            spelled.contains(concern.as_str()),
            "the jump is into the concern's own file: {spelled}"
        );
        let line = BLOCK_CONCERN
            .lines()
            .position(|text| text.trim() == "def tally_all")
            .expect("the fixture writes it");
        assert!(
            spelled.contains(&format!("\"line\":{line}")),
            "the place is the `def` itself, line {line}: {spelled}"
        );

        // **The line is not what proves it and neither is the file.** rubydex files this same
        // `def` as `Tallyable#tally_all`, an instance member of the concern, so the name rung
        // answers the very same line — a jump that looks right for a reason that is wrong.
        // What the two differ on is the *tier*: the walk resolves the receiver's singleton
        // chain and the name rung says so in a footnote.
        let card = harness.hover_at(&uri, "class Probe\n  Ledger.tally_all\nend\n", "tally_al")
            ["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            card.contains("in `class_methods do` in `Tallyable`, which `Ledger` includes"),
            "the card says which concern installed it and onto what: {card}"
        );
        assert!(
            !card.contains("Matched on the method name alone"),
            "not the name rung: {card}"
        );
    }

    #[test]
    fn one_name_two_concerns_is_one_row_and_a_constant_in_a_class_methods_is_none() {
        // Two declines the walk owes rubydex's own. A member declared by two concerns is offered
        // once, by the nearer — `include Countable` is written after `include Recountable`, so
        // Ruby's linearization puts Countable first and `collect_members`' dedup would have kept
        // exactly that one. And a **constant** nested in a `ClassMethods` is not a row at all:
        // `extend` installs methods, and `Countable::ClassMethods::LIMIT` is reachable through
        // the constant path and never through the singleton.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        let uri = harness.write("app/models/probe.rb", "");
        harness.index();

        let rows = harness.first_rows(&uri, "Story.counts~\n", 8);
        assert_eq!(
            rows,
            vec!["counts_by  ApplicationRecord.counts_by".to_owned()],
            "one row, on the class that includes the concern"
        );

        let constants = harness.declarations_at(&uri, "Story.LIM~\n");
        assert!(!constants.contains(&"LIMIT".to_owned()), "{constants:?}");
    }

    #[test]
    fn a_concerns_class_method_ranks_where_an_included_modules_member_ranks() {
        // The ranking half. `Distance::from_receiver` seeds only the chains
        // rubydex is about to walk, and a concern's `ClassMethods` is on none of them — so
        // every one of its members arrived at `NO_DISTANCE`: last, equally last, and *behind
        // `Object`'s own methods*, which is backwards for the name a model body is most likely
        // to be typing. The seed is `locator::Extends::step`, which counts the classes the
        // chain passes through rather than the ancestors, because the instance chain and the
        // singleton chain have different lengths.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        // An `Object` method matching the same prefix, which every class object answers because
        // `Object` is the end of every chain. It is the row the concern's has to beat.
        harness.write(
            "app/lib/patches.rb",
            "class Object\n  def counts_everything\n  end\nend\n",
        );
        let uri = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        harness.index();

        let rows = harness.first_rows(&uri, "class Story < ApplicationRecord\n  counts~\nend\n", 2);
        assert_eq!(
            rows,
            vec![
                "counts_by  ApplicationRecord.counts_by".to_owned(),
                "counts_everything  Object#counts_everything".to_owned(),
            ],
            "a concern's class method sits one step out, not last"
        );
    }

    #[test]
    fn a_class_that_writes_its_own_class_method_is_not_offered_a_concerns_as_well() {
        // The dedup rule, per member rather than per request: what a concern extends is only
        // ever what the ordinary walk did not answer. Two rows of one name would be the visible
        // failure; the invisible one is which of them the editor accepts on `tab`.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        let source = "class Story < ApplicationRecord\n  def self.validates(*names)\n  end\nend\n";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        let rows = harness.first_rows(
            &uri,
            "class Story < ApplicationRecord\n  def self.validates(*names)\n  end\n\n  valid~\nend\n",
            8,
        );
        let named: Vec<&String> = rows
            .iter()
            .filter(|row| row.starts_with("validates "))
            .collect();
        assert_eq!(
            named,
            vec![&"validates  Story.validates".to_owned()],
            "the class's own, once: {rows:?}"
        );
    }

    #[test]
    fn an_instance_of_a_model_is_never_offered_what_a_concern_extends_onto_its_class() {
        // The discriminator is rubydex's rather than a syntactic test, and this is the other
        // side of it: inside a `def`, and after `Story.new.`, `self` is a record — so the
        // concern edge does not apply and `Plain#helper`, which does not apply in a class body,
        // is exactly what does.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        let uri = harness.write("app/models/probe.rb", "");
        harness.index();

        for marked in [
            "class Story\n  def run\n    valid~\n  end\nend\n",
            "Story.new.valid~\n",
        ] {
            let offered = harness.declarations_at(&uri, marked);
            assert!(
                !offered.contains(&"validates".to_owned()),
                "an instance is not a class object: {marked:?} {offered:?}"
            );
        }

        let instance = harness.declarations_at(&uri, "Story.new.help~\n");
        assert!(instance.contains(&"helper".to_owned()), "{instance:?}");
    }

    #[test]
    fn a_column_completes_below_a_method_the_user_wrote_and_above_ruby_s_own() {
        // The ranking `synthesized.md` says to pin with a real schema in front of it rather
        // than discover from a bug report: what this class was written to do, then what its
        // table says it holds, then what every object can do. The order is the same one it
        // always was; what changed underneath it is why the middle band is in the middle —
        // `Locality` now scores a generated document by the file that implied it, so the
        // columns are the user's code sitting one directory further away rather than
        // somebody else's code altogether. See the test below for the case that separates
        // the two readings.
        let source = "Story.new.\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let model = harness.write(
            "app/models/story.rb",
            "class Story\n  def summary\n  end\nend\n",
        );
        harness.watch(&[&model]);

        assert_eq!(
            harness.declarations_at(&uri, "Story.new.~\n"),
            vec!["summary", "description", "id", "tags", "title", "tap"]
        );
    }

    #[test]
    fn what_the_class_was_written_to_do_leads_what_its_table_says_it_holds_from_anywhere() {
        // **The ordering is a property of the kind of declaration, not of the directory.** A
        // column is declared in `db/schema.rb` and the method beside it in
        // `app/models/story.rb`, so leaving the two to `Locality` decides them by which of those
        // the cursor happens to be nearer — and from a cursor in `db/` that is the schema, which
        // inverts the order the test above pins. `generated` is the term that makes it hold from
        // both ends, and this is the only thing that says so. The audit does not say it and
        // would not: over six corpora and 1,452 typed cursors, deleting the term hands back 18
        // rank-1 answers at a bare cursor against 3 lost from the top ten there and 1 rank-1
        // each at two and three characters, so a run scored on rank 1 alone reads its removal
        // as an improvement. This test is the reason it stays.
        let (mut harness, _schema, _uri) = rails_project("");
        let model = harness.write(
            "app/models/story.rb",
            "class Story\n  def summary\n  end\nend\n",
        );
        // Beside the schema and three steps from the model, which is the whole point of it.
        let near_the_schema = harness.write("db/probe.rb", "Story.new.\n");
        harness.watch(&[&model, &near_the_schema]);

        let rows = harness.declarations_at(&near_the_schema, "Story.new.~\n");
        assert_eq!(
            rows.first().map(String::as_str),
            Some("summary"),
            "the `def` leads from a cursor sitting next to the schema: {rows:?}"
        );
    }

    #[test]
    fn a_column_outranks_a_method_the_project_patched_onto_object() {
        // **The case the fixture above cannot separate, and the one the corpora are full of.**
        // `group` is the first term of the key and it asks one question — is this the user's
        // code — which was answered by whether `Locality` scored the document at all. A
        // generated document has no path, so the answer was *no*, and a method the project
        // patches onto `Object` therefore outranked every column, association and enum of the
        // receiver's own table: `Object` is the far end of the chain and the patch file is the
        // user's code, and the first term never let `distance` speak.
        //
        // Measured on a real application before the fix: two `Object` patches took ranks 1 and
        // 2 of `story.`, above `body`, `score` and `title`.
        let source = "Story.new.\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let patch = harness.write(
            "config/initializers/patch.rb",
            "class Object\n  def aaa_patch\n  end\nend\n",
        );
        harness.watch(&[&patch]);

        let rows = harness.declarations_at(&uri, "Story.new.~\n");
        let column = rows
            .iter()
            .position(|row| row == "title")
            .expect("the column is on the list");
        let patched = rows
            .iter()
            .position(|row| row == "aaa_patch")
            .expect("the patch is on the list");
        assert!(
            column < patched,
            "the table's own column above a patch on Object: {rows:?}"
        );
    }

    #[test]
    fn ruby_keeps_initialize_private_however_it_was_declared() {
        let mut harness = Harness::new();
        let item = harness.write("app/store.rb", ANCESTRY);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // `rb_add_method` privatises five names at the point of definition, so `def initialize`
        // is private whatever its class said — and nothing in the graph records that. rbs is not
        // even consistent about it: `Kernel#initialize_copy` is marked private and
        // `String#initialize_copy` public, so `"hi".` offered `initialize` and `initialize_copy`
        // while `count.` offered only the first.
        let outside = harness.declarations_at(&uri, "Store::Item.new.~\n");
        assert!(outside.contains(&"price".to_owned()), "{outside:?}");
        assert!(!outside.contains(&"initialize".to_owned()), "{outside:?}");

        // And the guard, so this cannot pass by banning the name outright: Ruby 2.7 onwards
        // permits a private call on a receiver written `self`, `initialize` included.
        let inside =
            harness.declarations_at(&item, &ANCESTRY.replace("      audit\n", "      self.~\n"));
        assert!(inside.contains(&"initialize".to_owned()), "{inside:?}");
    }

    #[test]
    fn a_private_method_needs_a_receiver_written_self() {
        let mut harness = Harness::new();
        let item = harness.write("app/store.rb", ANCESTRY);
        harness.index();

        // rubydex passes a private method whenever the caller's `self` is the same *class* as
        // the receiver. Ruby's exemption is for the receiver being *written* `self`, so this
        // offered `stash` from inside `Item` where a real interpreter raises `NoMethodError`.
        let other = harness.declarations_at(
            &item,
            &ANCESTRY.replace("      audit\n", "      Store::Item.new.~\n"),
        );
        assert!(other.contains(&"price".to_owned()), "{other:?}");
        assert!(!other.contains(&"stash".to_owned()), "{other:?}");

        // `::` is a method call too, and Ruby exempts it on the same terms — both halves
        // checked against a real interpreter rather than assumed.
        for marked in ["      self.~\n", "      self::~\n"] {
            let found = harness.declarations_at(&item, &ANCESTRY.replace("      audit\n", marked));
            assert!(found.contains(&"stash".to_owned()), "{marked}: {found:?}");
        }
    }

    #[test]
    fn a_public_method_below_a_block_holding_private_is_still_offered() {
        // The same wrong record the jump reads — a bare `private` written inside a block, which
        // rubydex applies to every `def` below the *block* — asked of the list instead. Both
        // surfaces have to repair it or they disagree about one cursor: a jump that lands on
        // `upsert_custom_fields` and a list that will not offer the name it landed on. See
        // `locator::Modifiers`.
        let concern = "\
module HasCustomFields
  class_methods do
    def custom_fields_for_ids(ids)
      ids
    end

    private

    def custom_field_meta_data
      @custom_field_meta_data
    end
  end

  def upsert_custom_fields(fields)
    fields
  end
end
";
        // **The receiver has to type, or this measures the name-based path instead.** Only
        // `from_graph` carries the repair — `by_name` is the arm that walks the whole graph and
        // is deliberately left with rubydex's record — so the cursor is written into the file as
        // indexed rather than replacing it, which is what keeps the rebase able to place
        // `Category` in the graph's coordinates.
        let source = "\
class Category
  include HasCustomFields

  def initialize
  end

  def go
    audit
  end
end
";
        let mut harness = Harness::new();
        harness.write("app/models/concerns/has_custom_fields.rb", concern);
        let uri = harness.write("app/models/category.rb", source);
        harness.index();

        let offered =
            harness.declarations_at(&uri, &source.replace("    audit\n", "    Category.new.~\n"));
        assert!(
            offered.contains(&"upsert_custom_fields".to_owned()),
            "below the block, so the `private` inside it never reached this one: {offered:?}"
        );
        assert!(
            !offered.contains(&"custom_field_meta_data".to_owned()),
            "and inside the block under that same `private`, which still applies: {offered:?}"
        );
    }

    #[test]
    fn visibility_is_the_callers_visibility_not_the_methods() {
        // Private is free from rubydex, but only if the `self` it is handed is the caller's.
        // Left unstated it defaults to nothing, every call site becomes an outsider, and a
        // class stops being able to see its own private methods.
        let source = "\
class Person
  def self.build
  end

  class << self
    private

    def secret_factory
    end
  end
end
";
        let mut harness = Harness::new();
        let person = harness.write("app/person.rb", source);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let outside = harness.declarations_at(&uri, "Person.~\n");
        assert!(outside.contains(&"build".to_owned()), "{outside:?}");
        assert!(
            !outside.contains(&"secret_factory".to_owned()),
            "{outside:?}"
        );

        let inside = harness.declarations_at(
            &person,
            &source.replace("  def self.build\n", "  def self.build\n    self.~\n"),
        );
        assert!(inside.contains(&"secret_factory".to_owned()), "{inside:?}");
    }

    #[test]
    fn a_singleton_method_body_completes_against_the_singleton() {
        // Inside `def self.build` the lexical scope is `Person` but `self` is `Person`'s
        // singleton class, and the two produce different lists. Getting this wrong offers
        // instance methods that would raise `NoMethodError` if accepted.
        let mut harness = Harness::new();
        let uri = harness.write("app/hr.rb", OFFICE);
        harness.index();

        let found =
            harness.declarations_at(&uri, &OFFICE.replace("      new\n", "      new\n      ~\n"));
        assert!(found.contains(&"build".to_owned()), "{found:?}");
        assert!(!found.contains(&"shout".to_owned()), "{found:?}");
        // Constants still come from the lexical scope, which has not moved.
        assert!(found.contains(&"NAME_LIMIT".to_owned()), "{found:?}");
    }

    #[test]
    fn a_class_body_completes_against_the_class_and_not_an_instance_of_it() {
        // Where the entire Rails DSL lives. `self` in a class body is the class object, so what
        // can be written there is its *singleton* methods — `validates`, `has_many`, `scope`.
        // Completing against the instance side offers `valid?` and omits every macro.
        const MODEL: &str = "\
class Base
  def self.validates(*names)
  end

  def valid?
  end
end

class Post < Base
end
";
        let mut harness = Harness::new();
        let uri = harness.write("app/model.rb", MODEL);
        harness.index();

        let body = harness.declarations_at(
            &uri,
            &MODEL.replace("class Post < Base\n", "class Post < Base\n  vali~\n"),
        );
        assert!(body.contains(&"validates".to_owned()), "{body:?}");
        assert!(!body.contains(&"valid?".to_owned()), "{body:?}");

        // And an instance method body is the other way round, which is the whole distinction.
        let method = harness.declarations_at(
            &uri,
            &MODEL.replace(
                "class Post < Base\n",
                "class Post < Base\n  def go\n    vali~\n  end\n",
            ),
        );
        assert!(method.contains(&"valid?".to_owned()), "{method:?}");
        assert!(!method.contains(&"validates".to_owned()), "{method:?}");
    }

    #[test]
    fn a_class_new_block_inside_a_def_completes_against_the_class_it_opens() {
        // Ruby refuses a `class` keyword in a method body, so `Class.new(base) do … end` is the
        // only way one is ever written there — and rubydex records that block as a class all the
        // same. It is `class_eval`'d, so `self` inside it is the new class *object*: the same
        // answer the written class body above gets, and the enclosing `def` says nothing about
        // it.
        //
        // `types::self_of` read the innermost `def` unconditionally and answered its instance
        // side, so the base's instance members were offered and the DSL — which is the whole
        // reason anybody writes the block — was not. The two bodies nest, so the innermost is
        // the one that decides. Found on chatwoot 2026-09-14 by lane 1's `calls` key, at a
        // `define_method` inside one: the list was a single row and it was
        // `define_singleton_method`, arriving from `Object`.
        const ANONYMOUS: &str = "\
class Base
  def self.validates(*names)
  end

  def valid?
  end
end

def build
  Class.new(Base) do
  end
end
";
        let mut harness = Harness::new();
        let uri = harness.write("app/model.rb", ANONYMOUS);
        harness.index();

        let body = harness.declarations_at(
            &uri,
            &ANONYMOUS.replace(
                "  Class.new(Base) do\n",
                "  Class.new(Base) do\n    vali~\n",
            ),
        );
        assert!(body.contains(&"validates".to_owned()), "{body:?}");
        assert!(
            !body.contains(&"valid?".to_owned()),
            "and not the instance the enclosing `def` runs on: {body:?}"
        );

        // A `def` written *inside* the block is the other way round again, which is the rule
        // rather than a second case: the innermost of the two bodies is that method now.
        let nested = harness.declarations_at(
            &uri,
            &ANONYMOUS.replace(
                "  Class.new(Base) do\n",
                "  Class.new(Base) do\n    def go\n      vali~\n    end\n",
            ),
        );
        assert!(nested.contains(&"valid?".to_owned()), "{nested:?}");
        assert!(!nested.contains(&"validates".to_owned()), "{nested:?}");
    }

    /// A class body, a block written straight into it, and every shape that is not one.
    ///
    /// `DSL` is the shape the defect was reported as: a macro that takes a block and runs it
    /// against something other than the class object. The four other cursors are the refusals,
    /// and each is a different reason — `self` fixed by a `def`, a module with no instances, a
    /// receiver written down, and an expression with no `self` at all.
    const DSL: &str = "\
class Base
  def self.rule(name)
  end

  def self.shared()
  end

  def digits()
  end

  def shared()
  end

  private

  def secret()
  end
end

module Mixin
  included do
  end
end

class Parser < Base
  rule(:colon) do
  end

  def go()
    [1].each do
    end
  end
end
";

    #[test]
    fn a_block_in_a_class_body_offers_the_instance_side_the_class_object_does_not_hold() {
        // The defect: `hover` inside one of these blocks names a member on an instance — see
        // `locator`'s closure rung — and `completion` at the same byte never left the class
        // object, so a card named a method the list beside it did not hold.
        let mut harness = Harness::new();
        let uri = harness.write("app/parser.rb", DSL);
        harness.index();

        let offered = harness.declarations_at(
            &uri,
            &DSL.replace("  rule(:colon) do\n", "  rule(:colon) do\n    ~\n"),
        );
        // The instance side, inherited rungs included — which is the half the name rung was
        // throwing away — and a private one, because no receiver was written.
        assert!(offered.contains(&"digits".to_owned()), "{offered:?}");
        assert!(offered.contains(&"go".to_owned()), "{offered:?}");
        assert!(offered.contains(&"secret".to_owned()), "{offered:?}");
        // The class object's own, which is what `self` still is until the DSL says otherwise.
        assert!(offered.contains(&"rule".to_owned()), "{offered:?}");
        // **Once.** `shared` is on both sides; the class object answers it, so the instance row
        // is not added — the same order `locator` holds by reaching its closure rung only after
        // `resolve_call` came back imprecise.
        assert_eq!(
            offered.iter().filter(|label| *label == "shared").count(),
            1,
            "{offered:?}"
        );
    }

    #[test]
    fn what_a_block_in_a_body_is_not() {
        let mut harness = Harness::new();
        let uri = harness.write("app/parser.rb", DSL);
        harness.index();

        // A `def` fixes `self` and a block inside one cannot unfix it, so this is the instance
        // side because it always was — and the class object's `rule` is *not* on it.
        let in_a_method = harness.declarations_at(
            &uri,
            &DSL.replace("    [1].each do\n", "    [1].each do\n      ~\n"),
        );
        assert!(
            in_a_method.contains(&"digits".to_owned()),
            "{in_a_method:?}"
        );
        assert!(!in_a_method.contains(&"rule".to_owned()), "{in_a_method:?}");

        // A module has no instances for the claim to be about, and `included do` re-binds
        // `self` to the *including* class, which is reachable from neither side of this file.
        let in_a_module = harness.declarations_at(
            &uri,
            &DSL.replace("  included do\n", "  included do\n    ~\n"),
        );
        assert!(
            !in_a_module.contains(&"digits".to_owned()),
            "{in_a_module:?}"
        );
        assert!(
            !in_a_module.contains(&"secret".to_owned()),
            "{in_a_module:?}"
        );

        // A receiver written down says what `self` is not, whatever block it sits in — refused
        // by `Cursor::in_a_closure`, which is `false` on any cursor with one in front of it.
        let written = harness.declarations_at(
            &uri,
            &DSL.replace("  rule(:colon) do\n", "  rule(:colon) do\n    Base.~\n"),
        );
        assert!(written.contains(&"shared".to_owned()), "{written:?}");
        assert!(!written.contains(&"digits".to_owned()), "{written:?}");

        // `::` asks the top level with no `self` at all, and the list it may hold is constants
        // only — so an instance method reaching it would be a syntax error offered as a row. It
        // is refused by the same gate, which is why that gate is written on the context and not
        // on the receiver: this one arrives as a `CompletionReceiver::Expression` like any bare
        // word, and only the context still says a `::` was typed.
        let rooted = harness.declarations_at(
            &uri,
            &DSL.replace("  rule(:colon) do\n", "  rule(:colon) do\n    ::~\n"),
        );
        assert!(rooted.contains(&"Parser".to_owned()), "{rooted:?}");
        assert!(!rooted.contains(&"digits".to_owned()), "{rooted:?}");
    }

    #[test]
    fn the_card_in_a_block_and_the_list_beside_it_name_the_same_member() {
        // The pair the defect is *about*, asked at one byte. Both directions: a name only an
        // instance has comes back as the instance's on both surfaces, and a name both sides
        // have comes back as the class object's on both.
        let source = DSL.replace(
            "  rule(:colon) do\n",
            "  rule(:colon) do\n    secret\n    shared\n",
        );
        let mut harness = Harness::new();
        let uri = harness.write("app/parser.rb", &source);
        harness.index();

        assert_eq!(
            card(&mut harness, &uri, &source, "secret\n"),
            "```ruby\nprivate Base#secret\n```\n\n*Found on an instance of `Parser` — `self` in \
             a block written into a class body is the class object unless whoever takes the \
             block re-binds it, and this name is only on an instance.*"
        );
        assert_eq!(
            card(&mut harness, &uri, &source, "shared\n"),
            "```ruby\nBase.shared\n```"
        );

        let offered = harness.declarations_at(&uri, &source.replace("    secret\n", "    ~\n"));
        assert!(offered.contains(&"secret".to_owned()), "{offered:?}");
        assert!(offered.contains(&"shared".to_owned()), "{offered:?}");
    }

    #[test]
    fn an_argument_list_offers_the_keywords_the_method_takes() {
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // In the order the signature declares them, not alphabetically: a signature is a list
        // and its order is information, unlike a namespace's members.
        let found = harness.declarations_at(&uri, "HR::Person.build(~)\n");
        assert_eq!(&found[..2], ["name:", "age:"], "{found:?}");
    }

    #[test]
    fn keyword_arguments_are_never_guessed_from_a_name() {
        // `person` is a local, so the call resolves by name alone and could be any `build` in
        // the project. Offering `name:` there would be a syntactically valid wrong answer, so
        // the argument list degrades to a plain expression instead.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.declarations_at(&uri, "person = nil\nperson.build(~)\n");
        assert!(!found.contains(&"name:".to_owned()), "{found:?}");
    }

    #[test]
    fn a_receiver_with_no_type_falls_back_to_every_method_name() {
        // The one context ya-lsp cannot answer exactly. It answers with names rather than
        // nothing, because the editor's own word list cannot see a method in an unopened file.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.declarations_at(&uri, "person = nil\nperson.sh~\n");
        assert_eq!(found, vec!["shout"], "{found:?}");
    }

    #[test]
    fn a_name_defined_by_two_classes_is_offered_once() {
        let mut harness = Harness::new();
        harness.write(
            "app/dup.rb",
            "class One\n  def render\n  end\nend\n\nclass Two\n  def render\n  end\nend\n",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.declarations_at(&uri, "thing = nil\nthing.rend~\n");
        assert_eq!(found, vec!["render"], "{found:?}");
    }

    #[test]
    fn the_users_own_code_outranks_a_gems() {
        // The same call `workspace/symbol` makes, for the same reason: a project has thousands
        // of declarations and its bundle has a hundred times that.
        let (dir, _gem_home, env) =
            project_with_gem("class Megaphone\n  def blare_loudly\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write(
            "app/own.rb",
            "class Speaker\n  def blare_softly\n  end\nend\n",
        );
        harness.index();
        harness.index_gems();
        let uri = harness.write("app/main.rb", "");

        let found = harness.declarations_at(&uri, "thing = nil\nthing.blare~\n");
        assert_eq!(found, vec!["blare_softly", "blare_loudly"], "{found:?}");
    }

    #[test]
    fn a_leading_scope_operator_offers_the_top_level_and_only_constants() {
        // rubydex's namespace walk stops before `Object`'s own members, so asking it about
        // `Object` directly answers with nothing. `::` has to be asked as an expression and
        // then filtered, and the filter is not cosmetic: a method or a keyword after `::` is
        // not valid Ruby.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.write(
            "app/top.rb",
            "TOP_LEVEL = 1\n\nclass Standalone\n  def lonely\n  end\nend\n",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.suggestions(&uri, "::~\n");
        assert!(found.contains(&"HR".to_owned()), "{found:?}");
        assert!(found.contains(&"TOP_LEVEL".to_owned()), "{found:?}");
        assert!(found.contains(&"Standalone".to_owned()), "{found:?}");
        assert!(!found.contains(&"lonely".to_owned()), "{found:?}");
        assert!(!found.contains(&"def".to_owned()), "{found:?}");

        assert_eq!(
            harness.suggestions(&uri, "::Standal~\n"),
            vec!["Standalone"]
        );
    }

    #[test]
    fn a_name_meant_to_be_left_alone_sinks() {
        // Ruby writes "internal" with an underscore, and underscores sort before letters — so
        // without the rule the first thing a Rails user sees after `Model.` is `__send`.
        let mut harness = Harness::new();
        harness.write(
            "app/thing.rb",
            "class Thing\n  def self.build\n  end\n\n  def self._internal\n  end\nend\n",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        assert_eq!(
            harness.declarations_at(&uri, "Thing.~\n")[..2],
            ["build", "_internal"]
        );
        // Typing the underscore means it: someone who writes `_` is asking for exactly these.
        assert_eq!(
            harness.declarations_at(&uri, "Thing._~\n"),
            vec!["_internal"]
        );
    }

    #[test]
    fn a_name_rubydex_invented_is_never_offered() {
        // `Class.new` gets called `<uri>:<offset><anonymous>`, which is not something anyone can
        // type. Measured in a 17,557-file workspace, `::` answered with a page of them.
        let mut harness = Harness::new();
        harness.write(
            "app/dyn.rb",
            "Widget = Class.new do\n  def spin\n  end\nend\n\nclass Named\n  class << self\n  end\nend\n",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.suggestions(&uri, "::~\n");
        assert!(found.contains(&"Named".to_owned()), "{found:?}");
        assert!(found.iter().all(|label| !label.contains('<')), "{found:?}");
        // And the same names are kept out of the symbol picker, which shares the test.
        assert!(
            harness
                .symbol_names("anonymous")
                .iter()
                .all(|name| !name.contains('<')),
        );
    }

    #[test]
    fn keywords_are_offered_in_an_expression_and_nowhere_else() {
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        assert!(
            harness
                .suggestions(&uri, "de~\n")
                .contains(&"def".to_owned())
        );
        // After a `.` a keyword is not a legal thing to write.
        assert!(
            !harness
                .suggestions(&uri, "HR::Person.de~\n")
                .contains(&"def".to_owned())
        );
    }

    #[test]
    fn a_comment_is_answered_with_null_and_an_empty_scope_with_a_list() {
        // Two different "nothing", and the difference is not cosmetic: `null` tells the client
        // to fall back to its own word list, an empty list tells it not to.
        let mut harness = Harness::new();
        let uri = harness.write("app/main.rb", "");

        assert!(harness.complete(&uri, "# take ~\n").is_null());
        assert!(harness.complete(&uri, "\"a string ~\"\n").is_null());

        let empty = harness.complete(&uri, "Nowhere::~\n");
        assert_eq!(empty["items"], serde_json::json!([]));
    }

    #[test]
    fn the_half_typed_word_is_replaced_rather_than_appended_to() {
        // Without an explicit edit range the client guesses the word boundaries from its own
        // pattern, and Ruby's `?`, `!` and `@` are exactly where that guess goes wrong.
        let mut harness = Harness::new();
        harness.write(
            "app/hr.rb",
            "class Person\n  def empty?\n  end\n\n  def go\n    @name = 1\n  end\nend\n",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.complete(&uri, "thing = nil\nthing.empty?~\n");
        let edit = &found["items"][0]["textEdit"];
        assert_eq!(edit["newText"], "empty?");
        assert_eq!(
            edit["range"],
            serde_json::json!({
                "start": { "line": 1, "character": 6 },
                "end": { "line": 1, "character": 12 },
            }),
            "{found}"
        );
    }

    #[test]
    fn a_list_the_cap_did_not_touch_is_complete() {
        // The flag costs the client a whole request per keystroke, so it is set where it is
        // load-bearing rather than everywhere: only the cap can drop a row that a longer prefix
        // would have reached, because every filter on the way here is a subsequence match.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        assert_eq!(harness.complete(&uri, "HR::~\n")["isIncomplete"], false);
        assert_eq!(harness.complete(&uri, "sh~\n")["isIncomplete"], false);
    }

    #[test]
    fn a_list_with_nothing_in_it_is_still_incomplete() {
        // Not the same statement as the one above read twice. A route that answers with no rows
        // does so because there was nothing to say, and "the complete answer is nothing" would
        // have the client stop asking as the word grows.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.complete(&uri, "Undefined::~\n");
        assert_eq!(found["items"].as_array().map(Vec::len), Some(0), "{found}");
        assert_eq!(found["isIncomplete"], true, "{found}");
    }

    #[test]
    fn a_completion_list_is_capped() {
        let mut harness = Harness::new();
        let mut source = String::new();
        for index in 0..MAX_COMPLETION_ITEMS + 100 {
            source.push_str(&format!("class Thing{index}\nend\n"));
        }
        harness.write("app/many.rb", &source);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.complete(&uri, "Thing~\n");
        assert_eq!(
            found["isIncomplete"], true,
            "a list the cap cut is the one case the client must re-ask about"
        );
        assert_eq!(
            found["items"].as_array().map(Vec::len),
            Some(MAX_COMPLETION_ITEMS)
        );
    }

    /// Enough distinct method names to put the whole-project guess over its admission ceiling,
    /// plus a family that lands between the two ceilings and one name nothing else matches.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn three_regimes_of_guess() -> String {
        let mut source = String::new();
        for index in 0..MAX_UNTYPED_CANDIDATES + 50 {
            source.push_str(&format!(
                "class Thing{index}\n  def alpha{index}\n  end\nend\n"
            ));
        }
        // `beta` matches none of the `alpha` names and neither matches `zebra_stripe`, so each
        // prefix below names exactly one of the three regimes.
        for index in 0..MAX_UNTYPED_COMPLETION_ITEMS + 72 {
            source.push_str(&format!(
                "class Other{index}\n  def beta{index}\n  end\nend\n"
            ));
        }
        source.push_str("class Marked\n  def zebra_stripe\n  end\nend\n");
        source
    }

    #[test]
    fn a_guess_too_wide_to_read_is_not_offered_at_all() {
        // The name-based list is what a receiver with no type falls to, and at an empty prefix
        // it is every method name in the project — 26,073 of them on the smallest corpus swept,
        // 45,953 on the largest, with the word the file actually wrote at rank 4,070.
        //
        // Two ceilings and not one, because the measurement splits them: over five corpora the
        // word sits inside the first `MAX_UNTYPED_COMPLETION_ITEMS` rows of every guess holding
        // up to `MAX_UNTYPED_CANDIDATES` candidates, 430 of 430 — so what cannot be trusted is
        // the *count*, and what can is the order. Hence: refuse on candidates, cut on rows.
        let mut harness = Harness::new();
        harness.write("app/many.rb", &three_regimes_of_guess());
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // `thing` is a local nothing assigns, so there is no receiver to type and no chain.
        let declined = harness.complete(&uri, "thing.~\n");
        assert_eq!(
            declined["items"].as_array().map(Vec::len),
            Some(0),
            "a guess this wide is not a completion list: {declined}"
        );
        assert_eq!(
            declined["isIncomplete"], true,
            "and saying it is complete would stop the client asking as the word grows"
        );

        // Between the two ceilings: few enough candidates to believe the ranking, more rows than
        // anybody reads. The answer is the best of them rather than all of them or none.
        let cut = harness.complete(&uri, "thing.beta~\n");
        assert_eq!(
            cut["items"].as_array().map(Vec::len),
            Some(MAX_UNTYPED_COMPLETION_ITEMS),
            "a guess the ranking can be trusted with is cut to what is readable"
        );
        assert_eq!(cut["isIncomplete"], true);

        // And under both, the whole of it: the same receiver, the same absent type, a prefix
        // narrow enough that what is left is a list a person would read to the end.
        let narrowed = harness.complete(&uri, "thing.zebra~\n");
        let labels: Vec<&str> = narrowed["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|item| item["label"].as_str())
            .collect();
        assert_eq!(
            labels,
            vec!["zebra_stripe"],
            "the guess returns whole once it is narrow"
        );
    }

    #[test]
    fn a_declined_guess_and_a_typed_list_are_not_two_lengths_of_the_same_answer() {
        // `GALLERY`'s treatment for the four answers a `.` can get, in one picture, because the
        // point of this item is that they are **different kinds of answer** rather than different
        // lengths of one. Read the right-hand column down:
        //
        // - `"hi".upca` has a receiver the graph names. Those rows are `String`'s own members and
        //   the ceiling they are bounded by is the response's.
        // - `thing.` has no receiver at all, so the candidates are every method name in the
        //   project attached to no class. Nothing is offered, because at an empty prefix `tier`
        //   is 1 for every row and `length` is 0 — there is no order to keep the best of.
        // - `thing.beta` has the same absent receiver, and enough of a word that the ranking can
        //   be trusted: few enough candidates to admit, more rows than anyone reads, so it is
        //   cut to what is readable rather than refused or sent whole.
        // - `thing.zebra` narrows it to one, and the whole of the guess is the answer.
        //
        // The second row is what used to be 512 rows of the alphabet, and the third is why this
        // is a decline and not a refusal.
        let (mut harness, uri) = with_types("");
        harness.write("app/many.rb", &three_regimes_of_guess());
        harness.index();

        let mut drawn = String::new();
        for marked in ["\"hi\".upca~", "thing.~", "thing.beta~", "thing.zebra~"] {
            let found = harness.complete(&uri, marked);
            let rows = found["items"].as_array().cloned().unwrap_or_default();
            let labels: Vec<&str> = rows
                .iter()
                .filter_map(|item| item["label"].as_str())
                .take(3)
                .collect();
            let shown = if labels.is_empty() {
                "(nothing)".to_owned()
            } else {
                format!("{}: {}", rows.len(), labels.join(", "))
            };
            drawn.push_str(&format!("{:<14} {shown}\n", marked.replace('~', "")));
        }

        assert_eq!(
            drawn.trim_end(),
            "\"hi\".upca      1: upcase
thing.         (nothing)
thing.beta     128: beta0, beta1, beta2
thing.zebra    1: zebra_stripe"
        );
    }

    #[test]
    fn an_exactly_typed_keyword_outranks_a_fuzzy_match_in_the_project() {
        // **`tier` leads the key, and for two releases `group` did.** A name in the user's own
        // code outranked everything else before match quality was consulted at all, so a word
        // typed in full lost to any project name matching it as a *subsequence*. Over 1,452
        // typed cursors on six corpora, putting `tier` first took the words sitting past rank
        // 128 one character into the word from 21 to **2**, and past 256 from 9 to **0**.
        //
        // It is written on a keyword because `group` still has three bands and this is the one
        // the harness can reach: `end` is band 2, below every declaration, which is where the
        // measurement put it — offering keywords first empties rank 1 and the whole top ten at
        // a bare-word cursor. Typed in full it comes back anyway, and that is this test.
        let mut harness = Harness::new();
        harness.write(
            "lib/patch.rb",
            "class Object\n  def extended_node\n  end\nend\n",
        );
        let uri = harness.write("lib/main.rb", "");
        harness.index();

        let rows = harness.suggestions(&uri, "end~\n");
        let keyword = rows
            .iter()
            .position(|row| row == "end")
            .expect("the keyword is offered");
        let fuzzy = rows
            .iter()
            .position(|row| row == "extended_node")
            .expect("and so is the fuzzy match");
        assert!(
            keyword < fuzzy,
            "the exact match leads the subsequence one: {rows:?}"
        );
    }

    #[test]
    fn the_name_based_list_is_what_guess_from_names_turns_off() {
        // `[types] guess_from_names = false` exists so that a user can have only answers this
        // server can defend. The name-based list is the same guess one request over — matched
        // on the word alone, belonging to no class — so the setting has to reach it, or it
        // silences every *card* that says "guessed" and none of the lists built the same way.
        //
        // It gates that arm and nothing else: a receiver the graph can name is not a guess.
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n\n\
                 [types]\nguess_from_names = false\n",
                signatures.display().to_string()
            ),
        )
        .unwrap();
        let mut off = Harness::at(dir, PositionEncoding::Utf16);
        let uri = off.write("lib/main.rb", "");
        off.index();
        off.index_gems();

        let silenced = off.complete(&uri, "thing.len~");
        assert_eq!(
            silenced["items"].as_array().map(Vec::len),
            Some(0),
            "with the rung off there is no name-based list: {silenced}"
        );

        let typed = off.complete(&uri, "\"hi\".upca~");
        let labels: Vec<&str> = typed["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|item| item["label"].as_str())
            .collect();
        assert!(
            labels.contains(&"upcase"),
            "and a receiver the graph names is untouched by it: {labels:?}"
        );
    }

    #[test]
    fn resolving_an_item_fills_in_its_documentation() {
        let mut harness = Harness::new();
        harness.write(
            "app/hr.rb",
            "class Person\n  # Says it loudly.\n  def shout(volume)\n  end\nend\n",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.complete(&uri, "thing = nil\nthing.shou~\n");
        let item = found["items"][0].clone();
        // The list itself carries no documentation: five hundred rows, one of them read.
        assert!(item["documentation"].is_null(), "{item}");

        let resolved = harness.ask("completionItem/resolve", item);
        let markdown = resolved["documentation"]["value"]
            .as_str()
            .unwrap_or_default();
        assert!(markdown.contains("Says it loudly"), "{resolved}");
        assert!(markdown.contains("shout(volume)"), "{resolved}");
    }

    #[test]
    fn resolving_an_item_the_graph_no_longer_holds_answers_the_item_it_was_given() {
        // A list is built, the configuration reloads, the graph is dropped and rebuilt, and the
        // user then arrows down onto a row from the old list. The `data` on that row is a
        // declaration id nothing answers to any more — which arrives from the client, so it is
        // handed to the server rather than produced by it, and rubydex ids are hashes with no
        // way to tell a stale one from a wrong one. The protocol says the item comes back
        // either way; enriching it is the optional half.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", "class Person\nend\n");
        harness.index();

        for data in [
            // A well-formed row naming a declaration that is gone.
            serde_json::json!({ "declaration": "1234567890123456789", "precise": true }),
            // And the shapes a client can send that are not rows at all.
            serde_json::json!("1234567890123456789"),
            serde_json::json!({ "declaration": "1234567890123456789" }),
            serde_json::Value::Null,
        ] {
            let resolved = harness.ask(
                "completionItem/resolve",
                serde_json::json!({ "label": "shout", "data": data }),
            );

            assert_eq!(resolved["label"], "shout", "the item comes back regardless");
            assert!(
                resolved["documentation"].is_null(),
                "and carries nothing invented: {resolved}"
            );
        }
    }

    /// One name in every sigil namespace Ruby has, chosen so each would answer the others'
    /// prefixes if a sigil were fuzzy-matched like a letter.
    ///
    /// `entry` is in all five names on purpose: it is the substring that makes every list below
    /// a real question rather than a coincidence of spelling. `$@` is here because it is the
    /// bug this fixture exists for — a global whose whole name after the sigil *is* an `@`, so
    /// a subsequence match on `@` reaches it and nothing else in Ruby does.
    ///
    /// `CURSOR` is where the cursor goes; `sigils_at` writes the line in.
    const SIGILS: &str = "\
$@ = nil
$entry_log = []

class Ledger
  ENTRY_LIMIT = 100

  @@entry_total = 0

  def initialize
    @entries = []
    @entry_note = \"\"
  end

  def record
    entry = 1
    CURSOR
  end
end
";

    /// [`SIGILS`] with `line` written into `#record`'s body, where the `~` marks the cursor.
    ///
    /// One buffer rather than two, unlike `ANCESTRY`: an instance variable belongs to the
    /// `self` it was assigned on, so the cursor has to be inside the same class that assigns it.
    fn sigils_at(line: &str) -> String {
        SIGILS.replace("CURSOR", line)
    }

    #[test]
    fn an_instance_variable_prefix_reaches_no_other_namespace() {
        // The whole list, and what is not in it: no `$@`, which is what shipped through two
        // releases. Nor `$entry_log`, `ENTRY_LIMIT` or either method — every one of them holds
        // `entry`, and none of them is something `@` can be the start of.
        //
        // `@@entry_total` *is* here, and belongs here: one `@` is on the way to two.
        let mut harness = Harness::new();
        let uri = harness.write("app/ledger.rb", "");
        harness.index();

        assert_eq!(
            harness.first_rows(&uri, &sigils_at("@~"), 10),
            [
                "@entries  Ledger#@entries",
                "@entry_note  Ledger#@entry_note",
                "@@entry_total  Ledger#@@entry_total",
            ]
        );
    }

    #[test]
    fn a_class_variable_prefix_does_not_reach_back_to_the_instance_variables() {
        // The other direction, which is the half that must not be symmetric: `@` admits `@@`
        // because the second character may still be coming, and `@@` admits no `@name` because
        // nothing can be typed that turns one into the other.
        let mut harness = Harness::new();
        let uri = harness.write("app/ledger.rb", "");
        harness.index();

        assert_eq!(
            harness.first_rows(&uri, &sigils_at("@@~"), 10),
            ["@@entry_total  Ledger#@@entry_total"]
        );
    }

    #[test]
    fn a_global_prefix_is_the_only_thing_that_reaches_a_global() {
        // And `$@` is a perfectly good answer *here*. The rule is not that it is a bad row, it
        // is that it belongs to one prefix.
        let mut harness = Harness::new();
        let uri = harness.write("app/ledger.rb", "");
        harness.index();

        assert_eq!(
            harness.first_rows(&uri, &sigils_at("$~"), 10),
            ["$entry_log  $entry_log", "$@  $@"]
        );
    }

    #[test]
    fn a_prefix_with_no_sigil_still_reaches_every_namespace() {
        // Deliberately unchanged, and pinned so it stays deliberate. Someone who has typed no
        // sigil has not said which namespace they mean, and a client filters as they keep
        // typing — so `entr` offers the constant, both instance variables, the class variable
        // and the global, and accepting one of them writes the sigil in.
        //
        // What it does not offer is `entry`, the local variable one line above the cursor:
        // rubydex's graph holds no locals, which is why `scopes.rs` walks Prism itself. That is
        // a missing feature rather than a wrong answer, and it is written here because a
        // first-ten list is where an absence is visible at all.
        let mut harness = Harness::new();
        let uri = harness.write("app/ledger.rb", "");
        harness.index();

        assert_eq!(
            harness.first_rows(&uri, &sigils_at("entr~"), 10),
            [
                "ENTRY_LIMIT  Ledger::ENTRY_LIMIT",
                "@entries  Ledger#@entries",
                "@entry_note  Ledger#@entry_note",
                "@@entry_total  Ledger#@@entry_total",
                "$entry_log  $entry_log",
            ]
        );
    }

    #[test]
    fn a_completion_row_says_the_class_it_was_offered_for_was_guessed() {
        // The tier vocabulary, extended to the guess. A guessed receiver is not the
        // name-based list — the rows really are one class's members, which is a better list —
        // so `precise` stays true and a second field carries the doubt. Saying nothing would
        // present six letters of inference as a resolved type.
        let mut harness = Harness::new();
        harness.write(
            "app/models/person.rb",
            "class Person\n  def shout\n  end\nend\n",
        );
        let uri = harness.write("app/main.rb", "");
        harness.index();

        let offered = harness.complete(&uri, "person.sh~");
        let row = offered["items"]
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .expect("a row");
        assert_eq!(row["label"], "shout");
        let card = harness.ask("completionItem/resolve", row)["documentation"]["value"]
            .as_str()
            .unwrap_or("(nothing)")
            .to_owned();
        assert!(card.contains("Person#shout"), "{card}");
        assert!(
            card.contains("Type guessed from the name `person` alone"),
            "{card}"
        );
        assert!(
            !card.contains("Matched on the method name alone"),
            "the rows are a real class's members, and this is the other guess: {card}"
        );
    }
}
