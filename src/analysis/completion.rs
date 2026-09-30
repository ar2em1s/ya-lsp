//! `textDocument/completion`: what can be written where the cursor is.
//!
//! [`cursor`] says what *shape* the cursor is in; this says what the graph offers there. The shape
//! is pure syntax and the offer pure semantics, and only the offer needs a loaded project to test.
//!
//! # What is exact and what is a guess
//!
//! 1. **Three contexts are exact**, because the graph resolved the receiver: `Foo::`, `Foo.`,
//!    `self.`, and a bare word (receiver: the enclosing `self`). rubydex walks the real ancestor
//!    chain and applies real visibility (a `private` method is offered inside its class, not
//!    outside), and for an argument list returns the called method's keyword parameters.
//! 2. **The fourth is `foo.`** where `foo` is a local, an instance variable or another call's
//!    result. [`types`](super::types) answers what it can. If every rung is empty, one is left: the
//!    receiver's spelling read as a class name. That list is a real class's members, and
//!    [`Completion::guess`] says which letters it came from.
//! 3. **When even that finds nothing**, the list falls back to every method name in the project: a
//!    different guess, presented as one. Names only, deduplicated, the user's own code first. Still
//!    better than the editor's word list, which cannot see methods in unopened files.
//!
//! **The two guesses must not be conflated.** `precise` says the rows are one class's members;
//! `guess` says that class was inferred from a name. A guessed receiver is `precise` *and* guessed:
//! a better list than the name-based one, a worse answer than a resolved type, and the card must be
//! able to say both.
//!
//! # When the list is `isIncomplete`
//!
//! Filtering happens here, not in the client, because otherwise the first keystroke would ship a
//! Rails bundle's hundred thousand candidates. So the answer is only correct for the prefix asked,
//! and `isIncomplete` tells the client to ask again rather than narrow what it has.
//!
//! - **Set only when the cap dropped rows.** Every filter here is a *subsequence* match ([`tier`],
//!   and rubydex's `MatchMode::Fuzzy` under [`by_name`]), so a longer prefix admits a subset of
//!   what a shorter one did. An untruncated list is therefore a superset of any fresh query, and
//!   the client can narrow it safely. The client does not keep this module's *ranking*: it
//!   re-scores rows against the longer prefix and uses `sort_text` only as a tiebreak, which is
//!   right, since its score knows what was typed since. The flag matters because each re-ask
//!   repeats the whole lookup.
//! - **An empty list stays incomplete.** Every empty answer means there was nothing to say (a
//!   receiver that is not a namespace, a `::` on something that cannot hold one), and "the complete
//!   answer is nothing" would make the client stop asking as the word grows.

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
    indexed::Placed,
    locator,
    position::Rebase,
    render,
    types::{self, Derivation, Scope, Sources, constant_at, object_name, singleton_of},
    views,
};

/// One suggestion, before it is dressed up as an LSP item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// What gets inserted and what the user reads: `shout`, `Person`, `@name`, `volume:`, `def`.
    pub label: String,
    /// The full spelling, shown beside the label: `HR::Person#shout`.
    pub detail: Option<String>,
    pub kind: Kind,
    /// Filled immediately for keywords, whose documentation is constant. Otherwise it is
    /// `completionItem/resolve`'s job, so a list of 300 does not read 300 comment blocks nobody
    /// looks at.
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
    /// Whether the cap dropped rows, so the client must ask again instead of narrowing this.
    ///
    /// Below the cap the list is complete and a client may filter it itself (the module docs
    /// explain why that is sound). An empty list is always incomplete: it means nothing to say, not
    /// that the answer is nothing.
    pub incomplete: bool,
    pub start: u32,
    pub end: u32,
    /// Whether the receiver these rows were offered for was one ya-lsp could name.
    ///
    /// A property of the *list*, not of a row, because every row was offered for the same receiver.
    /// `false` is the name-based fallback: every method in the project, matched by name, and each
    /// card must say so.
    pub precise: bool,
    /// The receiver's own spelling, when the class was guessed from it.
    ///
    /// The third tier, on a list: `precise` says the rows are a real class's members, and this says
    /// the class itself was a guess. Together they make a row honest about `@user.`: the methods
    /// are `User`'s, and `User` is six letters of inference.
    pub guess: Option<String>,
}

/// The three ceilings a completion answer is bounded by, each a different question.
///
/// A struct, not three adjacent `usize` parameters, because they are easy to transpose and mean
/// different things. `items` is how much JSON a keystroke may cost, for any list this server
/// believes in. The other two bound a **guess**, split because the ranking is trustworthy where the
/// row count is not: `untyped_candidates` decides whether to guess at all, and `untyped` how much
/// of the guess is worth reading. A typed receiver reads neither, so nothing is bounded twice.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// `MAX_COMPLETION_ITEMS`: the response ceiling for every offered list.
    pub items: usize,
    /// `MAX_UNTYPED_COMPLETION_ITEMS`: how many rows of the name-based list are sent. See
    /// [`by_name`].
    pub untyped: usize,
    /// `MAX_UNTYPED_CANDIDATES`: above this many candidates the name-based list is not offered at
    /// all, however few rows would be sent.
    pub untyped_candidates: usize,
}

/// The keys one segment on from what a literal key already says: inside
/// `t("users.|")`, the keys under `users`, replacing the segment being written. A key the table
/// holds more keys under is a segment; the rest end the key.
fn keyed(
    sources: &Sources<'_>,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    offset: u32,
    rebase: &Rebase,
    limits: Limits,
) -> Option<Completion> {
    let source = text.source();
    let (literal, member) = locator::resolve_keyed(sources, uri_id, text, offset, rebase)?;
    let typed =
        source.get(literal.start as usize..(offset as usize).max(literal.start as usize))?;
    let (parent, start) = match typed.rfind('.') {
        Some(at) => (&typed[..at], literal.start + at as u32 + 1),
        None => ("", literal.start),
    };
    let children = sources
        .knowledge
        .modules()
        .find_map(|module| module.keyed_under(&member, parent))?;
    let incomplete = children.len() > limits.items;
    let items = children
        .into_iter()
        .take(limits.items)
        .map(|child| Item {
            label: child.name,
            detail: Some(child.shown.lines().next().unwrap_or_default().to_owned()),
            kind: if child.branch {
                Kind::Module
            } else {
                Kind::Field
            },
            documentation: None,
            deprecated: false,
            declaration: None,
        })
        .collect();
    Some(Completion {
        items,
        incomplete,
        start,
        end: offset,
        precise: true,
        guess: None,
    })
}

/// What can be written at `offset`.
///
/// `None` where nothing can: inside a comment or literal, or in a document the graph has never
/// seen.
#[must_use]
pub(super) fn complete(
    sources: &Sources<'_>,
    uri_id: UriId,
    source: &str,
    offset: u32,
    limits: Limits,
    placed: &Placed,
    rebase: &Rebase,
) -> Option<Completion> {
    // One parse of the buffer for both questions asked of it here ([`cursor::Parsed`]).
    let text = cursor::Parsed::new(source);
    // **Inside a literal key a member looks up**, the keys under what it already says.
    if let Some(keys) = keyed(sources, uri_id, &text, offset, rebase, limits) {
        return Some(keys);
    }
    let graph = sources.graph;
    let cursor = cursor::at(&text, offset)?;
    let prefix = &source[cursor.start as usize..cursor.end as usize];

    // **The receiver's variables are answered from the repaired text.** Which variable a read is
    // depends on the file's structure, and the half-typed call re-nests everything below it (see
    // `Cursor::repaired`). The repaired text has the buffer's offsets, so `rebase` still maps it.
    let own = graph
        .documents()
        .get(&uri_id)
        .map(|document| document.uri().to_owned());
    let repaired = cursor.repaired.as_deref().map(std::rc::Rc::<str>::from);
    let repaired_read = |uri: &str| match (&repaired, &own) {
        (Some(repaired), Some(own)) if own == uri => Some((std::rc::Rc::clone(repaired), *rebase)),
        _ => (sources.read)(uri),
    };
    // Its own memo: the request's read the buffer, and this reads the repaired text.
    let memo = types::Memo::new(&repaired_read, sources.held_exits);
    let sources = &Sources {
        read: &repaired_read,
        memo: &memo,
        ..*sources
    };

    // **The coordinate change, the only one completion needs.** Everything above reads `source`
    // (the buffer); everything below keys the graph, whose offsets index the text last given to the
    // indexer. On a document nobody is typing in they are the same string and `rebase` is the
    // identity (see `Rebase`).
    //
    // Completion can be deferred because it never returns a graph span: `Completion::start`/`end`
    // come from the cursor, in buffer coordinates. `hover` and `definition` return spans *from* the
    // graph and need `Rebase::span_to_buffer` too, which is why they are not deferred.
    let (lo, hi) = match rebase.to_graph(offset) {
        Some(at) => (at, at),
        // The cursor is inside text typed since the index: the ordinary case while typing. A scope
        // survives it (see `Scope::covering`); a receiver lookup may not, which `rebased` below
        // decides.
        None => rebase.changed_in_graph(),
    };
    // `None` where the changed region runs out of a body: the graph cannot say which `class` or
    // `def` the caret is in, so the request declines and is retried against a settled graph instead
    // of answering from the wrong side of `self`.
    //
    // Logged, not counted on a field: how often this fires depends on how someone edits, so the
    // useful number is one a probe collects over a session.
    let Some(scope) = Scope::covering(graph, uri_id, lo, hi) else {
        tracing::debug!("completion declined: the changed region leaves the body the caret is in");
        return None;
    };
    // A receiver in text the graph never held cannot be looked up in it. Refusing is the safety
    // argument: answering anyway would offer another class's members, a wrong answer (seen as
    // `Alpha.new.` offering `Gamma`'s).
    let Some(context) = cursor.context.rebased(rebase) else {
        tracing::debug!("completion declined: the receiver is inside text the graph has not seen");
        return None;
    };
    // Pure syntax, decided before any lookup; see `Context::allows_private`.
    let private_ok = context.allows_private();
    // Empty until a candidate rubydex calls private arrives, which most lists never have, so only
    // the lists the repair is about pay for it.
    let modifiers = &sources.memo.modifiers;
    // Read from the layout, not passed as a ninth argument: `Sources` already carries the value
    // every fencing surface takes, and letting the halves of a fence drift apart is a defect this
    // crate has had before.
    let locality = Locality::at(graph, uri_id, placed, sources.layout);
    let mut precise = true;
    let mut guess = None;
    let (items, incomplete) = match receiver_for(sources, uri_id, &context, &scope, lo) {
        Some((receivers, only, derivation)) => {
            // The tier travels with the list: it belongs to the *receiver*, and every row was
            // offered for the same one. A guessed receiver still offers a real class's members
            // (better than matching every method by name), and each card must still say where the
            // class came from.
            guess = derivation.guess;
            // Each receiver's own chain is numbered in `from_graph`, which sets `distance`.
            let mut ranking = Ranking {
                prefix,
                distance: Distance::none(),
                locality,
                private_ok,
                modifiers: Some(modifiers),
            };
            let view = in_view(sources, uri_id, &cursor.context);
            let closure = InClosure::at(graph, &scope, cursor.in_a_closure);
            from_graph(
                graph,
                receivers,
                only,
                limits.items,
                &mut ranking,
                &view,
                closure.as_ref(),
            )
        }
        // Only one context arrives here with anything to say: a `.` on an untyped receiver. `foo::`
        // and a non-namespace receiver have no honest answer.
        //
        // **And `sources.guess` decides whether even that may answer.** These rows are the
        // name-based guess in another form (matched on the word alone, attached to no class), so
        // `[types] guess_from_names = false` must reach here, or the setting would silence the tier
        // on every card but not in lists. It gates only this arm: a typed receiver is not a guess.
        None => match &cursor.context {
            Context::MethodCall { .. } if sources.guess => {
                // No receiver means no chain, so `Distance` is silent and `Locality` is the only
                // nearness, but neither ranks this list: `tier` does. Measured over untyped cursors
                // on six corpora, reordering the terms behind `tier` (even deleting them) returns
                // the same lists, with the typed word at median rank 1 from the third character on.
                // A guess still broad enough for those terms to matter is *declined* by `admitted`
                // rather than ordered. The one ordering that hurts is lifting `Locality` above
                // `tier`, so the only risk here is putting nearness before what the user typed. See
                // `Distance::none`.
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
            // Nothing to say, which differs from an empty answer: calling this list complete would
            // make the client stop asking as the word grows.
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
    /// After a `::`, where a method or keyword would not be legal.
    Constants,
}

/// How many classes a union receiver may be and still complete: a sanity guard.
///
/// A union is asked of rubydex once per class, so this bounds the walks one keystroke makes. It
/// sits above the widest union the corpora complete on, a concern's `self` (one class per
/// includer). Past it the receiver is untyped and the name rung answers, as for every union
/// before.
const MAX_UNION_CLASSES: usize = 64;

/// Turn a classified cursor into the questions rubydex answers: one, or one per class of a union.
///
/// `None` means no exact question: an unknown receiver, or a `::` on something that is not a
/// namespace. `at` is the cursor in graph coordinates.
fn receiver_for(
    sources: &Sources<'_>,
    uri_id: UriId,
    context: &Context,
    scope: &Scope,
    at: u32,
) -> Option<(Vec<CompletionReceiver>, Only, Derivation)> {
    let graph = sources.graph;
    let (receiver, only, derivation) = match context {
        // `caller`, not `scope.self_id`, by necessity: rubydex's `expression_completion` needs a
        // `self` type, and with `None` it collects no methods and no instance variables at all,
        // which would break the commonest completion. The nesting's own declaration must be passed.
        Context::Expression => Some((
            CompletionReceiver::Expression {
                self_decl_id: scope.caller(graph),
                nesting_name_id: scope.nesting,
            },
            Only::Everything,
            Derivation::default(),
        )),
        Context::Argument { name } => {
            // Only a receiver rubydex could name gives real keyword arguments. A name-based guess
            // would put another class's parameters into this call: syntactically valid and wrong,
            // worse than none.
            //
            // `Context::Argument` *is* the implicit-receiver case (the cursor is inside a call's
            // parentheses, not after an operator), so a private callee is reachable here, as
            // `Context::allows_private` says.
            //
            // The same evidence the jump reads: a keyword argument from a `def` a block filed on
            // `Object` would be a parameter list the call cannot take. `locator::Blocks` is empty
            // until a hit lands on a root, so other cursors pay nothing.
            let blocks = &sources.memo.blocks;
            let receiver = match locator::precise_call(
                graph,
                uri_id,
                *name,
                sources.layout,
                locator::Privacy::Allowed,
                blocks,
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
                Receiver::Constant(offset) => constant_at(graph, uri_id, *offset, sources.layout)?,
                // `self::CONST` is legal and rare, and means the nesting. `caller` is the singleton
                // in a class or module body, and asking a singleton reaches its attached class, so
                // this answers the same list as `HR::`, from a body or a method.
                // `every_receiver_that_can_precede_a_double_colon_is_answered` pins it, since the
                // equivalence is rubydex's.
                Receiver::SelfObject(_) => scope.caller(graph)?,
                // `"foo"::Bar`, `Foo.new::Bar` and `foo.bar::Baz` all parse and mean nothing anyone
                // writes on purpose. An instance is not a namespace.
                //
                // `Receiver::Named` joins them: a name is at best an instance, and both rungs that
                // read one answer with a class, not a namespace.
                Receiver::Instance { .. }
                | Receiver::Literal { .. }
                | Receiver::Returned { .. }
                | Receiver::Yielded { .. }
                | Receiver::Yield { .. }
                | Receiver::BlockGiven { .. }
                | Receiver::Proc { .. }
                | Receiver::ProcParameter { .. }
                | Receiver::Assigned { .. }
                | Receiver::Destructured { .. }
                | Receiver::Spelled { .. }
                // `super::Bar` parses and means nothing: `super` returns a value, and a value is
                // not a namespace. `(a || b)::Bar` likewise (a shortcut returns one of its
                // operands, which are values), and `param::Bar` (a parameter holds whatever the
                // caller passed).
                | Receiver::Super { .. }
                | Receiver::Shortcut { .. }
                // `!x::Bar` parses too, and `!x` is `true` or `false`: a value, and the least
                // likely values to be a namespace.
                | Receiver::Negated(_)
                // `(c ? A : B)::X` and an exception caught are values, like a shortcut.
                | Receiver::Either(_)
                | Receiver::Rescued(_)
                | Receiver::Parameter { .. }
                // A variable holds a value, and `x::Bar` is a namespace only if its writes were
                // constants, which reads of it spell as `Foo::Bar` instead.
                | Receiver::Variable(_)
                | Receiver::Named(_) => return None,
                // `::Foo` asks for the top level, which is `Object`, but rubydex's namespace walk
                // deliberately stops *before* Object's own members, so `String::` does not list
                // every top-level constant. Asking the same question as a top-level expression
                // reaches them, and dropping non-constants leaves what `::` can be followed by.
                Receiver::TopLevel => {
                    return Some((
                        vec![CompletionReceiver::Expression {
                            self_decl_id: None,
                            nesting_name_id: object_name(graph),
                        }],
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
        // Every arm lives in `types::method_receiver`, because navigation asks the same question
        // and the two must agree: what `person.` is cannot depend on whether the user typed or
        // hovered.
        //
        // **A union offers every class's members** (decided 2026-09-30). A call on it runs on each
        // class that has the member (`types::narrowed`), and Ruby raises on the rest, so each row
        // is right for the values that have it; the detail names its class. `nil` stays folded
        // out, as for a `T?`.
        Context::MethodCall { receiver } => {
            let typed = types::method_receiver(sources, uri_id, receiver, scope)?;
            let classes = typed.classes();
            if classes.len() > MAX_UNION_CLASSES {
                return None;
            }
            let self_decl_id = scope.caller(graph);
            let receivers = classes
                .iter()
                .map(|&receiver_decl_id| CompletionReceiver::MethodCall {
                    self_decl_id,
                    receiver_decl_id,
                })
                .collect();
            return Some((receivers, Only::Everything, typed.derivation));
        }
    }?;
    let mut receivers = vec![receiver];
    if matches!(context, Context::Expression | Context::Argument { .. }) {
        receivers.extend(rebound(sources, uri_id, at, scope.nesting));
    }
    Some((receivers, only, derivation))
}

/// The classes a block around a bare word runs against, as receivers beside the lexical one.
///
/// - **Hover asks them first** (`locator::rebound_call`, [`types::rebound_self`]): an RSpec example
///   group and what `config.include` adds to it, a concern's `included do`, a signature's
///   `[self: T]`. So `let` in a `describe` block and `eq` in an example are on the list.
/// - **Merged as a union's are** ([`one_row_per_name`]), each row naming its class. The lexical
///   rows stay: hover falls back to them where the rebound `self` lacks the name.
/// - **Refused (`Some(None)`) or wider than [`MAX_UNION_CLASSES`] adds nothing**, and the lexical
///   list stands, as hover's lexical answer does.
fn rebound(
    sources: &Sources<'_>,
    uri_id: UriId,
    at: u32,
    nesting: NameId,
) -> Vec<CompletionReceiver> {
    types::rebound_self(sources, uri_id, at)
        .flatten()
        .filter(|typed| typed.classes().len() <= MAX_UNION_CLASSES)
        .map(|typed| {
            typed
                .classes()
                .iter()
                .map(|&class| CompletionReceiver::Expression {
                    self_decl_id: Some(class),
                    nesting_name_id: nesting,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Everything rubydex offers for the receivers, filtered to the prefix and capped.
///
/// One receiver, except on a union: then each class is asked with its own chain numbered
/// (`ranking.distance` is set per receiver), and [`one_row_per_name`] merges the lists.
///
/// The flag is `Completion::incomplete`: true where the cap dropped rows, and true where there was
/// nothing to say at all, which differs from "the answer is empty".
fn from_graph(
    graph: &Graph,
    receivers: Vec<CompletionReceiver>,
    only: Only,
    limit: usize,
    ranking: &mut Ranking,
    view: &[views::Reached],
    closure: Option<&InClosure>,
) -> (Vec<Item>, bool) {
    let union = receivers.len() > 1;
    let mut ranked = Vec::new();
    let mut answered = false;
    for receiver in receivers {
        // Collected before ranking, because the ranking reads two of the three, and merged after
        // it (see [`Beside`]). `Extended` is built once and read twice: its walk answers both what
        // a concern extends onto a class object and how far out it sits.
        let beside = Beside {
            extended: Extended::at(graph, &receiver),
            view,
            closure,
        };
        ranking.distance =
            Distance::from_receiver(graph, &receiver, beside.extended.as_ref(), beside.view);
        if let Some(rows) = ranked_for(graph, receiver, only, ranking, &beside) {
            ranked.extend(rows);
            answered = true;
        }
    }
    if !answered {
        return (Vec::new(), true);
    }
    let twins = if union {
        let (rows, twins) = one_row_per_name(ranked);
        ranked = rows;
        twins
    } else {
        HashMap::new()
    };
    let truncated = take_best(&mut ranked, limit);
    let mut items = spelled_out(graph, ranked);
    for item in &mut items {
        let others = twins.get(&item.label).into_iter().flatten();
        for declaration in others.filter_map(|id| graph.declarations().get(id)) {
            let detail = item.detail.get_or_insert_with(String::new);
            detail.push_str(" | ");
            detail.push_str(&render::qualified_name(graph, declaration.name()));
        }
    }
    (items, truncated)
}

/// One row per name on a union, and the other classes' declarations of each name, for the detail.
///
/// - **The row kept is the best by [`order`], ranked at the farthest distance any class gave the
///   name.** A name every class has is mostly what every object has, and the nearest would be the
///   shortest chain's: an `Array`'s `Object` sits three steps out, a model's sixty, so `frozen?`
///   would rank above the model's own `save`. Each class's own members keep their own distance.
/// - **A name two classes declare apart** (`as_json` on a model and on an `Array`) is one row,
///   whose detail names both: the label is what is inserted, and a list of twins is noise.
fn one_row_per_name(ranked: Vec<Ranked>) -> (Vec<Ranked>, HashMap<String, Vec<DeclarationId>>) {
    let mut named: HashMap<String, Vec<Ranked>> = HashMap::new();
    for entry in ranked {
        named
            .entry(entry.item.label.clone())
            .or_default()
            .push(entry);
    }
    let mut twins = HashMap::new();
    let rows = named
        .into_iter()
        .map(|(label, mut rows)| {
            let farthest = rows.iter().map(|row| row.distance).fold(0, u16::max);
            rows.sort_by(order);
            // Never empty: a name is here because a row carried it.
            let mut kept = rows.remove(0);
            kept.distance = farthest;
            let mut seen: HashSet<DeclarationId> = kept.item.declaration.into_iter().collect();
            let others = rows
                .iter()
                .filter_map(|row| row.item.declaration)
                .filter(|id| seen.insert(*id))
                .collect();
            twins.insert(label, others);
            kept
        })
        .collect();
    (rows, twins)
}

/// Whether `id` is a module, whose instance is some class's.
fn is_module(graph: &Graph, id: DeclarationId) -> bool {
    matches!(
        graph.declarations().get(&id),
        Some(Declaration::Namespace(Namespace::Module(_)))
    )
}

/// `Object`, asked beside a module's instance.
///
/// - **A module's instance is some class's, and every class descends from `Object`**, so the
///   member lookup asks `Object` where the module lacks a name (`types::member_of`), and `self.class`
///   in a module's `def` is `Kernel#class`. rubydex's walk of the module stops at its own ancestors.
/// - **Its rows fill gaps and never shadow** (in [`ranked_for`]): a name the module has is the
///   module's. They are numbered past the module's chain ([`Distance::from_receiver`]), so they
///   follow the module's own.
/// - **A bare word in a module's `def` has the same `self`**, so it is asked as an expression on
///   `Object`: private `Kernel` methods (`format`, `raise`) are what a bare word may call.
fn objects_side(graph: &Graph, receiver: &CompletionReceiver) -> Option<CompletionReceiver> {
    let object = DeclarationId::from("Object");
    match receiver {
        CompletionReceiver::MethodCall {
            self_decl_id,
            receiver_decl_id,
        } => is_module(graph, *receiver_decl_id).then_some(CompletionReceiver::MethodCall {
            self_decl_id: *self_decl_id,
            receiver_decl_id: object,
        }),
        CompletionReceiver::Expression {
            self_decl_id: Some(module),
            nesting_name_id,
        }
        | CompletionReceiver::MethodArgument {
            self_decl_id: Some(module),
            nesting_name_id,
            ..
        } => is_module(graph, *module).then_some(CompletionReceiver::Expression {
            self_decl_id: Some(object),
            nesting_name_id: *nesting_name_id,
        }),
        _ => None,
    }
}

/// One receiver's rows, before the cap: rubydex's answer and the lists beside it. `None` where
/// rubydex could not answer for it.
fn ranked_for(
    graph: &Graph,
    receiver: CompletionReceiver,
    only: Only,
    ranking: &Ranking,
    beside: &Beside,
) -> Option<Vec<Ranked>> {
    let object = objects_side(graph, &receiver);
    let candidates = match query::completion_candidates(graph, CompletionContext::new(receiver)) {
        Ok(candidates) => candidates,
        Err(error) => {
            // A receiver that is not a namespace after all. Nothing to say, nothing broken.
            tracing::debug!("no completion candidates: {error}");
            return None;
        }
    };

    let mut ranked: Vec<Ranked> = candidates
        .iter()
        .filter(|candidate| only.accepts(graph, candidate))
        .enumerate()
        .filter_map(|(sequence, candidate)| rank(graph, candidate, sequence, ranking))
        .collect();
    let held: HashSet<u64> = ranked
        .iter()
        .map(|entry| StringId::from(&entry.item.label).get())
        .collect();
    let objects: Vec<CompletionCandidate> = object
        .into_iter()
        .flat_map(|object| {
            query::completion_candidates(graph, CompletionContext::new(object))
                .into_iter()
                .flatten()
        })
        .collect();
    ranked.extend(
        objects
            .iter()
            .filter(|candidate| only.accepts(graph, candidate))
            .enumerate()
            .filter_map(|(sequence, candidate)| rank(graph, candidate, sequence, ranking))
            .filter(|entry| !held.contains(&StringId::from(&entry.item.label).get())),
    );
    // Before the cap, never after: these rows compete with the graph's on one key, and appending
    // after `take_best` would return `limit` plus however many a concern holds.
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
    // The view context's rows, added the same way and place for the same reason: before the cap,
    // competing with the graph's own on one key.
    add_view(graph, &mut ranked, ranking, beside.view);
    // Last of the three. A template has no class body to write a block in, so this and the view
    // context never both have rows. What the order does state: the concern edge is already merged,
    // so a name a concern extends onto the class object is held against this like any name the
    // graph's own walk found.
    add_closure(graph, &mut ranked, ranking, beside.closure);
    Some(ranked)
}

/// The lists that join the graph's own answer before the cap.
///
/// One value, because they are one idea. rubydex answers for a *receiver*; each of these is a name
/// reachable from the cursor that no receiver names: what a Rails concern extends onto a class
/// object, what Rails puts into a template's view context, and what a block written straight into a
/// class body may run against. Resolution can ask each as a second question, because it takes one
/// answer. Completion must **collect**, so they are merged into the ranked rows before `take_best`,
/// not appended after it.
struct Beside<'a> {
    extended: Option<Extended>,
    view: &'a [views::Reached],
    closure: Option<&'a InClosure>,
}

/// What a bare word in a **template** can complete to.
///
/// `None` for every other document and every context with a written receiver:
/// [`views::Views::reachable`]'s own gate plus one syntax test, since a template's view context
/// answers only an *implicit* receiver (`person.` in a template is still `person`'s).
///
/// The walk is [`views`]', shared with [`locator::resolve_typed`] so a jump and a list cannot
/// disagree about what a template can call. The concern edge shares [`locator::extended_modules`]
/// the same way: resolution takes the first answer, completion collects all.
fn in_view(sources: &Sources<'_>, uri_id: UriId, context: &Context) -> Vec<views::Reached> {
    if !matches!(context, Context::Expression | Context::Argument { .. }) {
        return Vec::new();
    }
    types::view_context(sources, uri_id)
        .map(|reachable| reachable.members(sources.graph))
        .unwrap_or_default()
}

/// Put the view context's rows into the list, taking a name away from `Object` where they share
/// one.
///
/// **The shadowing is Ruby's, not a preference.** A helper module is `include`d into the view class
/// and `Kernel` ends every chain, so `def format` in `ApplicationHelper` really replaces
/// `Kernel#format` for every template. Keeping the graph's row would complete the word and then
/// jump to the wrong definition. It costs a pass over the ranked rows, paid only where a template
/// offered a name the view context also holds.
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
/// - **The list beside `locator`'s closure rung.** That rung (whose docs hold the argument) answers
///   one name: a bare name in `rule(:colon) { … }` may be on the class object (`self` until
///   rebound) or on an instance (what DSLs rebind it to). This offers every name that rung would
///   answer, so `hover` in such a block never names a member the list lacks.
/// - **The two agree per member, which makes the order safe.** `locator` reaches its closure rung
///   only after `resolve_call` came back imprecise, so the class object's own answer (including
///   concern extensions) always wins. [`add_closure`] keeps that order by adding a row only where
///   the list lacks the name, which is why it runs after the concern edge.
/// - **A class, never a module, and never a written receiver.** Both are `locator`'s refusals,
///   placed where each is cheapest. A module has no instances, and its
///   `included do`/`class_methods do` blocks rebind `self` to the *including* class; that refusal
///   is tested here. A written receiver (`Foo.`, `person.`) decides `self` whatever block it is in;
///   that one is [`cursor::at`](cursor::at)'s, so `Cursor::in_a_closure` is `false` on such
///   cursors.
struct InClosure {
    /// The class whose instance the block's `self` may be.
    class: DeclarationId,
    /// The lexical nesting, which rubydex's expression query needs and this does not read: every
    /// row is filtered to a method, and the constants it reaches are already on the class object's
    /// list from the same nesting.
    nesting: NameId,
}

impl InClosure {
    /// `None` wherever either half of the evidence is missing.
    ///
    /// **The scope, not the receiver**, though they hold the same declaration: every receiver
    /// reaching here was built from `Scope::caller`, and a `.` or `::` never reaches here
    /// (`Cursor::in_a_closure` is `false` with a written receiver). Reading the scope states that
    /// once, instead of a match with two untakeable arms.
    fn at(graph: &Graph, scope: &Scope, in_a_closure: bool) -> Option<Self> {
        if !in_a_closure {
            return None;
        }
        // "rubydex called this a class object": the closure rung's test on the same declaration. A
        // bare call in a class body is recorded on the singleton and one inside a `def` is not, so
        // this also keeps method bodies out.
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
    /// - **The question is a receiverless call's in an instance method** (`self` is an instance of
    ///   this class), so rubydex walks the ancestors and reaches a superclass's `def`, the half the
    ///   name rung was dropping.
    /// - **Methods only.** The constants it also collects are the class object's from the same
    ///   nesting, already listed. An instance variable is a different claim: `@x` in a class body
    ///   is the class object's, whatever the block does, because rebinding the receiver does not
    ///   rebind the lexical scope.
    /// - **`held` is tested before the row is built**, as in [`Extended::candidates`]:
    ///   `last_segment` is a slice and the lookup a hash, while [`ranked_declaration`] walks a
    ///   declaration's definitions to score locality. On a controller's chain most names are
    ///   already the class object's.
    /// - **Visibility is [`ranked_declaration`]'s**, and `private_ok` is already true: no receiver
    ///   was written, so private methods are exactly what may be called.
    fn rows(&self, graph: &Graph, ranking: &Ranking, held: &HashSet<u64>) -> Vec<Ranked> {
        let receiver = CompletionReceiver::Expression {
            self_decl_id: Some(self.class),
            nesting_name_id: self.nesting,
        };
        // `unwrap_or_default` where [`from_graph`] logs: the one error here is *the receiver is not
        // a namespace*, and [`InClosure::at`] already checked that the class exists and is a
        // `Namespace::Class`.
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
/// The opposite of [`add_view`]'s shadowing, on purpose. A helper module really replaces
/// `Kernel#format` in a template, but a block's `self` is an **inference** about a DSL, and the
/// class object is what Ruby uses if the inference is wrong. So existing rows stand and this fills
/// gaps, which also keeps `hover` and this list in agreement on every row.
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

/// The members of a module a class object `extend`s that rubydex did not linearize: the collecting
/// half of the repair [`locator`] resolves.
///
/// Resolution can ask a second question after the first fails, because it takes one answer.
/// Completion *collects*, so the same walk runs here and its rows join the list before ranking and
/// the cap.
///
/// The walk is [`locator::extended_modules`], shared so its one shape is stated once. **This is not
/// the Rails concern edge**: `workspace/rails/concerns.rs` declares a concern's class methods onto
/// every including class, so `query::completion_candidates` offers them like any member.
struct Extended {
    /// The singleton the modules were found from, and the receiver rubydex answered for.
    ///
    /// Kept for deduplication: a name this already offers must not be offered again by the edge,
    /// and asking is the same ancestor walk resolution makes.
    on: DeclarationId,
    modules: Vec<locator::Extension>,
}

impl Extended {
    /// `None` where there is no edge to walk: a receiver that is not a class object (every instance
    /// call, every cursor inside a `def`), or a chain with no such module.
    fn at(graph: &Graph, receiver: &CompletionReceiver) -> Option<Self> {
        let on = class_object(graph, receiver)?;
        let modules = locator::extended_modules(graph, on);
        (!modules.is_empty()).then_some(Self { on, modules })
    }

    /// Every member the edge adds, nearest module first.
    ///
    /// - **Deduplicated like rubydex's own walk**: the nearest declaration of a name answers, so a
    ///   second module spelling the same name adds no row.
    /// - **A name the ordinary walk already offers is not added**, the resolution's rule applied
    ///   per member: a class writing its own `def self.validates` keeps it, and an extension only
    ///   ever answers what nothing else did.
    /// - **The prefix is tested first**, as [`ranked_declaration`] orders `tier` before
    ///   `reachable`: the ancestor walk costs many hash lookups on a Rails model's singleton chain,
    ///   a prefix test one string compare, so a keystroke pays only for the rows it could offer.
    fn candidates(&self, graph: &Graph, prefix: &str) -> Vec<DeclarationId> {
        let mut seen: HashSet<StringId> = HashSet::new();
        let mut found = Vec::new();
        for extends in &self.modules {
            let Some((_, extended)) = namespace(graph, extends.module) else {
                continue;
            };
            for (name, member) in extended.members() {
                // `extend` installs methods only: a constant nested in a `ClassMethods` module is
                // not reachable through the singleton.
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
/// The one shape the edge applies to, decided by rubydex, not syntax: a bare call in a class body
/// and an explicit `Foo.` both arrive as the singleton class, while the same call inside an
/// instance `def` arrives as the class.
///
/// `extended_modules` declines non-singletons, so this passes the receiver's declaration without
/// testing it twice. `NamespaceAccess` is the one arm that must look: `Foo::` names the class and
/// rubydex collects its *singleton's* methods, which is also why [`Distance::from_receiver`] seeds
/// that chain there.
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
/// - **The ranking's only relevance term.** Without it, an empty-prefix list is not ranked: `tier`
///   and `length` tie for every row, and the alphabet decides. `"hello".` would open on
///   `DelegateClass, Digest, append_as_bytes, …`, mostly not `String`'s.
/// - **Zero is the receiver's own members, one an included module's**, and `Object`, `Kernel` and
///   `BasicObject` come last as the end of every chain. That also means whatever rubydex misfiles
///   onto `Object` is already as far away as a name can be.
struct Distance {
    steps: HashMap<DeclarationId, u16>,
}

/// What an owner none of the chains reached is worth.
///
/// Last, equally, so the rest of the key still separates them. Every collected candidate comes from
/// one of the chains seeded below, so landing here means the graph disagrees with itself about who
/// owns a name: no benefit of the doubt.
const NO_DISTANCE: u16 = u16::MAX;

impl Distance {
    /// The name-based list has no receiver, so no chain and nothing to measure. Every row ties and
    /// the rest of the key decides.
    fn none() -> Self {
        Self {
            steps: HashMap::new(),
        }
    }

    /// Seed from the same walks `query::completion_candidates` is about to make.
    ///
    /// - **Each walk numbers from zero**, not end to end, because they are different kinds of
    ///   nearness on one scale: a sibling constant in the enclosing module is near the cursor but
    ///   on no ancestor chain, and `Foo::` reaches `Foo::Bar` and `Foo.build` by two routes both
    ///   starting at `Foo`. Numbering end to end would sink every method below every constant, or
    ///   the reverse.
    /// - **Ancestor chains first, nearest wins; the lexical walk only fills gaps.** `Object` is
    ///   both the last rung of every ancestor chain and the outermost lexical scope; scored as the
    ///   latter, its members (everything rubydex could not attribute) would sit one step from the
    ///   cursor.
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
            } => {
                let past = distance.chain_from(graph, *receiver_decl_id, 0);
                // `Object`'s chain after the module's own ([`objects_side`]).
                if is_module(graph, *receiver_decl_id) {
                    distance.chain_from(graph, DeclarationId::from("Object"), past);
                }
            }
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
                // Methods and instance variables come from `self`'s ancestors; constants from the
                // lexical nesting, a walk outwards through owners. Both are seeded, so a class's
                // own methods and its sibling constants are both near.
                if let Some(id) = self_decl_id.or(nesting) {
                    let past = distance.chain_from(graph, id, 0);
                    // `Object`'s chain after a module's own ([`objects_side`]).
                    if is_module(graph, id) {
                        distance.chain_from(graph, DeclarationId::from("Object"), past);
                    }
                }
                if let Some(id) = nesting {
                    distance.chain(graph, id);
                    lexical = Some(id);
                }
            }
        }
        // The extension edge, at the step of the class that installed it. Before the lexical walk,
        // like every ancestor chain: these are members, and `Object` must not claim them one step
        // from the cursor.
        if let Some(extended) = extended {
            distance.extended(extended);
        }
        // The view context's rows, at the step [`views::Reached`] carries (the chain Rails builds
        // `_helpers` from, not one of rubydex's). Before the lexical walk for the same reason as
        // the extension edge.
        for reached in view {
            if let Some(declaration) = graph.declarations().get(&reached.declaration) {
                distance.record(*declaration.owner_id(), reached.step as usize);
            }
        }
        // After every ancestor chain, never before one (see above).
        if let Some(id) = lexical {
            distance.lexical(graph, id);
        }
        distance
    }

    /// Number one linearized ancestor chain, outwards from the receiver.
    fn chain(&mut self, graph: &Graph, id: DeclarationId) {
        self.chain_from(graph, id, 0);
    }

    /// [`Self::chain`] numbered from `start`, answering the step after its last rung.
    fn chain_from(&mut self, graph: &Graph, id: DeclarationId, start: usize) -> usize {
        let Some((_, namespace)) = namespace(graph, id) else {
            return start;
        };
        let mut next = start;
        for ancestor in namespace.ancestors().iter() {
            // A rung rubydex could not linearize is still a rung: whatever lies past it is further
            // away, named or not.
            if let Ancestor::Complete(ancestor_id) = ancestor {
                self.record(*ancestor_id, next);
            }
            next += 1;
        }
        next
    }

    /// The one seed that is not a walk rubydex is about to make.
    ///
    /// A module a class object `extend`s but rubydex did not linearize is on **none** of those
    /// chains (why its members were missing), so every member would land on [`NO_DISTANCE`]: last,
    /// *behind `Object`'s own methods*, which is backwards for what a model body is most likely
    /// typing.
    ///
    /// One `record`, not a chain, because [`Extended::candidates`] offers a module's own members,
    /// never its ancestors'; numbering those could only make some *other* row look nearer.
    /// [`locator::Extension`] carries the step: the number of classes the receiver's chain passes,
    /// where Ruby's singleton chain puts the module.
    fn extended(&mut self, extended: &Extended) {
        for extends in &extended.modules {
            self.record(extends.module, extends.step);
        }
    }

    /// Number the lexical nesting outwards: `Billing::Invoice`, then `Billing`, then `Object`.
    ///
    /// rubydex's invariant is that only `Object` and `BasicObject` own themselves, so
    /// self-ownership ends the walk. The bound is there because a checked invariant is not an
    /// enforced one, and rubydex bounds its own owner walks the same way.
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

    /// The nearest ancestor chain counts.
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

    /// How far the namespace declaring this sits from the receiver.
    ///
    /// `owner_id` is rubydex's back-pointer to the namespace a member was collected from, so no
    /// name is parsed: `Foo::<Foo>#build()` and a top-level constant both answer without special
    /// cases.
    fn of(&self, declaration: &Declaration) -> u16 {
        self.steps
            .get(declaration.owner_id())
            .copied()
            .unwrap_or(NO_DISTANCE)
    }
}

/// How near a declaration sits to the cursor itself, counted in directories.
///
/// - **Why.** [`Distance`] measures nearness along a receiver's ancestor chain, and an untyped
///   receiver has no chain. Its list is every method name in the project, which on a Rails app
///   would otherwise open alphabetically (`account_type, add_row, amount, …`), telling the user
///   nothing.
/// - **What it asks instead:** not what the receiver is, but where the cursor is. It also breaks
///   ties in typed lists that `Distance` leaves (every top-level constant sits at the same chain
///   depth).
/// - **The path, not the namespace, by experiment.** A walk out through the lexical nesting works
///   on namespaced code, but a Rails model is `class Message < ApplicationRecord` at the top level,
///   with empty nesting, so half an app got nothing. Ruby projects put related code in one
///   directory whether or not they nest it, and Zeitwerk makes the directory *be* the namespace, so
///   the path carries everything the nesting did and works for flat code too.
struct Locality<'a> {
    /// Every document of the user's own code, and how far its directory is from the cursor's: 0 the
    /// file itself, 1 its directory or below, 2 the parent, and so on.
    ///
    /// - **Gems are absent, not far.** They are already below the user's code on `group`, and
    ///   scoring thousands of documents nobody asked about is wasted work.
    /// - **A generated document sits beside the file that implied it, at the same step**, flagged
    ///   `generated`. It has no path of its own (`synthesized::generated_uri` puts a scheme in
    ///   front of the source's URI), so the walk would score it as far as possible, and `group`
    ///   would read that as "not the user's code". A Rails app's most important declarations are
    ///   all generated, so without this a model's own columns would sit behind every method
    ///   anywhere on the chain.
    /// - **The third field is *the application never loads this document***:
    ///   [`environment::Names::in_a_test_tree`], [`environment::in_a_generator_template`] or
    ///   [`environment::Names::in_a_migration`]. It lives here because [`Locality::of`] is already
    ///   the one walk over a declaration's definitions, which runs per definition of every method
    ///   in the graph, so one lookup there is the budget. See [`Written::loadable`].
    /// - **The tags read different sets**, [`environment::Trees`]' rule: the test tree from `own`
    ///   (a gem's `lib/rack/test/` is a published library, so a miss must mean *not a spec*); the
    ///   template and migration from the whole graph (a tree `rails generate` copies from, or
    ///   `db:migrate` runs a file of, is the same wherever it ships; pundit's and engines' are
    ///   common).
    documents: HashMap<UriId, Where>,
    /// Which of the two fences is on: the first false where the cursor itself is in a test tree,
    /// the second false where the cursor itself is outside the project.
    ///
    /// Someone editing a spec is exactly who the next spec file's `def` answers, and someone
    /// editing a scratch file beside their project is who its top class answers.
    /// `environment::fenced_from` decides both, here and for the name rung. A field, not a
    /// per-candidate test, because it is about the *cursor*, which does not change between
    /// candidates, and the point of use runs once per method declaration in the graph.
    fenced: environment::Gates<'a>,
}

/// What [`Locality`] knows about one document, as the ranking and the two fences read it.
///
/// A struct, not a tuple: the fourth field made the tuple unreadable, and two fields can both be
/// true (a loose file whose path holds a `spec` segment is in both lists), so an entry holding
/// whichever loop wrote last would answer one fence wrongly.
#[derive(Clone, Copy)]
struct Where {
    /// How near the document sits to the cursor's own, or [`NO_LOCALITY`].
    step: u8,
    /// Whether ya-lsp generated it rather than the user having written it.
    generated: bool,
    /// Whether the application never loads it: a test tree, a generator template tree, or a
    /// migration.
    unloadable: bool,
    /// Whether it is outside the project altogether.
    outside: bool,
}

/// What a declaration in none of these documents is worth: nothing, equally, so the rest of the key
/// separates them.
const NO_LOCALITY: u8 = u8::MAX;

impl<'a> Locality<'a> {
    /// Built once per request, from the table [`Placed`] builds once per settle.
    ///
    /// - **Everything not about the cursor is already answered**: which documents are the user's
    ///   own, which trees the app never loads, which documents ya-lsp generated and from what, and
    ///   each own document's directory. All are properties of the settled graph, so they are
    ///   computed per settle, not per keystroke (see [`Placed`]).
    /// - **What is left is the one term a cursor moves**: how many leading segments each directory
    ///   shares with the cursor's. The cursor's directory is split **once**, here, which is why
    ///   [`shared_segments`] takes a slice and a `&str`.
    fn at(graph: &'a Graph, here: UriId, placed: &Placed, layout: environment::Layout<'_>) -> Self {
        // The sum over-counts documents in two of the four lists: a slightly large reservation
        // instead of a rehash.
        let mut documents = HashMap::with_capacity(
            placed.unloadable().len()
                + placed.each_own().len()
                + placed.generated().len()
                + placed.outside().len(),
        );
        let here_uri = graph.documents().get(&here).map(|document| document.uri());
        // The same gate `locator::loadable_from` asks, so the two dropping surfaces cannot disagree
        // about which cursors are fenced. A cursor in a document the graph never held scores and
        // fences nothing: both halves fail towards offering more.
        let fenced = environment::fenced_from(here_uri, layout);
        let Some(cursor) = here_uri else {
            return Self { documents, fenced };
        };
        let cursor: Vec<&str> = directory_of(cursor).split('/').collect();

        // **The two tags read from the whole graph, not `own`.** A gem's generator template is
        // unloadable for the same reason the project's is (nobody requires the tree
        // `rails generate` copies from); pundit's is the common case. An engine's `db/migrate/` is
        // the same: run one file at a time by a rake task, wherever it ships from. The loop below
        // then sets the step: a project template is overwritten there with its real distance and
        // the same flag, since `insert` is last-writer-wins and `own` comes second.
        //
        // An entry at `NO_LOCALITY` changes no ranking: `of` takes a `min` over the entries it
        // finds and falls back to exactly this value when it finds none.
        for id in placed.unloadable() {
            documents.insert(
                *id,
                Where {
                    step: NO_LOCALITY,
                    generated: false,
                    unloadable: true,
                    // Read here, not in its own loop, because a loose file with a `spec` segment in
                    // its path is in both lists, and one entry must carry both answers.
                    outside: placed.outside().contains(id),
                },
            );
        }

        // **Every document outside the project: the one list here not about ranking.** A file open
        // beside the user's work is not near or far: it is not the project's, so its row is dropped
        // rather than placed. `or_insert`, because the loop above may already have written it.
        for id in placed.outside() {
            documents.entry(*id).or_insert(Where {
                step: NO_LOCALITY,
                generated: false,
                unloadable: false,
                outside: true,
            });
        }

        for own in placed.each_own() {
            let step = if own.id == here {
                0
            } else {
                let shared = shared_segments(&cursor, &own.directory);
                u8::try_from(cursor.len() - shared + 1).unwrap_or(NO_LOCALITY)
            };
            // The test flag is the source document's and is carried onto its generated entry below:
            // what `workspace/rails/` writes for a file under `spec/` is reachable exactly where
            // that file is.
            documents.insert(
                own.id,
                Where {
                    step,
                    generated: false,
                    unloadable: own.unloadable,
                    // The user's own code and documents outside the project are disjoint by
                    // construction: `is_own` needs the workspace root or an `[index] load_paths`
                    // prefix, and outside means neither.
                    outside: false,
                },
            );
        }

        // Every document the generator pass wrote, at its source's step and with its source's test
        // flag. A generated document whose source is in none of the entries above (a gem's) is left
        // out. One source writes one document per body, so the set is read from the graph and
        // mapped back by naming, not guessed from a URI.
        for (id, source) in placed.generated() {
            if let Some(&source) = documents.get(source) {
                documents.insert(
                    *id,
                    Where {
                        generated: true,
                        ..source
                    },
                );
            }
        }
        // **The cursor's own document is not outside, for this request only.**
        // `environment::Outward::Alone`'s exception, written into the table instead of asked per
        // candidate (`of` reads one entry per definition of every method in the graph). Every
        // *other* outside document keeps its flag, so one unsaved buffer never reads another.
        if let Some(entry) = fenced
            .outward
            .own_document()
            .and_then(|cursor| documents.get_mut(&UriId::from(cursor)))
        {
            entry.outside = false;
        }
        Self { documents, fenced }
    }

    /// The nearest document the declaration was written in, whether ya-lsp wrote it, and whether
    /// the application loads it.
    ///
    /// - **The nearest**, because a class reopened in two places (a concern, a monkey patch) should
    ///   be scored by the copy the cursor can see, not whichever rubydex recorded first.
    /// - **One walk for all three**, because this runs over every method in the graph on the
    ///   name-based path: the second is a `bool` in the entry the first reads, the third two
    ///   counters over the same entries.
    /// - **A tie between a real and a generated file goes to the real one**: `(step, false)` sorts
    ///   before `(step, true)`. The test flag is **not** in that comparison: it decides whether the
    ///   row exists, not where it sits.
    fn of(&self, graph: &Graph, declaration: &Declaration) -> Written {
        let mut nearest: Option<(u8, bool)> = None;
        let mut tally = Tally::default();
        let mut elsewhere = Tally::default();
        for definition in declaration
            .definitions()
            .iter()
            .filter_map(|id| graph.definitions().get(id))
        {
            // A definition in none of the user's documents is a gem's, Ruby's signatures', or one
            // never scored: far, and loadable. Only `own` knows where *this project's* test trees
            // are, so a miss is not evidence of a spec (a gem with `test/` inside `lib/` is not
            // this rule's business).
            let entry = self.documents.get(definition.uri_id());
            if let Some(&Where {
                step, generated, ..
            }) = entry
            {
                nearest = Some(nearest.map_or((step, generated), |held| {
                    std::cmp::min(held, (step, generated))
                }));
            }
            // Outside the `if`: a definition in none of the user's documents still counts, and
            // counting them is what makes the "any" rule an "any". See [`Tally::loadable`], which
            // also decides a declaration with no definitions.
            tally.saw(entry.is_some_and(|entry| entry.unloadable));
            elsewhere.saw(entry.is_some_and(|entry| entry.outside));
        }
        let (step, generated) = nearest.unwrap_or((NO_LOCALITY, false));
        Written {
            step,
            generated,
            loadable: !self.fenced.trees || tally.loadable(),
            inside: !self.fenced.outward.on() || elsewhere.loadable(),
        }
    }
}

/// Where a declaration was written, as the ranking and the fence both need it.
///
/// Three answers from [`Locality::of`]'s single walk: two order the list, the third decides whether
/// the row is on it.
struct Written {
    /// How near the nearest of the user's documents holding it is to the cursor.
    step: u8,
    /// Whether that nearest document was generated by ya-lsp rather than written by the user.
    generated: bool,
    /// Whether **any** definition is in code the application loads.
    ///
    /// *Any*, and a declaration with no definitions counts as loadable: both are decided only in
    /// [`Tally::loadable`]. This removes a declaration whose *every* definition is in a test tree:
    /// a name that cannot be called from the cursor, the same reason `reachable` refuses a private
    /// method.
    ///
    /// **True for everything when the cursor is in a test tree**, so the term is off exactly where
    /// it would be wrong. See [`Locality::fenced`].
    loadable: bool,
    /// Whether **any** definition is inside the project at all.
    ///
    /// The second fence, off for a cursor that is itself outside, so a reader in a scratch file
    /// beside the project is offered that file's own names and nothing else outside.
    inside: bool,
}

/// A URI without its last segment. Both sides come from `Url::from_file_path`, so they are
/// canonical, and comparing text compares paths.
///
/// `pub(super)` for `indexed::Placed`, which reads this for every user document when the graph
/// settles, making it cheap. The rule sits here, beside the cursor half it is compared with.
pub(super) fn directory_of(uri: &str) -> &str {
    uri.rsplit_once('/').map_or(uri, |(directory, _)| directory)
}

/// How many leading path segments two directories share.
///
/// Segment-wise, not byte-wise, or `app/model` would share all of `app/models`.
///
/// **The argument shapes are the performance choice.** This runs once per own document with the
/// same cursor each time, so the cursor arrives pre-split; splitting it here would repeat thousands
/// of times per keystroke. The directory is a `&str` because splitting *it* is the work, and
/// `Placed` holding the directory makes it the only path work completion does.
fn shared_segments(cursor: &[&str], directory: &str) -> usize {
    cursor
        .iter()
        .zip(directory.split('/'))
        .take_while(|(cursor, segment)| **cursor == *segment)
        .count()
}

/// The namespace a receiver id names, following a constant alias as rubydex's own walk does:
/// `Money = Billing::Money` offers what it points at.
fn namespace_id(graph: &Graph, id: DeclarationId) -> Option<DeclarationId> {
    namespace(graph, id).map(|(id, _)| id)
}

/// The same, plus the declaration it had to look up anyway.
///
/// `chain` calls this once per ancestor of every candidate's receiver, so looking the id up again
/// would be a second hash lookup on that path, re-checking a `Declaration` already proven to be a
/// `Namespace`.
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
/// - **Deduplicated by name**, because `name` is defined by hundreds of classes in a Rails bundle,
///   and hundreds of identical rows is not a list. The user's own code wins the duplicate, so the
///   detail line names a file they can read.
/// - **Deduplicated before sorting, on a hash of the label**: this runs over every method in the
///   graph per keystroke, and copying each name would be a hundred thousand allocations to discard.
/// - **Over `admitted` candidates it returns no rows.** A decision, not an absence: the list
///   returns once typing narrows the candidates below the line. At an empty prefix it never does,
///   since the candidates are the whole project's names and the typed word ranks in the thousands.
/// - **Under it, rows are cut to `shown`**, which is honest here but not at an empty prefix: with
///   nothing typed, `tier` and `length` tie and only `Locality` (a name's directory) ranks, so
///   truncation would keep arbitrary rows. From three characters both are live, and in every
///   measured list of up to 512 candidates the typed word sits within the first 128 rows.
///
/// `isIncomplete` is true in both cases, which makes the refusal self-correcting (see the module
/// docs): calling the empty answer *complete* would make the client filter locally and never ask
/// again.
fn by_name(graph: &Graph, shown: usize, admitted: usize, ranking: &Ranking) -> (Vec<Item>, bool) {
    // Every method name contains a `#` and no other declaration's does, so rubydex's parallel
    // filter does the "methods only" pass for free.
    let query = format!("#{}", ranking.prefix);
    let mut best: HashMap<u64, Ranked> = HashMap::new();

    for id in query::declaration_search(graph, &[&query], &MatchMode::Fuzzy) {
        // The query starts with `#`, which only method names contain, so rubydex's parallel filter
        // has already kept only methods. Bound here, not re-tested: a second `matches!` over every
        // method in the graph proves nothing.
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

        // **Counted inside the walk, because the walk is what the ceiling stops.** Counted after
        // the dedup, because a duplicate is not a candidate anyone could pick. `best` only grows,
        // so once over the line it cannot come back, and ranking the rest would build a list that
        // is discarded whole.
        //
        // This is the common case. The query is `#` plus what was typed, and at a bare `.` nothing
        // is typed: a lone `#` fuzzy-matches every method name in the graph (tens of thousands),
        // and most cursors are still over the line at the fourth character.
        if best.len() > admitted {
            return (Vec::new(), true);
        }
    }

    let mut ranked: Vec<Ranked> = best.into_values().collect();
    let truncated = take_best(&mut ranked, shown);
    (spelled_out(graph, ranked), truncated)
}

/// Everything the ranking needs besides the candidate.
///
/// A struct, because `ranked_declaration` is the one point both the receiver path and the
/// name-based path go through: every new ranking term lands here, so the two paths cannot drift
/// apart.
struct Ranking<'a> {
    prefix: &'a str,
    distance: Distance,
    locality: Locality<'a>,
    /// Whether Ruby allows a private method at the cursor, decided from syntax alone by
    /// [`cursor::Context::allows_private`].
    private_ok: bool,
    /// The second opinion on rubydex's visibility record, for the one shape it gets wrong.
    ///
    /// Here for `locator`'s reason: a bare `private` inside a block is recorded against every `def`
    /// after the block, so an ordinary Rails concern's public methods would never be offered. See
    /// [`locator::Modifiers`].
    ///
    /// **`None` on the name-based path, as a bound.** The repair reads the declaring document
    /// (memoised per document), and the receiver path's candidates come from one ancestry, tens of
    /// documents. [`by_name`] walks every method in the graph before deciding whether to answer, so
    /// the repair there would read thousands of files per keystroke. Nothing is lost at any cursor:
    /// the name-based path runs only where no receiver resolved, and produces the *Guessed* tier,
    /// where a list one name short is exactly what the tier warns about. The repair exists to stop
    /// a **Resolved** card being contradicted by its own list.
    modifiers: Option<&'a locator::Modifiers<'a>>,
}

/// The five method names Ruby keeps private however they were declared.
///
/// - **`rb_add_method` makes them private by name**, so a `def initialize` is private whatever its
///   class said, and neither rbs nor rubydex records that. Checked against Ruby 4.0.1: these five,
///   nothing else. `method_missing` and `singleton_method_added` look like they belong but do not;
///   they are private on `BasicObject` because those copies were written that way, which the graph
///   already knows.
/// - **Instance methods only.** `def self.initialize` stays public, and `Bar.initialize` really
///   calls it, so [`reachable`] checks the owner first. `Class#initialize` is still caught, as an
///   instance method of `Class`.
const ALWAYS_PRIVATE: [&str; 5] = [
    "initialize",
    "initialize_clone",
    "initialize_copy",
    "initialize_dup",
    "respond_to_missing?",
];

/// Whether Ruby allows this method to be called where the cursor is.
///
/// Everything here already passed rubydex's visibility filter, which answers a slightly different
/// question. Two gaps, closed together:
///
/// - **rubydex passes a private method whenever the caller's `self` is the receiver's class.** Ruby
///   exempts only a receiver *written* `self`, so `Vault.new.secret` would be offered inside
///   `Vault`, where Ruby raises `NoMethodError`.
/// - **The graph trusts rbs about the five names above**, and rbs is inconsistent:
///   `Kernel#initialize_copy` is private but `String#initialize_copy` public, so `"hi".` would
///   offer both `initialize` and `initialize_copy` while `count.` offers only the first.
///
/// Both ask whether this may be written *here*, and `private_ok` answers.
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
        // rubydex treats `module_function`'s instance copy as private, as Ruby does.
        Some(Visibility::Private | Visibility::ModuleFunction)
    ) && modifiers.is_none_or(|modifiers| modifiers.confirm(graph, id))
    {
        return false;
    }
    !ALWAYS_PRIVATE.contains(&label) || singleton_owned(graph, declaration)
}

/// Whether a method hangs off a singleton class: rubydex's spelling of `def self.foo`.
fn singleton_owned(graph: &Graph, declaration: &Declaration) -> bool {
    matches!(
        graph.declarations().get(declaration.owner_id()),
        Some(Declaration::Namespace(Namespace::SingletonClass(_)))
    )
}

struct Ranked {
    group: u8,
    tier: u8,
    /// Whether the name is spelled as an internal one: `_fork`, `__send`.
    internal: bool,
    /// How far the namespace declaring this sits from the receiver. See [`Distance`].
    distance: u16,
    /// Whether ya-lsp wrote the document this was declared in.
    ///
    /// - **Its own term, below `distance` and above `locality`.** "What the class was written to
    ///   do, then what its table holds" is a property of the declaration's **kind**, not its
    ///   location. A column is declared in `db/schema.rb` and the method beside it in
    ///   `app/models/story.rb`; leaving them to `locality` would order them by which directory the
    ///   cursor is nearer, inverting between `app/` and `db/`. The test named for it fails without
    ///   this.
    /// - **The corpora mildly disagree**: developers reach a table's members slightly more often
    ///   than the `def`s beside them. It is kept because the alternative is an order that changes
    ///   with the cursor's directory.
    generated: bool,
    /// How near the declaration itself sits to the cursor. See [`Locality`].
    locality: u8,
    /// Where a keyword argument sits in its declaring signature; zero for everything else.
    ///
    /// Only a signature's own order. rubydex's emission order looks usable (the ancestor chain is
    /// walked outwards), but *within* one namespace it is a hash map's order and reshuffles when a
    /// member is added, and a list that rearranges as the file is edited is worse than
    /// alphabetical. [`Distance`] takes the stable half of emission order. A signature is a list,
    /// so its order is real.
    sequence: usize,
    /// The label's length, or zero when nothing is typed yet.
    ///
    /// Shortest-first is a good tiebreak among rows that all matched a prefix, and a poor one among
    /// rows that matched nothing (`Foo::` should read alphabetically). The zero lets the term rank
    /// high at no cost: it is silent exactly where it would be wrong, so the terms below keep their
    /// order. See [`order`].
    length: usize,
    /// The declaration whose qualified name becomes [`Item::detail`], **spelled after the cap, not
    /// before**.
    ///
    /// The detail line is in no term of [`order`] and is read only by the response builder.
    /// Spelling it when the row is built would spell it for every candidate the prefix admits
    /// (every method in the graph, at an empty prefix): two `format!`s and a name walk each, for
    /// strings all but `limit` of them discard. Measured, that was a large share of the analysis
    /// thread's CPU during completion.
    ///
    /// `None` wherever the detail is a constant the producer already knows: every non-declaration
    /// row.
    detail_of: Option<DeclarationId>,
    item: Item,
}

/// What the user typed first, then how near the name is, then the shorter one.
///
/// 1. **Match quality leads.** With `group` (is this the user's code) first, a project name
///    matching `wh` as a *subsequence* would beat `where` matching it as a *prefix*. Measured,
///    putting `tier` first nearly eliminates words ranked past 128 one character in. Everything
///    below is a tiebreak among rows the user asked for equally.
/// 2. **Names starting with `_` sink**: Ruby's "not meant for you" spelling. Without this, `User.`
///    opens on `__send`, `_fork` and `_load_from_sql` (underscores sort before letters). Typing `_`
///    lifts them back.
/// 3. **`group` has three bands.** Keyword arguments lead, because they are the only suggestions
///    that are *wrong* to leave out (in `build(` the parameter names are the answer). Then every
///    declaration, the user's and gems' **together**: `distance` knows a method on the receiver
///    beats one on `Object` whoever wrote either. Ruby's keywords come last: above declarations,
///    forty keywords would fill the top ten at a bare cursor. The cost is `end` ranking a few
///    places lower mid-word, returned once `end` is typed in full (an exact match outranks every
///    prefix).
/// 4. **[`Distance`] ranks a list with nothing typed**, where `tier` ties: the nearer owner wins,
///    and `Object` (end of every chain, drain for everything rubydex could not attribute) is as far
///    as possible.
/// 5. **`length` sits above both nearness terms.** It cannot speak at a bare cursor (`sort_length`
///    is zero there) and is the strongest tiebreak once a character arrives: among rows matching
///    `ren`, `render` is the answer, not `rendered_format`. Measured, this gets the right word to
///    rank 1 noticeably more often at one to three characters, and changes nothing at a bare
///    cursor, which is why the move is free.
///
/// [`Locality`] asks [`Distance`]'s question about a different thing. Distance is stronger and goes
/// first, but is silent twice: with no receiver, and among one chain's tied rows. Locality speaks
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

/// Keep the `limit` best rows in order, and say whether any were dropped.
///
/// The return value is all of `Completion::incomplete`: dropping a row is the only way this list
/// can miss something a longer prefix would reach. See the module docs.
fn take_best(ranked: &mut Vec<Ranked>, limit: usize) -> bool {
    let truncated = ranked.len() > limit;
    if truncated {
        ranked.select_nth_unstable_by(limit, order);
        ranked.truncate(limit);
    }
    ranked.sort_unstable_by(order);
    truncated
}

/// The rows that survived the cap, each given the detail line only it now needs.
///
/// The one place a [`Ranked`] becomes an [`Item`], so the deferral [`Ranked::detail_of`] describes
/// cannot be half-done. `graph.declarations()` is asked again rather than carrying the name,
/// because a borrow would thread a lifetime through `rank`, `order` and `take_best` to save one
/// lookup per *kept* row (at most `limit`), against the tens of thousands of rows no longer
/// spelled.
fn spelled_out(graph: &Graph, ranked: Vec<Ranked>) -> Vec<Item> {
    ranked
        .into_iter()
        .map(|entry| {
            let mut item = entry.item;
            if let Some(declaration) = entry.detail_of.and_then(|id| graph.declarations().get(&id))
            {
                item.detail = Some(render::qualified_name(graph, declaration.name()));
            }
            item
        })
        .collect()
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
                // comparisons where that would matter: keyword arguments lead the list, and Ruby's
                // keywords are their own band.
                distance: 0,
                generated: false,
                locality: 0,
                sequence,
                length: sort_length(prefix, &label),
                detail_of: None,
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
            detail_of: None,
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
    // A `Todo` namespace is a placeholder the resolver invented for a parent it never saw: no
    // definition to jump to, no members to offer.
    if matches!(declaration, Declaration::Namespace(Namespace::Todo(_))) {
        return None;
    }
    let name = declaration.name();
    // A singleton class and an anonymous `Class.new` both have invented names. The singleton's
    // *methods* are still offered, spelled as its class calls them.
    //
    // Take the segment first and test it, because `is_nameable` reads the same segment; asking the
    // name would walk it twice, over every method declaration in the graph.
    let label = render::last_segment(name);
    if !render::segment_is_nameable(label) {
        return None;
    }
    // After `tier`, on purpose. This runs per candidate (up to a hundred thousand), and `reachable`
    // costs a visibility lookup while the prefix test is a string compare, so the cheap filter goes
    // first. `private_ok` short-circuits wherever no receiver was written.
    let tier = tier(prefix, label)?;
    if !ranking.private_ok && !reachable(graph, ranking.modifiers, id, declaration, label) {
        return None;
    }

    // One walk over the definitions, not three. `Locality` holds exactly the user's own documents,
    // so "is this theirs" is "did any document score", and this runs over every method in the graph
    // on the name-based path.
    let written = ranking.locality.of(graph, declaration);
    // **A row the application cannot load is dropped, not ranked lower.** Ruby will not find the
    // name from the cursor (only RSpec loads its file), so offering it offers a suggestion that
    // cannot run, the same judgement `reachable` makes for a private method. Sinking it would still
    // take a slot under `MAX_COMPLETION_ITEMS` and count in `by_name`'s candidate total, which
    // decides whether a guess is offered at all. See [`Written::loadable`] for why the cursor's own
    // tree turns this off.
    if !written.loadable {
        return None;
    }
    // **The same drop for the other meaning.** A name whose every definition is outside the project
    // is not this project's: its file is one the user opened beside their work, which the
    // application cannot load at all. See [`Written::inside`] for why a scratch file's own cursor
    // turns this off.
    if !written.inside {
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
        detail_of: Some(id),
        item: Item {
            label: label.to_owned(),
            detail: None,
            kind: kind_of(declaration),
            documentation: None,
            deprecated: false,
            declaration: Some(id),
        },
    })
}

/// A name nobody reaches by typing its first character: `_internal`, `!`, `<=>`, `[]`, `$0`.
///
/// Punctuation and underscores sort before letters, so without this a large project offers `!`,
/// `%`, `&` and `__send` first after a `.`. A sigil is not punctuation (`@name` is a name), so it
/// is skipped before the test.
///
/// Typing one of these characters lifts them all back: someone who writes `_` means it.
fn is_internal(prefix: &str, label: &str) -> bool {
    let Some(first) = significant(label) else {
        return true;
    };
    if first.is_alphabetic() {
        return false;
    }
    significant(prefix).is_none_or(char::is_alphabetic)
}

/// The leading run of sigil characters: `@`, `@@`, `$`, or nothing.
///
/// A run, not a first character, because of `$@`: every character in it is a sigil character, so
/// the whole name comes back, which is right for the only question asked of it (only a `$` prefix
/// should reach it).
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

/// How well `prefix` matches `label`, or `None` if it does not match.
///
/// The floor is a case-insensitive subsequence, what editors fuzzy-match with: stricter would drop
/// rows the client would happily show.
fn tier(prefix: &str, label: &str) -> Option<u8> {
    // Except the sigil, which is not a letter to fuzzy-match. `@foo`, `@@foo` and `$foo` are three
    // Ruby namespaces, and a name in one is not a candidate for a prefix in another, so the label
    // must carry the prefix's sigil.
    //
    // - **`starts_with`, not equality**, because `@` is on the way to `@@`: someone who typed one
    //   `@` may type the second, and dropping class variables there would be the opposite mistake.
    //   It does not run backwards: a `@@` prefix admits no `@name`.
    // - **Why it exists:** without it `@` offers `$@`, since the subsequence match reads the sigil
    //   as a character and `$@` contains an `@`.
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
/// Read from the *declaration*, not a definition. The outline can tell an `attr_reader` from a
/// `def` because it looks at one definition; here there are hundreds of rows per keystroke and
/// rubydex files both as methods anyway. A definition lookup per row to change one icon is not
/// worth it.
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

/// Case-insensitive `starts_with` without allocating a lowercase copy: this runs per candidate, up
/// to a hundred thousand.
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

    #[test]
    fn a_cursor_in_a_document_the_graph_has_never_held_fences_nothing() {
        // **A file written since the last settle**: the one cursor `Locality` can be built for but
        // not scored from. No document exists under this URI, so there is no directory to measure
        // from and no path to read a fence from. Both halves fail towards offering *more*: a fence
        // needs evidence, and a missing document is none. So the project's own names still
        // complete, unranked by locality and unfenced.
        let mut harness = Harness::new();
        harness.write(
            "app/models/store.rb",
            "class Store\n  def restock\n  end\nend\n",
        );
        harness.index();

        let marked = "class Late\n  def run\n    Store.new.resto~\n  end\nend\n";
        let position = marked_position(marked);
        // Written, never opened: `with_text` reads it from disk, and the graph holds nothing under
        // its URI until the next settle.
        let late = harness.write("app/models/late.rb", &marked.replace('~', ""));

        let answer = harness.ask(
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": late.as_str() },
                "position": position,
            }),
        );
        let labels: Vec<&str> = answer["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["label"].as_str())
                    .collect()
            })
            .unwrap_or_default();
        assert!(labels.contains(&"restock"), "{labels:?}");
    }

    /// An id, URI or name no graph holds: what a client sends after a config reload dropped the
    /// graph its list was built from.
    #[test]
    fn nothing_in_an_empty_graph_is_near_the_cursor_or_encloses_it() {
        // Each of these takes an id from outside (the request's URI, the `data` a client echoes on
        // `completionItem/resolve`), and rubydex ids are hashes, so "the graph does not hold this"
        // is a state the server is handed. The ranking must degrade to "nothing", not to a panic or
        // a wrong answer.
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
            &Placed::of(&graph, |_| false, environment::Layout::default()),
            environment::Layout::default(),
        );
        assert!(
            locality.documents.is_empty(),
            "no cursor document, so nothing to measure nearness against"
        );

        // `Object` is in every real graph (rubydex indexes a built-in one), so this is the only way
        // to reach a nesting that resolves to no declaration.
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
        // A sigil is not punctuation: `@name` is a name.
        for label in ["@name", "$stdout", "shout", "Person"] {
            assert!(!is_internal("", label), "{label} should not sink");
        }
        // Typing one of these characters lifts them all back: someone who writes `_` means it.
        assert!(!is_internal("_", "_internal"));
        assert!(!is_internal("<", "<=>"));
        assert!(
            !is_internal("@_", "@_hidden"),
            "the sigil is stepped over first"
        );
        // A label with nothing after its sigils has no first character to judge.
        assert!(is_internal("", ""));
        assert!(is_internal("", "@"));
    }

    #[test]
    fn a_prefix_matches_as_a_subsequence_case_insensitively() {
        // What a client filters on between keystrokes.
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
        // Ruby allows them. ASCII on either side takes the fast path; two non-ASCII characters need
        // full lowercase mapping, the only route to `eq_ci`'s last line.
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
        // The tiers reaching a completion row. A row's card is the card a hover draws, so a
        // `completionItem/resolve` that always built it with `precise: true` would present every
        // name-based row (every project method matched by name) as certain.
        //
        // The tier belongs to the *list*, since every row was offered for the same receiver. It
        // travels on `data`, because when the client asks to resolve a row, that is all either side
        // still knows about the list's origin.
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
            !derived.contains("Guessed from name alone"),
            "and must not be presented as a guess: {derived}"
        );

        let guessed = resolve(&mut harness, &guessed);
        assert!(
            guessed.contains("Guessed from name alone"),
            "a name-matched row has to say so: {guessed}"
        );
    }

    #[test]
    fn a_local_completes_where_the_member_after_the_dot_is_already_written() {
        // The receiver's variables are read in the repaired text, and blanking a written member's
        // `.` there left `name upcase` or `name scan("a")`: a call to a method `name`, no local,
        // and the name-based list (or none) at the commonest edit of a method name.
        let (mut harness, uri) = with_types("");
        for (marked, member) in [
            ("name = \"hi\"\nname.~upcase\n", "upcase"),
            ("name = \"hi\"\nname.up~case\n", "upcase"),
            ("name = \"hi\"\nname.scan~(\"a\")\n", "scan"),
            ("items = [1]\nitems.first~ do |i|\n  i\nend\n", "first"),
            ("items = [1]\nitems.first~ { |i| i }\n", "first"),
        ] {
            let (labels, precise) = offered(&harness.complete(&uri, marked));
            assert!(
                precise && labels.iter().any(|label| label == member),
                "{marked:?} offered {labels:?}, precise: {precise}"
            );
        }
    }

    #[test]
    fn a_union_offers_every_classs_members_each_marked_with_its_class() {
        // A call on a union runs on each class that has the member (`types::narrowed`), so the
        // list is every class's, and the detail says which. Before, a union offered nothing: the
        // commonest receiver in a controller, `@invite = Invite.find(params[:id])`, is one.
        let (mut harness, uri) = with_types("");
        let answer = harness.complete(
            &uri,
            "class Cat\n  def speak = \"meow\"\n  def purr = 1\nend\n\
             class Dog\n  def speak = \"woof\"\nend\n\
             pet = rand ? Cat.new : Dog.new\npet.~\n",
        );
        let (labels, precise) = offered(&answer);
        assert!(precise, "a union is typed, not the name list: {labels:?}");
        let detail = |label: &str| {
            let rows: Vec<&str> = answer["items"]
                .as_array()
                .expect("a list")
                .iter()
                .filter(|item| item["label"] == label)
                .map(|item| item["detail"].as_str().unwrap_or("(none)"))
                .collect();
            rows.join(" / ")
        };
        assert_eq!(detail("purr"), "Cat#purr", "one class's member names it");
        // Two classes declaring the name apart: one row, both named.
        assert_eq!(detail("speak"), "Cat#speak | Dog#speak");
        // Every object's member: one row, for one declaration.
        assert_eq!(detail("tap"), "Kernel#tap");
        // Each class's own members rank before what every object has, at the farthest chain's
        // distance, not the nearest.
        let at = |label: &str| labels.iter().position(|row| row == label).expect(label);
        assert!(
            at("purr") < at("tap") && at("speak") < at("tap"),
            "{labels:?}"
        );
        // The case that needs it: `Kernel` is two steps from a `String` and five from `Deep`, and
        // `Deep`'s inherited `ancient`, three steps out, still leads it.
        let (labels, _) = offered(&harness.complete(
            &uri,
            "class Root\n  def ancient = 1\nend\nclass Mid < Root\nend\nclass Low < Mid\nend\n\
             class Deep < Low\nend\n\
             thing = rand ? Deep.new : \"x\"\nthing.~\n",
        ));
        let at = |label: &str| labels.iter().position(|row| row == label).expect(label);
        assert!(at("ancient") < at("tap"), "{labels:?}");

        // Literals too, and `nil` stays folded out, as for a `T?`.
        let (labels, precise) =
            offered(&harness.complete(&uri, "value = rand ? \"a\" : (rand ? 1 : nil)\nvalue.~\n"));
        assert!(precise, "{labels:?}");
        for member in ["upcase", "succ", "tap"] {
            assert!(
                labels.iter().any(|label| label == member),
                "{member}: {labels:?}"
            );
        }
        assert!(!labels.iter().any(|label| label == "nil?"), "{labels:?}");
    }

    #[test]
    fn a_modules_instance_lists_objects_members_after_its_own() {
        // `self` in a module's `def` is some class's instance, and every class descends from
        // `Object` (`types::member_of`): hover resolves `self.class` to `Kernel#class`, so the list
        // offers `Object`'s members too, after the module's own, and a name the module defines
        // stays the module's.
        let (mut harness, uri) = with_types("");
        let (labels, precise) = offered(&harness.complete(
            &uri,
            "module Cached\n  def key = 1\n  def run\n    self.~\n  end\nend\n",
        ));
        assert!(precise, "{labels:?}");
        let at = |label: &str| labels.iter().position(|row| row == label).expect(label);
        assert!(at("key") < at("tap") && at("run") < at("tap"), "{labels:?}");
        let answer = harness.complete(
            &uri,
            "module Cached\n  def tap = 2\n  def run\n    self.~\n  end\nend\n",
        );
        let details: Vec<&str> = answer["items"]
            .as_array()
            .expect("a list")
            .iter()
            .filter(|item| item["label"] == "tap")
            .map(|item| item["detail"].as_str().unwrap_or("(none)"))
            .collect();
        assert_eq!(details, ["Cached#tap"]);
    }

    #[test]
    fn a_bare_word_in_a_rebound_block_lists_what_the_block_runs_against() {
        // Hover asks a bare word on the `self` a signature gives the block first
        // (`locator::rebound_call`); the list offers the same members, beside the lexical ones.
        // RSpec's example groups and a concern's `included do` reach here the same way.
        let mut harness = signed(
            &[
                ("core/core.rbs", TYPED_RBS),
                (
                    "core/app.rbs",
                    "class Settings\n  def name: () -> String\nend\n\n\
                     class App\n  def configure: () { () [self: Settings] -> void } -> void\nend\n",
                ),
            ],
            "",
        );
        let uri = harness.write("lib/main.rb", "");
        harness.index();
        harness.index_gems();
        let answer = harness.complete(&uri, "App.new.configure do\n  na~\nend\n");
        let (labels, precise) = offered(&answer);
        assert!(
            precise && labels.iter().any(|label| label == "name"),
            "{labels:?}"
        );
        let detail = answer["items"]
            .as_array()
            .expect("a list")
            .iter()
            .find(|item| item["label"] == "name")
            .and_then(|item| item["detail"].as_str())
            .map(str::to_owned);
        assert_eq!(detail.as_deref(), Some("Settings#name"));
        // Outside the block the lexical `self` alone answers.
        let (labels, _) = offered(&harness.complete(&uri, "App.new.configure do\nend\nna~\n"));
        assert!(!labels.iter().any(|label| label == "name"), "{labels:?}");
    }

    #[test]
    fn a_bare_word_in_a_modules_def_lists_objects_members_too() {
        // The `.`-less half of the rule above: `format` in a helper module's `def` is
        // `Kernel#format` on hover, so the list offers it, private as a bare word may call it.
        let mut harness = signed(
            &[
                ("core/core.rbs", TYPED_RBS),
                (
                    "core/private.rbs",
                    "module Kernel\n  private\n\n  def shout: () -> Integer\nend\n",
                ),
            ],
            "",
        );
        let uri = harness.write("lib/main.rb", "");
        harness.index();
        harness.index_gems();
        let fixture = "module Report\n  def row\n    ~\n  end\nend\n";
        for (typed, member) in [("sh", "shout"), ("ta", "tap")] {
            let (labels, precise) =
                offered(&harness.complete(&uri, &fixture.replace('~', &format!("{typed}~"))));
            assert!(
                precise && labels.iter().any(|label| label == member),
                "{typed}: {labels:?}"
            );
        }
    }

    #[test]
    fn a_union_wider_than_the_cap_is_declined_as_every_union_was() {
        // `MAX_UNION_CLASSES` bounds the walks one keystroke makes: at the cap each class is
        // listed, one past it the receiver is not typed and the name rung answers, as for every
        // union before.
        let (mut harness, uri) = with_types("");
        let wide = |count: usize| {
            let classes: String = (0..count)
                .map(|at| format!("class C{at}\n  def own{at} = 1\nend\n"))
                .collect();
            let returns: String = (0..count)
                .map(|at| format!("  return C{at}.new if rand\n"))
                .collect();
            format!("{classes}def pick\n{returns}  nil\nend\npick.~\n")
        };
        let (labels, precise) = offered(&harness.complete(&uri, &wide(MAX_UNION_CLASSES)));
        assert!(
            precise && labels.iter().any(|label| label == "own0"),
            "{labels:?}"
        );
        let (labels, precise) = offered(&harness.complete(&uri, &wide(MAX_UNION_CLASSES + 1)));
        assert!(!precise, "{labels:?}");
    }

    #[test]
    fn a_constructors_keyword_arguments_complete_at_the_call() {
        // The redirect goes through `locator::resolve`, which is also where `Context::Argument`
        // gets the method whose parameters it offers. Without it, `Foo.new(` would complete against
        // `Class#new`'s `(*untyped, **untyped)`, which has no keywords.
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
        // `MAX::` is legal to type and means nothing: an `Integer` has no members to write there.
        // rubydex still resolves the receiver to a declaration, so every later step (the ancestor
        // walk, the member list) must answer "not a namespace" instead of assuming the id names
        // one.
        let mut harness = Harness::new();
        let uri = harness.write("app/main.rb", "MAX = 10\n");
        harness.index();

        let offered = harness.suggestions(&uri, "MAX = 10\nMAX::~\n");
        assert!(offered.is_empty(), "{offered:?}");
    }

    #[test]
    fn a_singleton_method_written_on_a_constant_is_scoped_to_that_class() {
        // `def Person.build` is the same method as `def self.build`, written from outside, and
        // rubydex records the receiver differently for each. Inside it `self` is `Person`, which
        // decides whether the class's own singleton methods are callable without a receiver.
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
        // `::` after a non-namespace is legal to type and means nothing, and each shape reaches a
        // different arm. Left unanswered, the fall-through would not give silence but a *wrong*
        // list: whatever the enclosing scope had.
        let mut harness = Harness::new();
        harness.write(
            "app/hr.rb",
            "module HR\n  MAX = 1\n  class Person\nend\nend\n",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // `self::` is legal and rare: it means the nesting, so a module's own constants are what
        // may follow.
        //
        // The answer must match `HR::`, from inside the module and from inside one of its methods:
        // `::` asks about a namespace, which is the same either way.
        let named = harness.suggestions(&uri, "HR::~\n");
        assert_eq!(named, vec!["MAX".to_owned(), "Person".to_owned()]);
        assert_eq!(
            // Something must follow the line: `self::` with only an `end` after it is a *method*
            // call in Prism's recovery, with `::` as the call operator.
            harness.suggestions(&uri, "module HR\n  self::~\n  X = 1\nend\n"),
            named,
            "in a module body, `self` is the module"
        );
        assert_eq!(
            harness.suggestions(&uri, "module HR\n  def y\n    self::P~\n  end\nend\n"),
            vec!["Person".to_owned()],
            "and inside a method it is still the module that `::` asks about"
        );

        // An instance is not a namespace, nor is a literal. Both parse.
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

        // Alphabetical, because nothing has been typed for length or match quality to separate
        // them.
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
        // And the nested one, where a bare name lookup would have stopped.
        assert_eq!(
            harness.declarations_at(&uri, "HR::Person::NAME~\n"),
            vec!["NAME_LIMIT"]
        );
    }

    #[test]
    fn a_plain_extend_offers_the_module_s_methods_on_the_class_object() {
        // The extension edge in its **Ruby** spelling, the one this module still walks
        // (`workspace/rails/concerns.rs` declares Rails' own), so what remains here is a
        // hand-written `extend SomeModule`. Every refusal in the walk is in this fixture, since
        // each would otherwise offer a row wrongly:
        //
        // - `TOOL_LIMIT` is a constant, and `extend` installs only methods;
        // - `build` does not match what was typed, and the prefix is tested before the expensive
        //   ancestor walk;
        // - `shared_tool` is in *both* modules, and the nearer one answers;
        // - `tool_up` is a name the class already answers itself, and an extension only answers
        //   what nothing else did.
        let mut harness = Harness::new();
        harness.write(
            "app/tools.rb",
            "\
module Tools
  TOOL_LIMIT = 10

  def tool_up
  end

  def shared_tool
  end

  def build
  end
end

module Extras
  def extra_tool
  end

  def shared_tool
  end
end

class Widget
  extend Tools
  extend Extras

  def self.tool_up
  end
end
",
        );
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.declarations_at(&uri, "Widget.tool~\n");
        // Offered through the edge and nothing else.
        assert!(found.contains(&"extra_tool".to_owned()), "{found:?}");
        assert!(found.contains(&"shared_tool".to_owned()), "{found:?}");
        // Offered once, by the class's own `def self.tool_up`, not twice.
        assert_eq!(
            found.iter().filter(|name| *name == "tool_up").count(),
            1,
            "{found:?}"
        );
        assert_eq!(
            found.iter().filter(|name| *name == "shared_tool").count(),
            1,
            "{found:?}"
        );
        // A constant in an extended module is not reachable through the singleton.
        assert!(!found.contains(&"TOOL_LIMIT".to_owned()), "{found:?}");
    }

    #[test]
    fn a_constant_receiver_offers_singleton_methods_and_not_instance_ones() {
        // The distinction rubydex models with a synthetic singleton class, and why `Foo.` resolves
        // to `Foo::<Foo>`, not `Foo`.
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
        // `build` is a singleton method, not callable on an instance, so not offered.
        assert!(!found.contains(&"build".to_owned()), "{found:?}");
    }

    #[test]
    fn an_instance_receiver_is_ranked_by_ancestor_distance() {
        let mut harness = Harness::new();
        harness.write("app/store.rb", ANCESTRY);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // One method per rung, in Ruby's resolution order: the class, its included module, its
        // parent class, and `Object` past all three. Alphabetically this would read
        // `audit, global_helper, price, save`.
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

        // Alphabetical order would put `global_helper` first, the one row `String` does not
        // declare.
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

        // A module's `::` is its own contents and stops there. Alphabetical, because nothing is
        // typed and a namespace has no ancestor chain to measure, the same situation as the untyped
        // receiver.
        //
        // More important than the order is the missing last row from `Object`: rubydex's namespace
        // walk deliberately stops before `Object`'s members, or `String::` would list every
        // top-level constant, and `Object` is where everything unattributed ends up.
        assert_eq!(
            harness.first_rows(&uri, "Store::~\n", 10),
            [
                "Auditable  Store::Auditable",
                "DEFAULT_CURRENCY  Store::DEFAULT_CURRENCY",
                "Item  Store::Item",
                "Record  Store::Record",
            ]
        );

        // A *class* under `::` also carries its singleton chain, because `Store::Item.build` may be
        // written `Store::Item::build`. So the nested constant leads, the class's singleton method
        // follows, and `Object` sits at the bottom where distance puts it.
        //
        // That makes the lists above and below asymmetric: `Store.` offers `global_helper` and
        // `Store::` does not, though both name the same module object. That is rubydex's namespace
        // walk, not a rule here; the fixture makes the seam visible rather than closing it.
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

        // One letter separates them, the right way round: `Item` starts with it, `Auditable` merely
        // contains it (`aud-i-table`, folded). Alphabetically `Auditable` would lead, so this is
        // the namespace list whose order is a claim, and the row the user typed `I` for comes
        // first.
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

        // Two scales on one number. Constants are reached through the lexical nesting and methods
        // through the ancestor chain, so each walk counts from the cursor rather than end to end;
        // otherwise every method would sit below every constant, or the reverse.
        //
        // `Object` is where they meet: the last rung of the ancestor chain *and* the outermost
        // lexical scope. It must be scored as the former, which is why `save` on `Record` outranks
        // `global_helper`. Scored as the latter, everything rubydex could not attribute would land
        // one step from the cursor.
        assert_eq!(
            harness.first_rows(&uri, &ANCESTRY.replace("      audit\n", "      ~\n"), 12),
            [
                "LIMIT  Store::Item::LIMIT",
                // The receiver is implicit here, so Ruby permits both, and the three receiver lists
                // above must not contain either. That they do not is what makes this pair a guard.
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

        // The receiver is implicit and `self` is the *class*, not an instance, so this list mirrors
        // the one above. `build` is offered without a receiver, because that is where
        // `def self.build` can be called; `initialize`, `price` and `stash` are gone, because none
        // can be written here. Same fixture, same names, two cursors, and Ruby permits a different
        // set at each.
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
                // Last of the declarations, where `Object` belongs: everything rubydex could not
                // attribute lands there.
                "global_helper  Object#global_helper",
            ]
        );
    }

    #[test]
    fn an_untyped_receiver_falls_back_to_the_alphabet_when_nothing_is_nearer() {
        let mut harness = Harness::new();
        let uri = harness.write("app/store.rb", ANCESTRY);
        harness.index();

        // The seam, pinned deliberately. The typed and untyped paths share only a ranking
        // constructor: one walks a receiver's ancestor chain, the other is a flat name search with
        // no receiver. No chain means no distance, so this list is exactly what it was, which
        // argues against leaving it a list, not that it is fine.
        //
        // Every candidate is in this fixture's one file, so `Locality` scores them alike and says
        // nothing, as it should: a term inventing order from no information would be worse than the
        // alphabet.
        //
        // The receiver is written after the last `end` on purpose. Inside the method, Prism's
        // recovery eats that `end` and nests every later top-level class one level deeper: the same
        // six names, spelled `Store::String#shout`.
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

        // The names are spelled to sort the wrong way on purpose: `aaa_far` is the project's
        // alphabetically first method and belongs last; `zzz_near` is the last and belongs above
        // it. Nothing else separates them: both are the user's code, neither matches a prefix, and
        // there is no receiver chain.
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

    /// A project with a spec tree declaring three names nothing else does, plus one the application
    /// declares too.
    ///
    /// `spec_only_helper` is the common shape: a `def` at the top level of a spec file, or inside
    /// an `RSpec.describe … do` block, which rubydex files on `Object` because a block body is not
    /// a namespace. `Object` ends every ancestor chain, so such a name is on the list for every
    /// receiver.
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
        // Ruby will not find any of these from a model: only RSpec loads their file. `spec_shared`
        // is the control: the application declares it too, and a spec reopening a class does not
        // take the class away.
        //
        // **Both paths, because they reach a spec differently.** A bare word asks `self`'s
        // ancestors, which end at an `Object` the spec tree wrote on; an untyped receiver asks
        // [`by_name`], which matches the whole graph by letters with no chain.
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
        // The fence is about where a declaration was written, not its kind, so it reaches the
        // constant list by the same line. A test double nobody outside the suite can build is not a
        // name to offer in `app/`.
        let (mut harness, app, _) = a_project_with_a_spec_tree();

        let offered = harness.declarations_at(&app, &format!("{ANCESTRY}Spec~\n"));
        assert!(
            !offered.iter().any(|label| label == "SpecOnlyDouble"),
            "{offered:?}"
        );
    }

    #[test]
    fn the_same_names_are_offered_to_a_cursor_inside_the_test_tree() {
        // The other half, and why this is a fence, not a filter: a developer editing a spec is
        // exactly who a helper in the next spec file answers. `environment::fenced_from` settles it
        // for the name rung too.
        let (mut harness, _, spec) = a_project_with_a_spec_tree();

        assert_eq!(
            harness.first_rows(&spec, "spec_~\n", 8),
            [
                // `length` decides between two rows nothing else separates, so the shorter name
                // leads. See `order`.
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
        // The fence fires on **evidence** (every definition under a test tree), and rubydex's
        // built-ins have no definitions: `Object` and `Module` exist without a line of Ruby behind
        // them. That is why it is a count, not a running `bool`: a `bool` starting at *not
        // loadable* with nothing to flip it would drop built-ins from every list outside a test
        // tree.
        let (harness, app, _) = a_project_with_a_spec_tree();
        let graph = &harness.analysis.graph;
        let locality = Locality::at(
            graph,
            UriId::from(app.as_str()),
            harness.analysis.placed(),
            environment::Layout::default(),
        );
        assert!(
            locality.fenced.trees,
            "a cursor in app/ is what the fence is for"
        );

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
        // The deny-list reads path segments, and a gem's path is not the project's to reason about:
        // `rspec-core` and `minitest` live under directories nobody here named. They are kept
        // because `Locality` holds only the *workspace's* documents, so a definition it never
        // scored is loadable, not suspicious.
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
        // The project's own tree, so this exercises the `own` loop's flag, not the whole-graph pass
        // (a test on a gem's engine would pass without it). `db/migrate` is on no autoload path:
        // the task loads one file by path, and a helper above `def change` is reachable from that
        // file only.
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
        // The other half, and the one place `own` is the wrong set. A gem's `lib/rack/test/` is a
        // published library, so an unscored document must be loadable; but a tree `rails generate`
        // copies **from** is no more a library in a gem than in the project. fabrication's
        // `.../cucumber_steps/templates/fabrication_steps.rb` puts a top-level `def with_ivars` on
        // `Object`, an ancestor of every receiver. So the template tag reads the whole graph, and
        // the test tag stays on `own`.
        //
        // **The cursor is bare on purpose**: that path is decided by `Locality` alone. A receiver
        // would reach the name through `locator`, which reads the path directly and was already
        // fenced, so such a test would pass without this.
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

    /// A constant holding an *object* is not a class object.
    ///
    /// - **The bug.** Upstream promotes a constant used as a receiver into a `Namespace::Todo` ("a
    ///   namespace I never saw defined"), whose singleton's ancestors are `Class`, `Module` and
    ///   `Object`. `ENV: RBS::Unnamed::ENVClass` and `URI::RFC2396_PARSER: URI::RFC2396_Parser`
    ///   have that shape, and completing against the singleton offered `alias_method` and
    ///   `attr_accessor` for `ENV.`, precisely, wrongly, *instead of* the name-based list.
    /// - **Pinned both ways.** The members of the class the signature says it holds are offered
    ///   (via `types::held_by`: `HOLDER: Vault::Store` names the object's type, so the decoy
    ///   `unlock` the name rung would add is gone), and the singleton's own are not.
    #[test]
    fn a_constant_that_holds_an_object_is_not_a_class_object() {
        let mut harness = signed(
            &[(
                "core/s.rbs",
                "module Vault
  module Store
    def unlock: () -> String
  end
end

             HOLDER: Vault::Store
",
            )],
            "",
        );
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

    /// The same constant, typed by the Ruby that assigns it instead of a signature.
    ///
    /// The sibling of the test above: no signature will ever declare an application's own config
    /// object, and the line building it says the same thing. Completion is where the `Todo`
    /// singleton cost most (an empty list or `attr_accessor`), so the rung's second half is pinned
    /// here too.
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
        // Every spelling of `extend`, qualified or not, Ruby or RBS, indexed together with its
        // namespace. rubydex links all of these itself, so this holds even without
        // `locator::extends_written_on`; the repair is for an `extend` indexed *later* (the test
        // below).
        //
        // The fixture is `stdlib/securerandom/0/securerandom.rbs` written out: most `extend`s in
        // the vendored signatures are qualified, and this is the commonest.
        let mut harness = signed(
            &[(
                "core/s.rbs",
                "module Random\n  module Formatter\n    def hex: (?Integer) -> String\n  end\nend\n\n\
             module Joined::Deep\n  def joined: () -> String\nend\n\n\
             module Flat\n  def flat: () -> String\nend\n\n\
             module SecureRandom\n  extend Random::Formatter\nend\n\n\
             module JoinExt\n  extend Joined::Deep\nend\n\n\
             module FlatExt\n  extend Flat\nend\n",
            )],
            "",
        );
        // The same edge written in Ruby, which rubydex resolves itself and must keep resolving: the
        // repair must never be the only answer for a shape rubydex handles.
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
                !found.contains("Guessed from name alone"),
                "{needle} resolves rather than guessing: {found}"
            );
        }

        // The completion side of the same edge. Resolution takes the first answer and completion
        // collects all, so the walk is shared, and the list is asked here as well as the card.
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
    fn an_extend_written_after_the_first_resolve_is_still_read() {
        // The test above indexes everything at once. This one writes the `extend` *after* the graph
        // settled (an edit, a second file reopening the module, a new signature in `sig/`), as an
        // agent editing through a shell or a user typing would. rubydex schedules a namespace's
        // ancestors only when it *creates* the declaration, so only the first case is rubydex's
        // answer; the other three are `locator::extends_written_on`'s, and each fails without that
        // repair.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "app/fmt.rb",
            "module Ns\n  module Fmt\n    def solo_m\n    end\n\n    def twice_m\n    end\n\n    \
             def reopened_m\n    end\n  end\nend\n",
        );
        harness.write(
            "sig/random.rbs",
            "module Random\n  module Formatter\n    def hex: (?Integer) -> String\n  end\nend\n",
        );
        let solo = harness.write("app/solo.rb", "module Solo\nend\n");
        harness.write("app/twice_a.rb", "module Twice\n  def a\n  end\nend\n");
        let twice_b = harness.write("app/twice_b.rb", "module Twice\nend\n");
        harness.write("app/reopened.rb", "module Reopened\nend\n");
        harness.write("app/late.rb", "module Late\nend\n");
        let source = "Solo.solo_m\nTwice.twice_m\nReopened.reopened_m\nLate.hex\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        // A module declared in one file, which that file's edit deletes and re-creates.
        harness.write("app/solo.rb", "module Solo\n  extend Ns::Fmt\nend\n");
        // A module declared in two files, so editing one leaves the declaration standing.
        harness.write("app/twice_b.rb", "module Twice\n  extend Ns::Fmt\nend\n");
        // A new file reopening a module the graph already holds.
        let reopening = harness.write(
            "app/reopening.rb",
            "module Reopened\n  extend Ns::Fmt\nend\n",
        );
        // A new signature reopening one, qualified: `stdlib/securerandom`'s shape.
        let signature = harness.write(
            "sig/late.rbs",
            "module Late\n  extend Random::Formatter\nend\n",
        );
        harness.watch(&[&solo, &twice_b, &reopening, &signature]);

        let mut wrong = Vec::new();
        for (needle, owner) in [
            ("solo_m", "Ns::Fmt#solo_m"),
            ("twice_m", "Ns::Fmt#twice_m"),
            ("reopened_m", "Ns::Fmt#reopened_m"),
            ("hex", "Random::Formatter#hex"),
        ] {
            let found = card(&mut harness, &uri, source, needle);
            if !found.contains(owner)
                || found.contains("possible definitions")
                || found.contains("Guessed from name alone")
            {
                wrong.push(format!("{needle}: {found}"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[test]
    fn a_call_on_a_class_object_is_never_offered_an_instance_method_of_an_unrelated_class() {
        // The fallback filter, the half a user meets first. The name-based list was every
        // declaration ending in this name; a class object answers from its singleton chain, which
        // the search above already walked, so a candidate owned by a `class` is provably
        // unreachable, while one owned by a `module` must stay (a `ClassMethods` is a module's
        // instance method).
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

        // Two of three candidates survive: a module's instance method (what `extend` installs), and
        // another class's **singleton** method (a class object is what it would be called on). Only
        // `Widget#spin` is dropped.
        let narrowed = card(&mut harness, &uri, source, "spin\n");
        assert!(narrowed.contains("2 possible definitions"), "{narrowed}");
        assert_eq!(
            harness.candidates_at(&uri, source, "spin\n"),
            ["Gadget.spin", "Spinner#spin"],
            "an instance method of an unrelated class is not reachable here"
        );

        // Emptied where that is the truth. `whirl` is only another class's instance method, which
        // `Story`'s class object cannot reach, so there is no card rather than a guess that is
        // never right: a `Settings::General.app_domain` landed on a configuration setting.
        let emptied = harness.hover_at(&uri, source, "whirl");
        assert!(emptied.is_null(), "{emptied}");
    }

    #[test]
    fn a_class_body_completes_the_class_methods_of_every_concern_it_includes() {
        // Without this, the same cursor that *resolves* `validates` would complete to an empty
        // list: resolution takes one answer and completion collects, `query::completion_candidates`
        // walks the singleton's ancestors, and the `extend` is on none of them.
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

        // The decline, the same gate from the collecting side. `Plain` has no nested
        // `ClassMethods`, so nothing of it is extended: its `helper` is an instance method of every
        // *record*, and this cursor is a class object. `Odd`'s `ClassMethods` is a constant, not a
        // module, so it is declined, and the list is as if the constant were absent. `ClassMethods`
        // itself *is* offered, as a constant: Ruby resolves it through the cref's ancestors, and
        // rubydex agrees, unrelated to this edge.
        let body = harness.declarations_at(&uri, "class Story < ApplicationRecord\n  ~\nend\n");
        assert!(!body.contains(&"helper".to_owned()), "{body:?}");
    }

    /// Rails' own idiom, worth measuring before believing: an `include` written **inside a `def`**
    /// in a `ClassMethods` module is for the class the macro is called on, and rubydex records it
    /// as a mixin of the enclosing module. Copied exactly from `ActiveModel::SecurePassword`.
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
        // Why the walk takes the module's own members, not its ancestors'. `extend M` really
        // installs `M`'s ancestors' methods too, so an ancestor walk reads Ruby right, but in
        // Rails' core gems most `module ClassMethods` mixins are written inside a `def`, each
        // meaning the class the macro was called on. `Story.valid?` raises in Ruby.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", SECURABLE);
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        // Written, not opened, because `hover_at` asks about the indexed file. It is also asked
        // **before** any completion, which opens a buffer over it: two requests about one document
        // ask about whatever text it holds now, and reversing them would measure the last thing
        // typed.
        let source = "Story.valid?\n";
        let uri = harness.write("app/models/probe.rb", source);
        harness.index();

        // Resolution reads the same walk, which is why it is shared: otherwise `Story.valid?` would
        // resolve, precisely, to a method Ruby raises on.
        let found = card(&mut harness, &uri, source, "valid?");
        assert!(
            found.contains("Guessed from name alone"),
            "the name rung is the honest answer here: {found}"
        );

        let offered = harness.declarations_at(&uri, "Story.vali~\n");
        assert!(
            !offered.contains(&"valid?".to_owned()),
            "an instance method of a module a macro includes into the record: {offered:?}"
        );

        // The macro itself is a member the module really declares, and it still answers.
        let macros = harness.declarations_at(&uri, "Story.has_secure~\n");
        assert!(
            macros.contains(&"has_secure_password".to_owned()),
            "{macros:?}"
        );
    }

    #[test]
    fn a_class_object_completes_what_a_concern_extends_onto_it_however_it_is_written() {
        // Three spellings of one receiver, all answered by rubydex with the singleton, which is why
        // `class_object` passes what the receiver holds instead of testing syntax.
        // `Story::validates` is legal and rare; it is here because `NamespaceAccess` is the one arm
        // that must find the singleton itself.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        // A second document, because a buffer is what the cursor completes *in*: writing
        // `Story.valid` into `story.rb` would replace its `class Story`, the receiver would resolve
        // to nothing and reach the name-based list, which offers `validates` too, and the test
        // would pass for the wrong reason.
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

        // Proof the receiver really resolved: `Plain#helper` is an instance method of every record
        // and on no class object's chain, so only the name-based list would offer it here.
        let precise = harness.declarations_at(&uri, "Story.help~\n");
        assert!(!precise.contains(&"helper".to_owned()), "{precise:?}");
    }

    /// What declaring `ClassMethods` does **not** fix, pinned so it is a fact, not an assumption.
    ///
    /// `class_methods do` is the common spelling of the concern edge, and
    /// `workspace/rails/concerns.rs` writes the `ClassMethods` module down so the existing walk
    /// finds it. But rubydex has no namespace for a block body, so a `def` inside
    /// `class_methods do` is also filed on the enclosing scope, as an *instance* member of the
    /// concern, and every including class gets it on its instance side. That is wrong in Ruby:
    /// `ActiveSupport::Concern` `module_eval`s the block on a module it then **extends**, so the
    /// method is on the class object only.
    ///
    /// Declaring the module adds the right answer; it cannot remove the wrong one, which is a real
    /// graph definition that no generated declaration overwrites. **The class side does not double
    /// up**, which is what could have gone wrong: `Ledger.tally_by` is the generated member alone.
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

        // The **container** is the assertion, not the label: a label alone would pass on the name
        // rung, where `Tallyable#tally_all` (rubydex's filing of the same `def`) sits one module
        // away. `Ledger.tally_by` is the class side, which only the fan-out onto the includer
        // provides.
        let rows = harness.first_rows(&uri, "Ledger.tally~\n", 8);
        assert_eq!(
            rows,
            vec![
                "tally_by  Ledger.tally_by".to_owned(),
                "tally_all  Ledger.tally_all".to_owned(),
            ],
            "both public defs of the block, on the class that includes the concern"
        );

        // Three declines, each a different Ruby rule. `named_private` and `after_private` are the
        // file's own visibility, in both spellings. `not_extended` is a `def self.`: a singleton
        // method of the `ClassMethods` module itself, which `extend` installs nowhere.
        for declined in ["named_private", "after_private", "not_extended"] {
            let offered = harness.declarations_at(&uri, &format!("Ledger.{declined}~\n"));
            assert!(
                !offered.contains(&declined.to_owned()),
                "{declined} is not extended onto the includer: {offered:?}"
            );
        }

        // `class_methods do` in a **class** raises `NoMethodError` (`class_methods` is defined on
        // `ActiveSupport::Concern`, which extends modules). `Ledger` writes one, and it declares
        // nothing.
        let never = harness.declarations_at(&uri, "Ledger.never~\n");
        assert!(!never.contains(&"never_reached".to_owned()), "{never:?}");
    }

    /// The member the block declares is a place, and the place is the `def` the user wrote.
    ///
    /// A generated declaration usually cannot have this: `synthesized.rs` maps one to a real source
    /// span only where a generator recorded it. Here the `def` really is in the file, so the jump
    /// lands on that line, not on a macro that implied it.
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

        // **Neither the line nor the file proves it.** rubydex files this `def` as
        // `Tallyable#tally_all` too, so the name rung answers the same line: a jump that looks
        // right for the wrong reason. The *tier* differs: the walk resolves the receiver's
        // singleton chain, and the name rung says it guessed in a footnote.
        let card = harness.hover_at(&uri, "class Probe\n  Ledger.tally_all\nend\n", "tally_al")
            ["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            !card.contains("Guessed from name alone"),
            "not the name rung: {card}"
        );
    }

    #[test]
    fn one_name_two_concerns_is_one_row_and_a_constant_in_a_class_methods_is_none() {
        // Two declines the walk inherits from rubydex. A member declared by two concerns is offered
        // once, by the nearer: `include Countable` comes after `include Recountable`, so Ruby's
        // linearization puts Countable first, as rubydex's own dedup would. And a **constant**
        // nested in `ClassMethods` is not a row: `extend` installs methods, and
        // `Countable::ClassMethods::LIMIT` is reachable only through the constant path.
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
        // The ranking half. `Distance::from_receiver` seeds only the chains rubydex is about to
        // walk, and an extended module is on none of them, so its members would land at
        // `NO_DISTANCE`: last, *behind `Object`'s own methods*, backwards for what a model body
        // most likely types. The seed is `locator::Extension::step`, which counts classes the chain
        // passes, not ancestors, because the instance and singleton chains differ in length.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/application_record.rb", CONCERNS);
        harness.write("app/models/concerns/countable.rb", OWN_CONCERN);
        // An `Object` method matching the same prefix, answered by every class object since
        // `Object` ends every chain: the row the extended one must beat.
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
        // Dedup per member, not per request: an extension answers only what the ordinary walk did
        // not. Two rows of one name is the visible failure; the invisible one is which the editor
        // accepts on `tab`.
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
        // The discriminator is rubydex's, not syntax, and this is its other side: inside a `def`,
        // and after `Story.new.`, `self` is a record, so the extension edge does not apply and
        // `Plain#helper` (excluded in a class body) is exactly what does.
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
        // The ranking `synthesized.md` says to pin with a real schema: what the class was written
        // to do, then what its table holds, then what every object can do. `Locality` scores a
        // generated document by the file that implied it, so the columns are the user's code one
        // directory further away, not somebody else's code. The test below separates the two
        // readings.
        let source = "Story.new.\n";
        let (mut harness, _schema, uri) = rails_project(source);
        let model = harness.write(
            "app/models/story.rb",
            "class Story\n  def summary\n  end\nend\n",
        );
        harness.watch(&[&model]);

        // The table's members include what Rails' attribute methods define around each column
        // (`x=`, `x?`, `x_changed?`, `x_was`, `saved_change_to_x?`), from the same schema line.
        assert_eq!(
            harness.declarations_at(&uri, "Story.new.~\n"),
            vec![
                "summary",
                "description",
                "description=",
                "description?",
                "description_changed?",
                "description_was",
                "id",
                "id=",
                "id?",
                "id_changed?",
                "id_was",
                "saved_change_to_description?",
                "saved_change_to_id?",
                "saved_change_to_tags?",
                "saved_change_to_title?",
                "tags",
                "tags=",
                "tags?",
                "tags_changed?",
                "tags_was",
                "title",
                "title=",
                "title?",
                "title_changed?",
                "title_was",
                "tap"
            ]
        );
    }

    #[test]
    fn what_the_class_was_written_to_do_leads_what_its_table_says_it_holds_from_anywhere() {
        // **The ordering is a property of the declaration's kind, not its directory.** A column is
        // declared in `db/schema.rb` and the method beside it in `app/models/story.rb`, so leaving
        // them to `Locality` orders them by which the cursor is nearer, and from `db/` that inverts
        // the test above. `generated` makes the order hold from both ends, and only this test says
        // so: the audit, scored on rank 1, would read removing the term as a slight improvement.
        let (mut harness, _schema, _uri) = rails_project("");
        let model = harness.write(
            "app/models/story.rb",
            "class Story\n  def summary\n  end\nend\n",
        );
        // Beside the schema and three steps from the model, which is the point.
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
        // **The case the fixture above cannot separate, and the corpora's common one.** `group`,
        // the first key term, asks "is this the user's code", answered by whether `Locality` scored
        // the document. A generated document has no path, so the answer would be *no*, and a method
        // the project patches onto `Object` would outrank every column, association and enum of the
        // receiver's table: `Object` is the far end of the chain but the patch file is user code,
        // and `distance` never gets to speak. On a real app, two `Object` patches took ranks 1 and
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

        // `rb_add_method` makes five names private at definition, so `def initialize` is private
        // whatever its class said, and the graph does not record it. rbs is inconsistent too:
        // `Kernel#initialize_copy` is private but `String#initialize_copy` public, so `"hi".` would
        // offer both while `count.` offers only `initialize`.
        let outside = harness.declarations_at(&uri, "Store::Item.new.~\n");
        assert!(outside.contains(&"price".to_owned()), "{outside:?}");
        assert!(!outside.contains(&"initialize".to_owned()), "{outside:?}");

        // The guard, so this cannot pass by banning the name outright: since Ruby 2.7, a private
        // call on a receiver written `self` is allowed, `initialize` included.
        let inside =
            harness.declarations_at(&item, &ANCESTRY.replace("      audit\n", "      self.~\n"));
        assert!(inside.contains(&"initialize".to_owned()), "{inside:?}");
    }

    #[test]
    fn a_private_method_needs_a_receiver_written_self() {
        let mut harness = Harness::new();
        let item = harness.write("app/store.rb", ANCESTRY);
        harness.index();

        // rubydex passes a private method whenever the caller's `self` is the receiver's *class*.
        // Ruby exempts only a receiver *written* `self`, so this would offer `stash` inside `Item`,
        // where Ruby raises `NoMethodError`.
        let other = harness.declarations_at(
            &item,
            &ANCESTRY.replace("      audit\n", "      Store::Item.new.~\n"),
        );
        assert!(other.contains(&"price".to_owned()), "{other:?}");
        assert!(!other.contains(&"stash".to_owned()), "{other:?}");

        // `::` is a method call too, exempted on the same terms; both checked against a real
        // interpreter.
        for marked in ["      self.~\n", "      self::~\n"] {
            let found = harness.declarations_at(&item, &ANCESTRY.replace("      audit\n", marked));
            assert!(found.contains(&"stash".to_owned()), "{marked}: {found:?}");
        }
    }

    #[test]
    fn a_public_method_below_a_block_holding_private_is_still_offered() {
        // The same wrong record the jump reads (a bare `private` inside a block, applied by rubydex
        // to every `def` after the *block*), asked of the list. Both surfaces must repair it, or a
        // jump lands on `upsert_custom_fields` while the list will not offer it. See
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
        // **The receiver must type, or this measures the name-based path.** Only `from_graph`
        // carries the repair (`by_name` walks the whole graph and deliberately keeps rubydex's
        // record), so the cursor is written into the indexed file rather than replacing it, which
        // lets the rebase place `Category` in graph coordinates.
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
        // rubydex handles private visibility, but only if it is given the caller's `self`. Left
        // unset it defaults to nothing, every call site becomes an outsider, and a class cannot see
        // its own private methods.
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
        // Inside `def self.build` the lexical scope is `Person` but `self` is its singleton class,
        // and the two give different lists. Getting it wrong offers instance methods that raise
        // `NoMethodError` when accepted.
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
        // Where the whole Rails DSL lives. `self` in a class body is the class object, so what may
        // be written there is its *singleton* methods: `validates`, `has_many`, `scope`. Completing
        // against the instance side offers `valid?` and omits every macro.
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

        // An instance method body is the reverse, which is the whole distinction.
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
        // Ruby refuses the `class` keyword in a method body, so `Class.new(base) do … end` is the
        // only way to write one there, and rubydex records the block as a class. It is
        // `class_eval`'d, so `self` inside is the new class *object*, as in a written class body,
        // whatever the enclosing `def` says.
        //
        // The two bodies nest, so the innermost decides. Reading the innermost `def`
        // unconditionally would offer the base's instance members and not the DSL, which is why
        // anyone writes the block (seen in a corpus at a `define_method` inside one, where the list
        // was a single row: `define_singleton_method`, from `Object`).
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

        // A `def` *inside* the block reverses it again, by the same rule: the innermost body is now
        // that method.
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
    /// `DSL` is the reported shape: a macro that takes a block and runs it against something other
    /// than the class object. The other four cursors are refusals, each for a different reason:
    /// `self` fixed by a `def`, a module with no instances, a written receiver, and an expression
    /// with no `self` at all.
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
        // The bug: `hover` in such a block names a member on an instance (see `locator`'s closure
        // rung), while `completion` at the same byte stayed on the class object, so a card named a
        // method the list lacked.
        let mut harness = Harness::new();
        let uri = harness.write("app/parser.rb", DSL);
        harness.index();

        let offered = harness.declarations_at(
            &uri,
            &DSL.replace("  rule(:colon) do\n", "  rule(:colon) do\n    ~\n"),
        );
        // The instance side, inherited rungs included (the half the name rung dropped), and a
        // private one, since no receiver was written.
        assert!(offered.contains(&"digits".to_owned()), "{offered:?}");
        assert!(offered.contains(&"go".to_owned()), "{offered:?}");
        assert!(offered.contains(&"secret".to_owned()), "{offered:?}");
        // The class object's own, which is what `self` stays until the DSL says otherwise.
        assert!(offered.contains(&"rule".to_owned()), "{offered:?}");
        // **Once.** `shared` is on both sides; the class object answers it, so no instance row is
        // added: the order `locator` keeps by reaching its closure rung only after `resolve_call`
        // came back imprecise.
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

        // A `def` fixes `self` and a block inside it cannot unfix it, so this is the instance side
        // as always, and the class object's `rule` is *not* on it.
        let in_a_method = harness.declarations_at(
            &uri,
            &DSL.replace("    [1].each do\n", "    [1].each do\n      ~\n"),
        );
        assert!(
            in_a_method.contains(&"digits".to_owned()),
            "{in_a_method:?}"
        );
        assert!(!in_a_method.contains(&"rule".to_owned()), "{in_a_method:?}");

        // A module has no instances for the claim to be about, and `included do` rebinds `self` to
        // the *including* class, reachable from neither side of this file.
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

        // A written receiver decides `self` whatever block it is in; refused by
        // `Cursor::in_a_closure`, which is `false` with a written receiver.
        let written = harness.declarations_at(
            &uri,
            &DSL.replace("  rule(:colon) do\n", "  rule(:colon) do\n    Base.~\n"),
        );
        assert!(written.contains(&"shared".to_owned()), "{written:?}");
        assert!(!written.contains(&"digits".to_owned()), "{written:?}");

        // `::` asks the top level with no `self`, and may be followed only by constants, so an
        // instance method there would be a syntax error offered as a row. The same gate refuses it,
        // which is why the gate is on the context, not the receiver: this arrives as a
        // `CompletionReceiver::Expression` like a bare word, and only the context remembers the
        // `::`.
        let rooted = harness.declarations_at(
            &uri,
            &DSL.replace("  rule(:colon) do\n", "  rule(:colon) do\n    ::~\n"),
        );
        assert!(rooted.contains(&"Parser".to_owned()), "{rooted:?}");
        assert!(!rooted.contains(&"digits".to_owned()), "{rooted:?}");
    }

    #[test]
    fn the_card_in_a_block_and_the_list_beside_it_name_the_same_member() {
        // The pair the bug is *about*, at one byte, both ways: a name only an instance has comes
        // back as the instance's on both surfaces, and a name both sides have comes back as the
        // class object's on both.
        let source = DSL.replace(
            "  rule(:colon) do\n",
            "  rule(:colon) do\n    secret\n    shared\n",
        );
        let mut harness = Harness::new();
        let uri = harness.write("app/parser.rb", &source);
        harness.index();

        assert_eq!(
            card(&mut harness, &uri, &source, "secret\n"),
            "```ruby\nprivate Base#secret\n```"
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

        // In the signature's order, not alphabetical: a signature is a list and its order is
        // information, unlike a namespace's members.
        let found = harness.declarations_at(&uri, "HR::Person.build(~)\n");
        assert_eq!(&found[..2], ["name:", "age:"], "{found:?}");
    }

    #[test]
    fn keyword_arguments_are_never_guessed_from_a_name() {
        // `person` is a local, so the call resolves by name alone and could be any `build`.
        // Offering `name:` there would be a syntactically valid wrong answer, so the argument list
        // degrades to a plain expression.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        let found = harness.declarations_at(&uri, "person = nil\nperson.build(~)\n");
        assert!(!found.contains(&"name:".to_owned()), "{found:?}");
    }

    #[test]
    fn a_receiver_with_no_type_falls_back_to_every_method_name() {
        // The one context ya-lsp cannot answer exactly. It answers with names rather than nothing,
        // because the editor's word list cannot see methods in unopened files.
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
        // The same call `workspace/symbol` makes, for the same reason: a project has thousands of
        // declarations and its bundle a hundred times more.
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
        // rubydex's namespace walk stops before `Object`'s own members, so asking it about `Object`
        // directly returns nothing. `::` must be asked as an expression and filtered, and the
        // filter matters: a method or keyword after `::` is not valid Ruby.
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
        // Ruby spells "internal" with an underscore, which sorts before letters, so without the
        // rule a Rails user's first row after `Model.` is `__send`.
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
        // Typing the underscore means it: someone who writes `_` wants exactly these.
        assert_eq!(
            harness.declarations_at(&uri, "Thing._~\n"),
            vec!["_internal"]
        );
    }

    #[test]
    fn a_name_rubydex_invented_is_never_offered() {
        // `Class.new` gets named `<uri>:<offset><anonymous>`, which nobody can type; in a large
        // workspace `::` answered with a page of them.
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
        // The same names stay out of the symbol picker, which shares the test.
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
        // After a `.`, a keyword is not legal to write.
        assert!(
            !harness
                .suggestions(&uri, "HR::Person.de~\n")
                .contains(&"def".to_owned())
        );
    }

    #[test]
    fn a_comment_is_answered_with_null_and_an_empty_scope_with_a_list() {
        // Two different "nothing"s: `null` tells the client to use its own word list, an empty list
        // tells it not to.
        let mut harness = Harness::new();
        let uri = harness.write("app/main.rb", "");

        assert!(harness.complete(&uri, "# take ~\n").is_null());
        assert!(harness.complete(&uri, "\"a string ~\"\n").is_null());

        let empty = harness.complete(&uri, "Nowhere::~\n");
        assert_eq!(empty["items"], serde_json::json!([]));
    }

    #[test]
    fn the_half_typed_word_is_replaced_rather_than_appended_to() {
        // Without an explicit edit range, the client guesses word boundaries from its own pattern,
        // and Ruby's `?`, `!` and `@` are exactly where that guess goes wrong.
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
        // The flag costs the client a request per keystroke, so it is set only where it matters:
        // only the cap can drop a row a longer prefix would reach, because every filter here is a
        // subsequence match.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", OFFICE);
        harness.index();
        let uri = harness.write("app/main.rb", "");

        assert_eq!(harness.complete(&uri, "HR::~\n")["isIncomplete"], false);
        assert_eq!(harness.complete(&uri, "sh~\n")["isIncomplete"], false);
    }

    #[test]
    fn a_list_with_nothing_in_it_is_still_incomplete() {
        // Not the statement above repeated. A route answering no rows had nothing to say, and "the
        // complete answer is nothing" would make the client stop asking as the word grows.
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

    /// Enough distinct method names to push the whole-project guess over its admission ceiling,
    /// plus a family between the two ceilings and one name nothing else matches.
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
        // An untyped receiver falls to the name-based list, which at an empty prefix is every
        // method name in the project: tens of thousands, with the typed word ranked in the
        // thousands.
        //
        // Two ceilings, because the evidence splits them: across the corpora the word is within the
        // first `MAX_UNTYPED_COMPLETION_ITEMS` rows of every guess with up to
        // `MAX_UNTYPED_CANDIDATES` candidates. So the *count* cannot be trusted but the order can:
        // refuse on candidates, cut on rows.
        let mut harness = Harness::new();
        harness.write("app/many.rb", &three_regimes_of_guess());
        harness.index();
        let uri = harness.write("app/main.rb", "");

        // `thing` is an unassigned local: no receiver to type, no chain.
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

        // Between the ceilings: few enough candidates to trust the ranking, more rows than anyone
        // reads. The answer is the best of them, not all or none.
        let cut = harness.complete(&uri, "thing.beta~\n");
        assert_eq!(
            cut["items"].as_array().map(Vec::len),
            Some(MAX_UNTYPED_COMPLETION_ITEMS),
            "a guess the ranking can be trusted with is cut to what is readable"
        );
        assert_eq!(cut["isIncomplete"], true);

        // Under both, all of it: same receiver, same missing type, a prefix narrow enough that the
        // rest is a list a person would read to the end.
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
        // `GALLERY`'s treatment for the four answers a `.` can get, in one picture: they are
        // **different kinds of answer**, not different lengths of one. Read the right-hand column:
        //
        // - **`"hi".upca`** has a receiver the graph names. Its rows are `String`'s own members,
        //   bounded by the response ceiling.
        // - **`thing.`** has no receiver, so the candidates are every project method name, attached
        //   to no class. Nothing is offered: at an empty prefix `tier` and `length` tie, so there
        //   is no order to keep the best of.
        // - **`thing.beta`** has the same absent receiver but enough of a word to trust the
        //   ranking: few enough candidates to admit, more rows than anyone reads, so it is cut to a
        //   readable length.
        // - **`thing.zebra`** narrows to one, and the whole guess is the answer.
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
        // **`tier` leads the key.** With `group` first, a name in the user's code outranked
        // everything before match quality was consulted, so a word typed in full lost to any
        // project name matching it as a *subsequence*. Measured, putting `tier` first nearly
        // eliminates words ranked past 128 one character in.
        //
        // Tested with a keyword, because `group` still has three bands and this is the one the
        // harness can reach: `end` is in band 2, below every declaration (keywords first would fill
        // the top ten at a bare cursor). Typed in full it comes back, which is this test.
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
        // `[types] guess_from_names = false` lets a user have only answers this server can defend.
        // The name-based list is the same guess one request over (matched on the word, owned by no
        // class), so the setting must reach it too, or it silences every "guessed" *card* but none
        // of the lists built the same way.
        //
        // It gates that arm only: a receiver the graph can name is not a guess.
        let mut off = signed(
            &[("core/core.rbs", TYPED_RBS)],
            "\n\
                 [types]\nguess_from_names = false\n",
        );
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
        // The list carries no documentation: five hundred rows, one of them read.
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
        // A list is built, the config reloads, the graph is rebuilt, and the user arrows onto a row
        // from the old list. That row's `data` is a declaration id nothing answers to, coming from
        // the client, and rubydex ids are hashes, so a stale one looks like a wrong one. The
        // protocol says the item comes back either way; enriching it is the optional half.
        let mut harness = Harness::new();
        harness.write("app/hr.rb", "class Person\nend\n");
        harness.index();

        for data in [
            // A well-formed row naming a declaration that is gone.
            serde_json::json!({ "declaration": "1234567890123456789", "precise": true }),
            // And shapes a client can send that are not rows at all.
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

    /// One name in every Ruby sigil namespace, chosen so each would answer the others' prefixes if
    /// a sigil were fuzzy-matched like a letter.
    ///
    /// `entry` is in all five names so every list below is a real question, not a coincidence of
    /// spelling. `$@` is the case this fixture exists for: a global whose name after the sigil *is*
    /// an `@`, so a subsequence match on `@` reaches it and nothing else.
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

    /// [`SIGILS`] with `line` written into `#record`'s body; the `~` marks the cursor.
    ///
    /// One buffer, not two (unlike `ANCESTRY`): an instance variable belongs to the `self` it was
    /// assigned on, so the cursor must be in the class that assigns it.
    fn sigils_at(line: &str) -> String {
        SIGILS.replace("CURSOR", line)
    }

    #[test]
    fn an_instance_variable_prefix_reaches_no_other_namespace() {
        // The whole list, and what is not in it: no `$@`. Nor `$entry_log`, `ENTRY_LIMIT` or either
        // method: each contains `entry`, and none can start with `@`.
        //
        // `@@entry_total` *is* here, rightly: one `@` is on the way to two.
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
        // The other direction, which must not be symmetric: `@` admits `@@` because the second
        // character may be coming, and `@@` admits no `@name` because nothing typed can turn one
        // into the other.
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
        // `$@` is a fine answer *here*. It is not a bad row; it belongs to one prefix.
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
        // Deliberately unchanged, and pinned so it stays deliberate. With no sigil typed, the user
        // has not chosen a namespace, and the client filters as they type, so `entr` offers the
        // constant, both instance variables, the class variable and the global; accepting one
        // writes its sigil in.
        //
        // It does not offer `entry`, the local one line above: rubydex's graph holds no locals
        // (which is why `scopes.rs` walks Prism itself). A missing feature, not a wrong answer,
        // noted here because a first-ten list is where an absence shows.
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
        // The tier vocabulary, extended to the guess. A guessed receiver is not the name-based list
        // (the rows really are one class's members, a better list), so `precise` stays true and a
        // second field carries the doubt. Saying nothing would present six letters of inference as
        // a resolved type.
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
            card.contains("Guessed from name alone"),
            "the rows are a real class's members, and the class was a guess: {card}"
        );
    }
}
