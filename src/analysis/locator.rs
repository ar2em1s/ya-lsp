//! What the cursor is on, which declaration that is, and where the declaration lives.
//!
//! Every navigation request goes through here (`hover`, `definition`, `references`), asking the
//! same three questions in the same order, so their answers stay consistent.
//!
//! # Why the narrowest span wins
//!
//! rubydex records a document's definitions, constant references and method references separately,
//! and their spans nest freely. The cursor on `name` inside `Person#shout` is inside a method
//! reference (4 bytes), the `shout` definition (70 bytes) and the `Person` definition (394 bytes)
//! at once. The innermost is what the user pointed at.
//!
//! Width is the second test, not the first: a span that **begins** at the cursor beats one that
//! merely covers it, whatever their widths. `locate` explains why at that line.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use rubydex::{
    model::{
        declaration::{Ancestor, Declaration, Namespace},
        definitions::{Definition, Mixin, Receiver},
        document::Document,
        graph::Graph,
        ids::{DeclarationId, DefinitionId, NameId, StringId, UriId},
        name::ParentScope,
        references::{ConstantReference, MethodRef},
        visibility::Visibility,
    },
    offset::Offset,
    query::{self, FindMemberError},
};

use ruby_prism::{
    ArgumentsNode, BlockNode, CallNode, ClassNode, DefNode, LambdaNode, ModuleNode,
    SingletonClassNode, Visit,
};

use crate::workspace::DocUri;

use super::{
    cursor::{self, Context},
    environment,
    indexed::{self, Indexed},
    position::{ByteSpan, Rebase},
    render, scopes,
    synthesized::{Origin, Synthesized},
    types::{self, Derivation, Tier},
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

/// A [`Target`] plus the span the cursor landed in.
///
/// That span is what an editor underlines for a definition link, so it must be the *reference's*
/// span, not the span of what it resolves to.
#[derive(Debug, Clone, Copy)]
pub struct Located<'g> {
    pub start: u32,
    pub end: u32,
    pub target: Target<'g>,
}

/// An instance variable at the cursor, and every assignment sharing its `self`.
///
/// - **The graph does not model this.** rubydex records an instance variable's *assignment* as a
///   declaration and records no references, so [`locate`] finds nothing at a read and the
///   declaration itself at a write. Neither answers what `@title` means: every place in this `self`
///   it is written.
/// - **In buffer coordinates, not graph coordinates**, because [`scopes`](super::scopes) reads the
///   buffer. Nothing here is or may be rebased: the answer is about the text the client holds this
///   keystroke, as `documentHighlight`'s is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    /// The span the cursor landed in: what an editor underlines.
    pub start: u32,
    pub end: u32,
    /// Every write, in source order. Empty where this file only reads the variable: one a
    /// superclass assigns, or a template's, which its controller assigns.
    pub writes: Vec<(u32, u32)>,
}

/// A macro's symbol argument at the cursor, and the span an editor underlines.
///
/// - **Another thing the graph does not hold**, differently from [`Variable`]: rubydex models an
///   instance variable wrongly, and a symbol literal not at all.
/// - **So this is asked *after* [`locate`].** No span is answered by both. Where the graph does
///   answer at a symbol, it is because `attr_reader :count` filed a definition whose name span is
///   the symbol, which is the declaration this would find anyway.
/// - **No declarations of its own.** A symbol names a graph declaration, so it travels in the
///   [`Resolution`] beside this and uses the same rebase-and-place machinery as every other target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// The name, colon excluded: `authenticate`.
    pub name: String,
    /// The name's span, colon excluded, **in buffer coordinates**: it was read from the client's
    /// text.
    pub start: u32,
    pub end: u32,
}

/// Where a declaration is written down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// The graph's own spelling of the document URI.
    pub uri: String,
    /// The whole construct: `class Person ... end`.
    pub full: (u32, u32),
    /// Just the name, for the editor to highlight. Falls back to `full` for kinds rubydex records
    /// no name span for (constants, `attr_*`, aliases).
    pub selection: (u32, u32),
}

/// Which declarations a target names, and how sure the answer is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub declarations: Vec<DeclarationId>,
    /// `false` when the receiver's type was unknown and the candidates came from matching the
    /// method name alone. Callers must present such an answer as a guess.
    pub precise: bool,
    /// `true` when these declarations are not what the name declares but what the user meant by
    /// writing it: `Foo.new` answered with `Foo#initialize`.
    ///
    /// Navigation wants the redirect; `references` must not, because `def initialize` does not
    /// declare `new`, and listing it among `.new` call sites is noise.
    pub redirected: bool,
    /// What ya-lsp followed to type the receiver, if anything.
    ///
    /// Empty for everything from [`resolve`], which has no text and so cannot derive a type. Only
    /// [`resolve_typed`] fills it, and only for a call.
    pub derivation: Derivation,
    /// The class the receiver turned out to be, on a precise answer.
    ///
    /// - **`implementation` cannot rebuild this from the declarations.** `story.save` is declared
    ///   by `ActiveRecord::Persistence`, whose descendants are every model, so an override list
    ///   built from the declaration would answer a `Story` with `User#save`. The receiver is the
    ///   question's subject, and only the rung that named or derived it knows the class.
    /// - **Set on a precise answer only.** On the name-based list the receiver's class says nothing
    ///   a reader acts on. Here it is the class a `def` was actually found on, and what is below it
    ///   is a list of real overrides.
    /// - **`None` where no receiver was established** (a bare top-level call, a name reached
    ///   through a view context, a block whose `self` was inferred). Then the declaration's own
    ///   owner is the widest honest question.
    pub receiver: Option<DeclarationId>,
}

impl Resolution {
    fn precise(declarations: Vec<DeclarationId>) -> Self {
        Self {
            declarations,
            precise: true,
            redirected: false,
            derivation: Derivation::default(),
            receiver: None,
        }
    }

    /// One declaration ya-lsp typed itself, with what it followed to get there.
    ///
    /// `precise`, because the [`Derivation`] carries the tier, as `resolve_typed` does for a
    /// derived receiver.
    fn derived(declaration: DeclarationId, derivation: Derivation) -> Self {
        Self {
            declarations: vec![declaration],
            precise: true,
            redirected: false,
            derivation,
            receiver: None,
        }
    }

    fn redirected(declaration: DeclarationId) -> Self {
        Self {
            declarations: vec![declaration],
            precise: true,
            redirected: true,
            derivation: Derivation::default(),
            receiver: None,
        }
    }

    /// The same answer, told which class the receiver was.
    ///
    /// A builder, not a parameter on the three constructors: every caller that can fill it is one
    /// line of [`resolve_call`], and every other caller is elsewhere. See [`Self::receiver`].
    #[must_use]
    fn on(self, receiver: DeclarationId) -> Self {
        Self {
            receiver: Some(receiver),
            ..self
        }
    }
}

/// Everything the cursor could be pointing at, narrowed to the tightest span.
///
/// Several targets can share that span (`Person.new` records the `Person` reference and a synthetic
/// reference to its singleton over the same bytes), so this returns all of them and the caller
/// takes the first that resolves to a place.
#[must_use]
pub fn locate(graph: &Graph, uri_id: UriId, offset: u32) -> Vec<Located<'_>> {
    let Some(document) = graph.documents().get(&uri_id) else {
        return Vec::new();
    };
    let found = targets(graph, document)
        .filter(|(_, span)| covers(span, offset))
        .filter_map(|(order, _)| located(graph, document, order))
        .collect();
    narrowest(found, offset)
}

/// [`locate`], answered from the graph's index of a large document's spans ([`Spans`]).
///
/// **For a caller that asks one document many times.** A body read asks where each constant in a
/// library's method is, and walking every span of a large file for each one was a tenth of
/// a large app's hover time. The same targets in the same order as [`locate`]'s walk, which a
/// smaller document still takes.
#[must_use]
pub fn locate_held(graph: &Indexed, uri_id: UriId, offset: u32) -> Vec<Located<'_>> {
    let Some(document) = graph.documents().get(&uri_id) else {
        return Vec::new();
    };
    if !Spans::worth_holding(document) {
        return locate(graph, uri_id, offset);
    }
    let found = graph
        .spans(uri_id, || Spans::of(graph, document))
        .covering(offset)
        .into_iter()
        .filter_map(|order| located(graph, document, order))
        .collect();
    narrowest(found, offset)
}

/// Which of a document's targets a span is: its kind in the top two bits, in the order [`locate`]
/// lists ties (a constant, a call, a definition), and its place in the document's list of that
/// kind below them. So sorting orders is listing the targets as the document does.
type Order = u32;

const CONSTANT: Order = 0;
const CALL: Order = 1 << 30;
const DEFINITION: Order = 2 << 30;
/// The bits that hold the place. A list longer than this is past any file rubydex can hold, and
/// its tail is not located.
const PLACE: Order = CALL - 1;

/// Every span [`locate`] can land in, with its [`Order`]: each constant reference, each call and
/// each definition's name.
fn targets<'g>(
    graph: &'g Graph,
    document: &'g Document,
) -> impl Iterator<Item = (Order, &'g Offset)> + 'g {
    // A document lists ids the graph itself filed, so a miss is just a lookup yielding nothing, and
    // the place still counts it: [`located`] finds the same id there.
    fn placed<'g, I>(
        ids: &'g [I],
        kind: Order,
        span: impl Fn(&I) -> Option<&'g Offset> + 'g,
    ) -> impl Iterator<Item = (Order, &'g Offset)> + 'g {
        ids.iter().enumerate().filter_map(move |(place, id)| {
            let place = u32::try_from(place).ok().filter(|place| *place <= PLACE)?;
            Some((kind | place, span(id)?))
        })
    }
    placed(document.constant_references(), CONSTANT, |id| {
        Some(graph.constant_references().get(id)?.offset())
    })
    .chain(placed(document.method_references(), CALL, |id| {
        Some(graph.method_references().get(id)?.offset())
    }))
    .chain(placed(document.definitions(), DEFINITION, |id| {
        Some(name_span(graph.definitions().get(id)?))
    }))
}

/// A definition's span for the cursor: its name, not its body. Otherwise every click inside a
/// method body would be "on" the method, and hover would fire over whitespace.
fn name_span(definition: &Definition) -> &Offset {
    definition
        .name_offset()
        .unwrap_or_else(|| definition.offset())
}

/// The target `order` names in `document` ([`targets`]). `None` for a constant reference rubydex
/// made up ([`is_synthetic`]).
fn located<'g>(graph: &'g Graph, document: &'g Document, order: Order) -> Option<Located<'g>> {
    let place = usize::try_from(order & PLACE).ok()?;
    match order & !PLACE {
        CONSTANT => {
            let reference = graph
                .constant_references()
                .get(document.constant_references().get(place)?)?;
            (!is_synthetic(graph, reference))
                .then(|| at(reference.offset(), Target::Constant(reference)))
        }
        CALL => {
            let reference = graph
                .method_references()
                .get(document.method_references().get(place)?)?;
            Some(at(reference.offset(), Target::Call(reference)))
        }
        _ => {
            let definition = graph
                .definitions()
                .get(document.definitions().get(place)?)?;
            Some(at(name_span(definition), Target::Definition(definition)))
        }
    }
}

/// The targets covering the cursor that [`locate`] answers: the narrowest, after the tier below.
fn narrowest(mut found: Vec<Located<'_>>, offset: u32) -> Vec<Located<'_>> {
    // **A span that begins at the cursor outranks every span that does not**, before width is
    // considered. `covers` is end-inclusive, so a span that merely *ends* at the cursor is a
    // candidate too, on purpose; this tier keeps that from costing an answer.
    //
    // - **Why.** `!display_social_login?` records a call to `!` over the bang and a call to the
    //   method over the name, adjacent, not nested. With the cursor on the `d`, width alone picks
    //   the bang (one byte against twenty-one) and answers `BasicObject#!`. The method's span
    //   starts at the cursor; the bang's does not.
    // - **Why *starts at*, not *contains*.** Containment is ordinary nesting, which width already
    //   sorts. `Rails.env.development?` records a method reference over the whole expression and
    //   one per message; at the `.` after `Rails`, the wide call contains the cursor and the
    //   `Rails` constant only ends there. The constant is right (it types the receiver). Nothing
    //   begins at that byte, so width decides.
    // - **It keeps `a.b += c` working**: rubydex records that call over the `.`, one byte before
    //   the message, so with the cursor on `b` nothing begins there either.

    if found.iter().any(|located| located.begins_at(offset)) {
        found.retain(|located| located.begins_at(offset));
    }

    let Some(narrowest) = found.iter().map(Located::width).min() else {
        return Vec::new();
    };
    found.retain(|located| located.width() == narrowest);
    found
}

/// A large document's spans, sorted by where each starts, so [`locate_held`] finds the ones
/// covering the cursor by a binary search and a short walk back, not a walk of the file. Held with
/// the graph ([`Indexed::spans`]).
pub struct Spans {
    /// `(start, end, order)`, sorted.
    spans: Vec<(u32, u32, Order)>,
    /// The furthest end among `spans[..=i]`: where it falls short of the cursor, no span at or
    /// before `i` reaches it, and the walk back stops.
    reach: Vec<u32>,
}

impl Spans {
    /// A document with fewer targets is walked, not indexed: the walk is quick, and an index of
    /// every small file a body read touches would be memory held for nothing.
    const FROM: usize = 512;

    fn worth_holding(document: &Document) -> bool {
        document.constant_references().len()
            + document.method_references().len()
            + document.definitions().len()
            >= Self::FROM
    }

    fn of(graph: &Graph, document: &Document) -> Self {
        let mut spans: Vec<(u32, u32, Order)> = targets(graph, document)
            // A synthetic constant is left out here, not only by [`located`]: it spans a whole
            // call, and one wide span near the top would make every walk back reach the top.
            .filter(|(order, _)| located(graph, document, *order).is_some())
            .map(|(order, span)| (span.start(), span.end(), order))
            .collect();
        spans.sort_unstable();
        let reach = spans
            .iter()
            .scan(0, |furthest, (_, end, _)| {
                *furthest = (*furthest).max(*end);
                Some(*furthest)
            })
            .collect();
        Self { spans, reach }
    }

    /// Every span covering `offset` ([`covers`]), sorted by [`Order`].
    fn covering(&self, offset: u32) -> Vec<Order> {
        let upto = self.spans.partition_point(|(start, _, _)| *start <= offset);
        let mut found: Vec<Order> = (0..upto)
            .rev()
            .take_while(|index| self.reach[*index] >= offset)
            .filter(|index| offset <= self.spans[*index].1)
            .map(|index| self.spans[index].2)
            .collect();
        found.sort_unstable();
        found
    }
}

/// [`locate`] at graph offset `at`, plus the call rubydex files away from its name.
///
/// An operator write through a call files `b` elsewhere: `a.b ||= c` and
/// `a.b &&= c` on the operator, where a cursor on `b` finds nothing, and `a.b += c` on the `.`,
/// which `covers`' end-inclusive rule reaches from the first byte of `b` only. There, the call
/// filed away is answered, placed on the message: where the editor underlines, and where
/// [`resolve_typed`] reads the receiver from.
///
/// - **Only where the graph holds nothing better at the cursor**: nothing at all, or only a call of
///   one or two bytes ending there (the `.`, `&.` or `::` an operator write files on). The parse is
///   paid only there, and nothing rubydex placed on the message is second-guessed.
/// - **Only the reference the message reads.** rubydex files `b=` on the same span, and that is the
///   writer, not the method under the cursor.
/// - `offset` is the buffer's, where the parse reads; the spans answered are the graph's, as
///   [`locate`]'s are.
#[must_use]
pub fn locate_written<'g>(
    graph: &'g Graph,
    uri_id: UriId,
    at: u32,
    text: &cursor::Parsed<'_>,
    offset: u32,
    rebase: &Rebase,
) -> Vec<Located<'g>> {
    let found = locate(graph, uri_id, at);
    let behind = |located: &Located<'_>| {
        matches!(located.target, Target::Call(_)) && located.end == at && located.width() <= 2
    };
    if !found.iter().all(behind) {
        return found;
    }
    let Some(cursor::Misplaced::Parked {
        message,
        operator,
        name,
    }) = cursor::misplaced(text, offset)
    else {
        return found;
    };
    let (Some(operator), Some(start), Some(end)) = (
        rebase.to_graph(operator.0),
        rebase.to_graph(message.0),
        rebase.to_graph(message.1),
    ) else {
        return found;
    };
    let moved: Vec<Located<'g>> = locate(graph, uri_id, operator)
        .into_iter()
        .filter(|located| {
            located.start == operator
                && matches!(located.target, Target::Call(reference)
                    if member_name(graph, *reference.str())
                        .is_some_and(|member| member.strip_suffix("()") == Some(name.as_str())))
        })
        .map(|located| Located {
            start,
            end,
            ..located
        })
        .collect();
    if moved.is_empty() { found } else { moved }
}

/// A call rubydex recorded nothing for, answered from the parse: the
/// message's buffer span, and what [`resolve_typed`] would answer had the reference existed.
///
/// - **Only a call inside a constant path's parent** ([`cursor::Misplaced::Unrecorded`]), asked
///   where [`locate`] found nothing, so nothing rubydex recorded is second-guessed.
/// - **The two rungs every call with a written receiver gets**: the typed receiver's member
///   ([`on_a_typed_receiver`]), else the name rung. A call with no receiver written gets the name
///   rung alone, because the view context and the closure rung read a reference this call lacks.
/// - **Fenced as [`resolve_typed`] fences** an imprecise answer.
#[must_use]
pub fn resolve_misplaced(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    offset: u32,
    rebase: &Rebase,
) -> Option<((u32, u32), Resolution)> {
    let cursor::Misplaced::Unrecorded { message, name } = cursor::misplaced(text, offset)? else {
        return None;
    };
    let graph = sources.graph;
    let scope_at = rebase.to_graph(message.0)?;
    let context = cursor::at(text, message.0)?.context.rebased(rebase)?;
    let privacy = Privacy::at(&context, &sources.memo.modifiers);
    // A method declaration's own spelling, which [`member_name`] hands the rungs everywhere else.
    let member = format!("{name}()");
    let named = by_name(graph, &member, ClassObject::No, privacy);
    let resolution = match &context {
        Context::MethodCall { receiver } => {
            on_a_typed_receiver(sources, uri_id, receiver, &member, scope_at, privacy, named)
        }
        _ => named,
    };
    if resolution.precise {
        return Some((message, resolution));
    }
    let fence = environment::Fence::at(uri_of(graph, uri_id), sources.layout);
    Some((message, loadable_from(graph, fence, resolution)))
}

/// The instance variable at `offset`, or `None` when the cursor is on something else.
///
/// **Asked before [`locate`], necessarily.** `highlight::find` asks the scope walk first too: the
/// walk can say *no*, and its `None` lets a constant or a call fall through to the graph.
/// `@name = 1` is the one span both could answer, and the graph's answer is the single write, none
/// of the reads. Two requests disagreeing on one span is the bug; one rule asked in one order is
/// the fix.
///
/// `rebound` places a loose occurrence ([`occurrences_at`]).
#[must_use]
pub fn variable_at(
    text: &cursor::Parsed<'_>,
    offset: u32,
    rebound: &dyn Fn(u32) -> Option<i32>,
) -> Option<Variable> {
    let (name, at, occurrences) = occurrences_at(text, offset, rebound)?;
    // A local is the other thing the walk handles, and not this question: `person = Person.new` is
    // visible from where the reader stands, and the graph does not pretend otherwise. A class
    // variable never arrives: `scopes` models none.
    if !name.starts_with('@') {
        return None;
    }
    Some(Variable {
        start: at.start,
        end: at.end,
        writes: occurrences
            .iter()
            .filter(|occurrence| occurrence.write)
            .map(|occurrence| (occurrence.start, occurrence.end))
            .collect(),
    })
}

/// The variable under `offset`, the occurrence the cursor is on, and every occurrence of it:
/// [`scopes::variable`], with each loose
/// instance-variable occurrence ([`scopes::Placed::loose`]) at the level `rebound` says its block
/// runs at, else where the walk put it.
///
/// `after_action …, if: -> { @payload }` runs on an instance, so its `@payload` is the one
/// `def load` writes, not the class object's. One grouping for `definition`, `hover` and
/// `documentHighlight`, so a jump never lands on a span the highlight leaves dark. `rebound` takes
/// buffer offsets ([`rebinding`]).
#[must_use]
pub fn occurrences_at(
    text: &cursor::Parsed<'_>,
    offset: u32,
    rebound: &dyn Fn(u32) -> Option<i32>,
) -> Option<(String, scopes::Occurrence, Vec<scopes::Occurrence>)> {
    let Some((name, cursor, family)) = scopes::instance_family(text, offset) else {
        return scopes::variable(text, offset);
    };
    let under = family[cursor].occurrence.clone();
    let levels: Vec<i32> = family
        .iter()
        .map(|placed| {
            if placed.loose {
                rebound(placed.occurrence.start).unwrap_or(placed.level)
            } else {
                placed.level
            }
        })
        .collect();
    let level = levels[cursor];
    let occurrences = family
        .into_iter()
        .zip(levels)
        .filter(|(_, placed)| *placed == level)
        .map(|(placed, _)| placed.occurrence)
        .collect();
    Some((name, under, occurrences))
}

/// [`types::rebound_level`] at a buffer offset, for [`occurrences_at`]: `None` where the offset
/// is in text the graph has not seen.
pub fn rebinding<'a>(
    sources: &'a types::Sources<'_>,
    uri_id: UriId,
    rebase: &'a Rebase,
) -> impl Fn(u32) -> Option<i32> + 'a {
    move |at| types::rebound_level(sources, uri_id, rebase.to_graph(at)?)
}

/// The variable at the cursor and what it is: the card's half of the same answer.
///
/// [`resolve_typed`] is this for a call. Separate functions, because a variable is not a call and
/// has no receiver; same shape, because hover and go-to-definition read one answer and must not
/// disagree.
///
/// - **`scope_at` is `offset` in graph coordinates.** The only graph read here is the nesting a
///   guessed constant is resolved in, and the graph is keyed by its own text. Everything else uses
///   the buffer.
/// - **`rebase` carries the receiver across, and is required.** The assignment typing `@story` is
///   read from the buffer and yields a [`Receiver`](cursor::Receiver) whose offsets are graph
///   *keys*. Without translation, an unindexed keystroke above the cursor makes `Story` a constant
///   one byte off: a name-guessed card normally, the wrong class when unlucky. `None` where the
///   receiver is in text the graph has never seen (see [`resolve_typed`]).
/// - **The [`Resolution`] is always `precise`**, and its [`Derivation`] says otherwise where
///   needed: an assignment names its line and the name rung names itself, as for `@story.title`.
#[must_use]
pub fn resolve_variable(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    offset: u32,
    scope_at: u32,
    rebase: &Rebase,
) -> Option<(Variable, Resolution)> {
    let (variable, typed) = variable_type(sources, uri_id, text, offset, scope_at, rebase)?;
    // A union has no single declaration to jump to, so the variable is left unanswered rather than
    // answered with half of it (`types::Typed::one`).
    let declaration = typed.one()?;
    Some((variable, Resolution::derived(declaration, typed.derivation)))
}

/// The variable at the cursor and what it holds, a union included: the card's half of
/// [`resolve_variable`], which a jump needs one class of. Arguments as there.
#[must_use]
pub fn variable_type(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    offset: u32,
    scope_at: u32,
    rebase: &Rebase,
) -> Option<(Variable, types::Typed)> {
    let variable = variable_at(text, offset, &rebinding(sources, uri_id, rebase))?;
    let receiver =
        cursor::instance_variable(text.source(), (variable.start, variable.end)).rebased(rebase)?;
    let scope = types::Scope::at(sources.graph, uri_id, scope_at);
    let typed = types::method_receiver(sources, uri_id, &receiver, &scope)?;
    Some((variable, typed))
}

/// A read of an instance variable, and the graph's declaration of it: for the card a read gets
/// when [`resolve_variable`] typed nothing.
///
/// A read is a span rubydex files nothing under, so [`locate`] finds nothing there, and the card
/// was silent while `definition` jumped to the writes. Two places hold the declaration:
///
/// - **A write in this file**: its span is a `Definition` the graph holds, so the
///   read gets the card a cursor on that write gets. Every write the walk returns is one variable
///   of one class, so the first names it.
/// - **A write in another file of the object's classes**: the writes the type side
///   folds for a read ([`types::instance_writes`]), whose first owner's variable is declared.
///
/// Reads only: a cursor on a write finds that write in [`locate`] already.
#[must_use]
pub fn written_variable(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    offset: u32,
    rebase: &Rebase,
) -> Option<(Variable, Resolution)> {
    let variable = variable_at(text, offset, &rebinding(sources, uri_id, rebase))?;
    if variable.writes.contains(&(variable.start, variable.end)) {
        return None;
    }
    let resolution = declared_variable(sources, uri_id, &variable, rebase)?;
    Some((variable, resolution))
}

/// The graph's declaration of the variable at the cursor, read or write: the card names it
/// (`Shelf::Book#@title`) beside what it holds ([`variable_type`]).
#[must_use]
pub fn variable_declaration(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    variable: &Variable,
    rebase: &Rebase,
) -> Option<DeclarationId> {
    declared_variable(sources, uri_id, variable, rebase)?
        .declarations
        .first()
        .copied()
}

/// Where a variable is declared: its first write in this file, else the declaration the writes
/// the type side folds for a read name ([`types::instance_writes`]).
fn declared_variable(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    variable: &Variable,
    rebase: &Rebase,
) -> Option<Resolution> {
    let graph = sources.graph;
    let Some(&(write, _)) = variable.writes.first() else {
        let declaration = types::instance_writes(sources, uri_id, variable.start).declaration?;
        return Some(Resolution::precise(vec![declaration]));
    };
    let located = locate(graph, uri_id, rebase.to_graph(write)?)
        .into_iter()
        .find(|located| {
            matches!(
                located.target,
                Target::Definition(Definition::InstanceVariable(_))
            )
        })?;
    let fence = environment::Fence::at(uri_of(graph, uri_id), sources.layout);
    Some(resolve(graph, &located, fence))
}

/// The **type** of whatever the cursor is on, as a declaration to jump to.
///
/// [`resolve_typed`] answers *where is this method declared*; this answers *what class is this
/// value*. Both end at [`types::method_receiver`], so the class `typeDefinition` jumps to is the
/// class the card names and `completion` offers.
///
/// - **The instance variable is asked first**, which is why this is not simply [`cursor::type_of`].
///   `@story` is what the graph models wrongly, so every request asks the scope walk first
///   (`navigation.md`; [`resolve_variable`] is the walk). Asking `cursor::type_of` first would
///   classify `@story` twice, possibly differently.
/// - **A union is no answer**, as at a variable: `Typed::one` refuses it, so a value that is a
///   `String` on one branch and an `Integer` on another sends the reader nowhere.
/// - **The tier is carried, not gated here.** Which tiers a *jump* may use is the request's rule,
///   as with [`resolve_typed`], whose guessed answers `hover` draws and `implementation` refuses.
/// - **`scope_at` and `rebase`** work as in [`resolve_variable`].
#[must_use]
pub fn type_of(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    offset: u32,
    scope_at: u32,
    rebase: &Rebase,
) -> Option<((u32, u32), Resolution)> {
    if let Some((variable, resolution)) =
        resolve_variable(sources, uri_id, text, offset, scope_at, rebase)
    {
        return Some(((variable.start, variable.end), resolution));
    }
    let (start, end, receiver) = cursor::type_of(text, offset)?;
    let receiver = receiver.rebased(rebase)?;
    let scope = types::Scope::at(sources.graph, uri_id, scope_at);
    let typed = types::method_receiver(sources, uri_id, &receiver, &scope)?;
    Some(((start, end), class_of(sources.graph, &typed)?))
}

/// What the call under the cursor returns, for a card whose method declares nothing.
///
/// - **Only a call**, and only with the cursor in its message name ([`cursor::type_of`]'s rule):
///   the card is that method's.
/// - **Never a guess.** A type read off a name is not an answer about this call.
#[must_use]
pub fn call_type(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    offset: u32,
    scope_at: u32,
    rebase: &Rebase,
) -> Option<types::Typed> {
    let (_, _, receiver) = cursor::type_of(text, offset)?;
    if !matches!(
        receiver,
        cursor::Receiver::Returned { .. } | cursor::Receiver::Spelled { .. }
    ) {
        return None;
    }
    let receiver = receiver.rebased(rebase)?;
    let scope = types::Scope::at(sources.graph, uri_id, scope_at);
    types::method_receiver(sources, uri_id, &receiver, &scope)
        .filter(|typed| typed.derivation.tier() != types::Tier::Guessed)
}

/// A bare name under the cursor in a partial that its render calls pass as a local:
/// the name's span, what it holds, and where each call passes it
/// ([`types::partial_local_of`]).
pub fn partial_local(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    offset: u32,
) -> Option<PartialLocal> {
    let (start, end, receiver) = cursor::type_of(text, offset)?;
    let receiver = match receiver {
        cursor::Receiver::Spelled { was, .. } => *was,
        other => other,
    };
    let cursor::Receiver::Returned {
        on,
        method,
        arity: cursor::Arity::Exactly(0),
        block: cursor::Block::None,
        safe: false,
        ..
    } = receiver
    else {
        return None;
    };
    if !matches!(*on, cursor::Receiver::SelfObject(_)) {
        return None;
    }
    let (typed, places) = types::partial_local_of(sources, uri_id, &method)?;
    Some(PartialLocal {
        span: (start, end),
        typed,
        places,
    })
}

/// [`partial_local`]'s answer.
pub struct PartialLocal {
    /// The name, in the cursor's text.
    pub span: (u32, u32),
    pub typed: types::Typed,
    /// Each document passing it, with the spans of the values in that document's text.
    pub places: types::Places,
}

/// The declaration a type sends a reader to, which for a class object is not the type itself.
///
/// - **A singleton class has no place of its own.** `Story` as a receiver is `Story::<Story>`:
///   exact, and what `completion` lists. rubydex files a definition for it only where a file wrote
///   `class << self`, so jumping to the type would land nowhere for most constants. The reader
///   wants the class it hangs off, `class Story`. [`attached_class`] is the same hop
///   [`constructor`] takes: the singleton's owner, never a name taken apart.
/// - **Applied here, not in [`types`]**, because it is a rule about places, not types: the card
///   still says class object and `completion` still offers the singleton's members. Only the jump
///   needs somewhere to land.
fn class_of(graph: &Graph, typed: &types::Typed) -> Option<Resolution> {
    // A union has no single declaration to jump to, so the cursor is left unanswered
    // (`types::Typed::one`, as for a variable).
    let declaration = typed.one()?;
    let declaration = attached_class(graph, declaration).unwrap_or(declaration);
    Some(Resolution::derived(declaration, typed.derivation.clone()))
}

/// What a macro's `:symbol` argument names, as a member of the class the macro is written in.
///
/// - **One rule for every macro.** A macro's symbol is a member of the class the macro sits in: its
///   own, an ancestor's, or one a generator wrote. `before_action :authenticate` is a `def` here or
///   above; `validates :title` is the column `db/schema.rb` declared; `belongs_to :user` is the
///   reader `workspace/rails/models.rs` wrote. So there is no macro table, just the ancestor walk a
///   typed call takes, and an unknown DSL works unchanged.
/// - **Instance side first, then the class object.** `scope :recent` declares a singleton method
///   and most macros name instance ones, so the order is frequency. Both are asked because choosing
///   would need the table this avoids.
/// - **Derived, never resolved.** The member is found exactly, but that the symbol *is* a member is
///   a convention: only `before_action` says `:authenticate` is something to call. So the answer
///   carries [`Derivation::named_by`] and the card names the macro.
/// - **An undeclared name answers `None`, not a name-matched guess.** `:destroy`, `:draft` and
///   `:desc` are values, and a jump from one into some `def destroy` is a wrong answer the reader
///   cannot spot.
/// - **`scope_at` is `offset` in graph coordinates**, as in [`resolve_variable`]: the nesting comes
///   from the graph, the symbol from the buffer.
#[must_use]
pub fn resolve_symbol(
    graph: &Graph,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    offset: u32,
    scope_at: u32,
) -> Option<(Symbol, Resolution)> {
    let symbol = cursor::macro_symbol(text, offset)?;
    let scope = types::Scope::at(graph, uri_id, scope_at);
    // rubydex keys a method member by name plus `()`, as `member_name` gives every other caller. A
    // symbol arrives as the bare word.
    let member = format!("{}()", symbol.name);
    let found = |owner| member_of(graph, owner, &member).filter(|id| !ruby_s_own(graph, *id));
    let declaration = scope
        .nesting_id(graph)
        .and_then(found)
        .or_else(|| scope.caller(graph).and_then(found))?;
    Some((
        Symbol {
            name: symbol.name,
            start: symbol.start,
            end: symbol.end,
        },
        Resolution::derived(
            declaration,
            Derivation {
                named_by: Some(symbol.macro_name),
                ..Derivation::default()
            },
        ),
    ))
}

/// What a symbol names where it is the first argument of a member that takes a method's name:
/// `widget.send(:shout)`, `method(:shout)`, `try(:title)`, `Widget.instance_method(:shout)`.
///
/// - **The call is resolved first, as a jump from its own name would be** ([`resolve_typed`]), and
///   only a precise answer that is not a guess counts: the member it reaches decides whether the
///   symbol is a name at all ([`types::named_by_symbol`]), and the class it was sent to is where
///   the name is looked up. So a class's own `send` names nothing, and a `send(:x)` straight in a
///   class body names the class object's `x`, where [`resolve_symbol`] would ask the instance first.
/// - **The named method is looked up as a call to it would be**, privacy included: `public_send`
///   and `try` reach no private method.
/// - **A symbol only**: a name only running Ruby spells answers nothing.
/// - **The tier is the call's.** Which method the symbol names is Ruby's rule, not a convention, so
///   nothing is added to what finding the receiver rested on.
#[must_use]
pub fn resolve_named(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    offset: u32,
    rebase: &Rebase,
) -> Option<(Symbol, Resolution)> {
    let graph = sources.graph;
    let symbol = cursor::named_symbol(text, offset)?;
    let message = rebase.to_graph(symbol.message)?;
    let call = locate(graph, uri_id, message)
        .into_iter()
        .filter(|located| matches!(located.target, Target::Call(_)))
        .find_map(|located| {
            let start = rebase
                .span_to_buffer(ByteSpan {
                    start: located.start,
                    end: located.end,
                })?
                .start;
            resolve_typed(sources, uri_id, text, &located, start, rebase)
        })?;
    if !call.precise || call.derivation.tier() == Tier::Guessed {
        return None;
    }
    let ([found], Some(on)) = (call.declarations.as_slice(), call.receiver) else {
        return None;
    };
    let named = types::named_by_symbol(sources, uri_id, *found, on, &symbol.name)?;
    Some((
        Symbol {
            name: symbol.name,
            start: symbol.start,
            end: symbol.end,
        },
        Resolution {
            declarations: vec![named],
            precise: true,
            redirected: false,
            receiver: graph
                .declarations()
                .get(&named)
                .map(|declaration| *declaration.owner_id()),
            derivation: call.derivation,
        },
    ))
}

/// An instance variable a symbol under the cursor names ([`types::NamedVariable`]), and the
/// symbol's span.
///
/// - **The first argument of `instance_variable_get` or `instance_variable_defined?`**:
///   `client.instance_variable_get(:@base_uri)`. The call is resolved as a jump from its own name
///   would be ([`resolve_named`]'s way), a guess refuses, and the variable is its receiver's.
/// - **A macro's `:@name`** ([`cursor::macro_variable`]): `delegate :render, to: :@template`,
///   Forwardable's `def_delegators :@items, :size`. What a macro makes runs on the objects of the
///   namespace it is written in, so the variable is their instances'.
/// - **A line test first**: this is asked at every cursor the variable walk declines, and only a
///   line holding `:@` can answer, so the others pay no parse.
#[must_use]
pub fn named_variable(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    offset: u32,
    rebase: &Rebase,
) -> Option<(Symbol, types::NamedVariable)> {
    let graph = sources.graph;
    let at = (offset as usize).min(text.source().len());
    let line = text.source()[..at].rfind('\n').map_or(0, |start| start + 1)
        ..text.source()[at..]
            .find('\n')
            .map_or(text.source().len(), |end| at + end);
    if !text.source().get(line)?.contains(":@") {
        return None;
    }
    if let Some(symbol) = cursor::named_symbol(text, offset)
        && symbol.name.starts_with('@')
    {
        let message = rebase.to_graph(symbol.message)?;
        let call = locate(graph, uri_id, message)
            .into_iter()
            .filter(|located| matches!(located.target, Target::Call(_)))
            .find_map(|located| {
                let start = rebase
                    .span_to_buffer(ByteSpan {
                        start: located.start,
                        end: located.end,
                    })?
                    .start;
                resolve_typed(sources, uri_id, text, &located, start, rebase)
            })?;
        if !call.precise || call.derivation.tier() == Tier::Guessed {
            return None;
        }
        let ([found], Some(on)) = (call.declarations.as_slice(), call.receiver) else {
            return None;
        };
        if !types::reads_a_variable(graph, *found) || symbol.name.starts_with("@@") {
            return None;
        }
        let (object, class_side) = types::object_of(graph, on)?;
        return Some((
            Symbol {
                name: symbol.name.clone(),
                start: symbol.start,
                end: symbol.end,
            },
            types::NamedVariable {
                name: symbol.name,
                object,
                class_side,
            },
        ));
    }
    let symbol = cursor::macro_variable(text, offset)?;
    let object = types::namespace_at(sources, uri_id, rebase.to_graph(symbol.start)?)?;
    Some((
        Symbol {
            name: symbol.name.clone(),
            start: symbol.start,
            end: symbol.end,
        },
        types::NamedVariable {
            name: symbol.name,
            object,
            class_side: false,
        },
    ))
}

/// The literal key under the cursor, where the call it is passed to is a member that looks a key
/// up ([`generated::KEYED`](crate::generated::KEYED)): the literal, and the member's declaration
/// name. The call is resolved as a jump from its own name would be, and a guess refuses: the
/// member decides whether the literal is a key at all.
#[must_use]
pub fn resolve_keyed(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    offset: u32,
    rebase: &Rebase,
) -> Option<(cursor::KeyedLiteral, String)> {
    let graph = sources.graph;
    let literal = cursor::keyed_literal(text, offset)?;
    let message = rebase.to_graph(literal.message)?;
    let call = locate(graph, uri_id, message)
        .into_iter()
        .filter(|located| matches!(located.target, Target::Call(_)))
        .find_map(|located| {
            let start = rebase
                .span_to_buffer(ByteSpan {
                    start: located.start,
                    end: located.end,
                })?
                .start;
            resolve_typed(sources, uri_id, text, &located, start, rebase)
        })?;
    if call.derivation.tier() == Tier::Guessed {
        return None;
    }
    let [found] = call.declarations.as_slice() else {
        return None;
    };
    if !types::looks_up_a_key(sources.types, *found) {
        return None;
    }
    let member = graph.declarations().get(found)?.name().to_owned();
    Some((literal, member))
}

/// What a symbol argument under the cursor names: a method-name argument first ([`resolve_named`]),
/// then a macro's ([`resolve_symbol`]). `offset` is in the buffer, `at` the same place in the graph.
#[must_use]
pub fn symbol_at(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    offset: u32,
    at: u32,
    rebase: &Rebase,
) -> Option<(Symbol, Resolution)> {
    resolve_named(sources, uri_id, text, offset, rebase)
        .or_else(|| resolve_symbol(sources.graph, uri_id, text, offset, at))
}

/// The declarations a target names, with receiver types this crate derives and a template's view
/// context.
///
/// The entry point for callers with the document's text: `hover` and `definition`. Over [`resolve`]
/// it adds two rungs between "rubydex named the receiver" and "matched on the name alone": a
/// receiver ya-lsp typed itself (from a signature or assignment), and a **bare** name in a
/// template, which has no receiver at all. The answer's [`Derivation`] tells the card which.
///
/// - **The two rungs are told apart by syntax and never both asked.** A call with a written
///   receiver is not implicit, so `cursor::at` runs once and its answer dispatches. Asking twice
///   would parse the file twice.
/// - **Callers without text** (`references`, the type hierarchy) use [`resolve`] and never follow a
///   derived type. A work list is places to *edit*, and a derived receiver is the one thing in it
///   that could be wrong.
/// - **One fence on the name rung.** An imprecise answer (the name-based list) is filtered through
///   [`loadable_from`] on the way out. Last, not inside any rung, because it is about the *target*,
///   not how it was found, and a filter applied in one of two places has a hole.
/// - **`None` is a refusal, not an absence.** The receiver is parsed from the buffer and looked up
///   in the graph, which differ between a keystroke and its settle. `rebase` translates; where it
///   cannot (the receiver *is* being typed), this answers `None`, the request declines, and
///   `Analysis::serve` settles and asks again, as `completion::complete` does. Falling through to
///   the name list instead would look settled to the caller, never be retried, and the deferred
///   path would quietly answer less than the eager one.
#[must_use]
pub fn resolve_typed(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    located: &Located<'_>,
    at_in_source: u32,
    rebase: &Rebase,
) -> Option<Resolution> {
    let resolution = typed(sources, uri_id, text, located, at_in_source, rebase)?;
    if resolution.precise {
        return Some(resolution);
    }
    Some(loadable_from(
        sources.graph,
        environment::Fence::at(uri_of(sources.graph, uri_id), sources.layout),
        resolution,
    ))
}

/// Everything [`resolve_typed`] answers, before the fence.
fn typed(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    located: &Located<'_>,
    at_in_source: u32,
    rebase: &Rebase,
) -> Option<Resolution> {
    let graph = sources.graph;
    // **The gate goes into `resolve_call` because one precise answer needs fencing and is decided
    // there.** See its root arm: a member found on `Object` is found on every receiver, so a `def`
    // only the suite declares would answer for the whole workspace.
    //
    // Built before the target is matched, because [`resolve`] takes one too: a constant or a `def`
    // is precise and no tree rule touches it; its one narrowing is a document outside the project.
    let fence = environment::Fence::at(uri_of(graph, uri_id), sources.layout);
    // **A `def` rubydex files under another method is the one it really defines**
    // ([`types::own_def_member`]): an RSpec group's helper, not the `Object` method every spec's
    // helper of that name became.
    if let Target::Definition(definition @ Definition::Method(_)) = located.target
        && let Some(name) = definition.name_offset()
        && let Some(member) = types::own_def_member(
            sources,
            uri_of(graph, uri_id).unwrap_or_default(),
            (name.start(), name.end()),
        )
        && let Some(declaration) = graph.declarations().get(&member)
    {
        return Some(Resolution::precise(vec![member]).on(*declaration.owner_id()));
    }
    let Target::Call(reference) = located.target else {
        return Some(resolve(graph, located, fence));
    };
    // The request's memo: the root arm asks it *during* the walk, not of the answer. Empty until
    // something lands on a root, so jumps that do not pay nothing.
    let blocks = &sources.memo.blocks;
    let resolution = resolve_call(graph, reference, fence, Privacy::Allowed, Some(blocks));
    // **A receiverless call in a block a signature rebinds is looked up on what the signature says
    // `self` is**, before rubydex's answer, which reads `self` from the body the block is written
    // in. `before_save do update(…) end` calls the record's `update`, never the class's.
    if let Some(rebound) = rebound_call(sources, uri_id, text, located, at_in_source, reference) {
        return Some(rebound);
    }
    // The request's memo. Empty until asked, so a resolved non-private jump pays nothing.
    let modifiers = &sources.memo.modifiers;
    // Only where rubydex could not name the receiver itself. A derived type is *worse* than a
    // resolved one and must never displace it.
    //
    // **The one precise answer that is not final: a private declaration.** Whether Ruby allows it
    // here is a syntax question (see [`Privacy`]), and the syntax is read below from the buffer
    // only this rung has. Asking eagerly would parse the file on every resolved jump and hover;
    // asking here costs one visibility lookup, and a parse only when the answer is private, which
    // is rare.
    let vetoable = resolution.precise && holds_private(graph, modifiers, &resolution);
    if resolution.precise && !vetoable {
        return Some(resolution);
    }
    let Some(member) = member_name(graph, *reference.str()) else {
        return Some(resolution);
    };
    // The same classification completion runs, asked of a finished call instead of a half-typed
    // one. `cursor::at` needs no special case: its test is that the cursor sits between the
    // operator and the end of the message, which a cursor *on* a method name does.
    //
    // - **Where it cannot be read, the answer above stands.** Both early returns hand back the
    //   resolution unchanged, private or not: the gate refuses only on facts it established, never
    //   on one it failed to establish.
    // - **`source` is the buffer and `located` is the graph's**, two different texts whenever a
    //   keystroke is unindexed. `Scope::at` below reads the graph, so it gets graph coordinates;
    //   `cursor::at` parses the buffer, so it gets buffer coordinates. Passing `located.start` to
    //   both would read the token left of the user's.
    let Some(cursor) = cursor::at(text, at_in_source) else {
        return Some(resolution);
    };
    // The context the buffer described must move into graph coordinates before any lookup.
    // `cursor::at` read `Story` at a buffer offset, and `types::method_receiver` passes that offset
    // to `locate`, which indexes the graph's text. The refusal is the point; see this function's
    // docs.
    let context = cursor.context.rebased(rebase)?;
    // What the syntax permits, read once and applied at all three rungs below.
    let privacy = Privacy::at(&context, modifiers);
    // The veto on the *resolved* rung: a re-resolve, not a filter on the result. The gate changes
    // **which rung answers**: refusing the ancestor hit sends the call on to the extend repair and
    // then the name rung, either of which may hold a public answer hidden behind the private one.
    // Striking the declaration from the finished `Resolution` would leave a precise answer with
    // nothing in it, which callers read as *has no such method*.
    if vetoable {
        return Some(match privacy {
            Privacy::Allowed => resolution,
            Privacy::Refused(modifiers) => resolve_call(
                graph,
                reference,
                fence,
                Privacy::Refused(modifiers),
                Some(blocks),
            ),
        });
    }
    // The name rung was drawn before the syntax was read (it is `resolve_call`'s answer when
    // nothing above answered), so the refusal is applied to it here. This is `by_name`'s last step,
    // over the list `by_name` produced; the two agree because both are outermost.
    let resolution = Resolution {
        declarations: privacy.keep(graph, resolution.declarations),
        ..resolution
    };
    // **The class-object narrowing, made certain where the syntax proves the class object.**
    // `resolve_call` could not tell a statement of the body from a bare word in a block, so it
    // narrowed without emptying; here a written `Settings::General.app_domain` drops another
    // class's instance method even when nothing else is left.
    let resolution = if proves_the_class_object(&context, cursor.in_a_closure) {
        on_a_proven_class_object(graph, reference, &member, privacy).unwrap_or(resolution)
    } else {
        resolution
    };
    // **A block whose `self` a generator said is not rubydex's receiver's**.
    // rubydex files a call in a concern's `included do` on the module's class object, which Ruby
    // never makes `self` there: Rails runs the block against each including class, and a callback
    // block inside it against a record. `workspace/rails/concerns.rs` names those classes
    // ([`Runs::Each`](crate::generated::Runs::Each)), or refuses where none is known, and
    // [`types::rebound_self`] answers with them. So neither the class-object narrowing nor the typed
    // rung on the module may read that object: each including class's own member where every one
    // has it, else the name rung whole with nothing said about the receiver. Only where `self` is
    // the receiver: `Countable.x` written out really means the module.
    if matches!(
        &context,
        Context::Expression
            | Context::Argument { .. }
            | Context::MethodCall {
                receiver: cursor::Receiver::SelfObject(_)
            }
    ) {
        match types::rebound_self(sources, uri_id, located.start) {
            Some(None) => return Some(by_name(graph, &member, ClassObject::No, privacy)),
            Some(Some(typed)) if typed.derivation.each.is_some() => {
                return Some(
                    on_each(sources, uri_id, &typed, &member)
                        .unwrap_or_else(|| by_name(graph, &member, ClassObject::No, privacy)),
                );
            }
            _ => {}
        }
    }
    Some(match &context {
        Context::MethodCall { receiver } => on_a_typed_receiver(
            sources,
            uri_id,
            receiver,
            &member,
            located.start,
            privacy,
            resolution,
        ),
        // No receiver written, which in a template means the view context. `Argument` is included
        // for `Context::allows_private`'s reason: `link_to "x", story_path(s)` writes `story_path`
        // with an implicit receiver, as a statement would.
        Context::Expression | Context::Argument { .. } => types::view_context(sources, uri_id)
            .and_then(|reachable| reachable.member(graph, &member))
            .map(|found| Resolution {
                declarations: vec![found.declaration],
                precise: true,
                redirected: false,
                derivation: Derivation {
                    view: Some(found.how),
                    ..Derivation::default()
                },
                // A view context is three half-scopes, not one class, and the member came from
                // whichever holds it. There is no receiver to be below; see `views`.
                receiver: None,
            })
            // Both cannot answer (a template has no class body for a block), so the order just
            // ranks them. A view context is a fact about how Rails loads the file; a closure's
            // `self` is an inference about what a DSL does with a block.
            .or_else(|| in_a_closure(graph, reference, cursor.in_a_closure, &member))
            .unwrap_or(resolution),
        Context::NamespaceAccess { .. } => resolution,
    })
}

/// The typed-receiver rung: the member `receiver`'s own type has, gated as the resolved rung is.
///
/// `resolution` is the name rung's answer, which stands where the receiver has no one type or lacks
/// the member, told which class it was. Shared by [`typed`] and [`resolve_misplaced`], so a call
/// rubydex never recorded is answered by the rung every other call is.
///
/// `scope_at` is a graph offset, for [`types::Scope::at`].
fn on_a_typed_receiver(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    receiver: &cursor::Receiver,
    member: &str,
    scope_at: u32,
    privacy: Privacy<'_>,
    resolution: Resolution,
) -> Resolution {
    let graph = sources.graph;
    let scope = types::Scope::at(graph, uri_id, scope_at);
    // The second half covers a union: the call runs on each class that has the member
    // (`types::narrowed_classes`), each answering from its own declaration, or else there is no
    // class to look it up on, and it reads like an untyped receiver.
    let Some((typed, classes)) = types::method_receiver(sources, uri_id, receiver, &scope)
        .and_then(|typed| {
            let classes = match typed.one() {
                Some(one) => vec![one],
                None => types::narrowed_classes(sources, uri_id, &typed, member)?,
            };
            Some((typed, classes))
        })
    else {
        return resolution;
    };
    let mut declarations = Vec::new();
    for on in classes {
        match types::member_of(sources, uri_id, on, StringId::from(member)) {
            // **Gated like the resolved rung above.** This is where upstream's looseness lands:
            // `Vault.new.secret` types the receiver from the constructor, then asks for a
            // member the interpreter refuses. A *Derived* card still claims the code can make
            // this call.
            //
            // **Also gated on the root, the third road to the same mis-attribution.** A member
            // found on `Object` is found on every receiver, so `resolve_call`'s root arm
            // refuses one whose every `def` is written inside a block. But that arm only sees
            // receivers *rubydex* named. Typed here instead, the walk restarts from a
            // declaration the arm never saw, reaches the same `def`, and answers `precise`,
            // which `resolve_typed` does not fence. Example: `Widget` with
            // `String.class_eval { def self.configure }` in the workspace would answer
            // *Resolved* `Object#configure`.
            //
            // **One class failing a gate leaves the name rung**, so the answer never rests on
            // some of a union's classes.
            Some(found)
                if declared_on_the_root(graph, Some(&sources.memo.blocks), found)
                    && privacy.admits(graph, found) =>
            {
                if !declarations.contains(&found) {
                    declarations.push(found);
                }
            }
            // The receiver was typed and the method is not on it, or Ruby would refuse the call.
            // The name-based list is still the honest answer: a signature can be incomplete, and
            // a wrong "no such method" would be worse than a guess.
            _ => return resolution,
        }
    }
    Resolution {
        declarations,
        precise: true,
        redirected: false,
        // The class the call was about, for `implementation`: one, or none for a union.
        receiver: typed.one(),
        derivation: typed.derivation,
    }
}

/// [`typed`]'s answer for a receiverless call inside a block whose `self` a signature names
/// ([`types::rebound_self`]), or `None` where that does not apply and the rungs below decide.
///
/// - **Only where no receiver is written**, read from the buffer: `self` is the receiver of a bare
///   call and nothing else. The parse happens only inside a rebound block.
/// - **Precise, and derived**: the member is found exactly on a type a signature gave. The card
///   names the signature, as a typed receiver's does.
/// - **Gated like the typed rung** on the root: a member on `Object` whose every `def` sits in a
///   block is not everyone's.
fn rebound_call(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    located: &Located<'_>,
    at_in_source: u32,
    reference: &MethodRef,
) -> Option<Resolution> {
    let graph = sources.graph;
    let typed = types::rebound_self(sources, uri_id, located.start)??;
    let cursor = cursor::at(text, at_in_source)?;
    if !matches!(
        cursor.context,
        Context::Expression | Context::Argument { .. }
    ) {
        return None;
    }
    let member = member_name(graph, *reference.str())?;
    on_each(sources, uri_id, &typed, &member)
}

/// `member` on every class `typed` can be, each class's own declaration: precise, derived, and
/// gated on the root like the typed rung. `None` where any class lacks it, so the answer never rests
/// on some of them. One class is the ordinary case; several are a concern's including classes
///.
fn on_each(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    typed: &types::Typed,
    member: &str,
) -> Option<Resolution> {
    let graph = sources.graph;
    let mut declarations = Vec::new();
    for &on in typed.classes() {
        let found = types::member_of(sources, uri_id, on, StringId::from(member))?;
        if !declared_on_the_root(graph, Some(&sources.memo.blocks), found) {
            return None;
        }
        if !declarations.contains(&found) {
            declarations.push(found);
        }
    }
    Some(Resolution {
        declarations,
        precise: true,
        redirected: false,
        receiver: typed.one(),
        derivation: typed.derivation.clone(),
    })
}

/// The instance side of a class, for a bare name written inside a **block** in its body.
///
/// # Two live scopes, only one searched by the graph
///
/// - **rubydex has no notion of a block.** Its nesting stack holds lexical scopes, `Class.new`
///   owners and methods, so a bare call inside `[1].each { … }` in a class body gets the same
///   receiver as a statement of that body: the **singleton** class. That is right for a block
///   nobody rebinds.
/// - **It is wrong for every block a DSL takes.** `rule(:colon) { str(':') }` runs against a parser
///   *instance*, `scope :recent, -> { where(...) }` against a relation,
///   `validates :x, if: -> { active? }` against a record. The class object has no such method, and
///   the answer falls to the name rung.
/// - **The name rung then picks *wrongly*.** [`reachable_on_a_class_object`] drops every candidate
///   a `class` owns, since a class object cannot reach one. That rule is right for a class object,
///   and wrong for a cursor whose `self` is not one: an inherited method is thrown away and a
///   same-named module method kept.
///
/// # What this asks
///
/// Two halves, both required. The syntax half (is there a block between the cursor and its body?)
/// arrives as a field on the [`Cursor`](cursor::Cursor), from a second walk over the already-parsed
/// tree. The graph half is here: does the attached class have this member on its instance side?
/// Together: the name is **absent** from the class object and **present** on an instance, and a
/// file that meant the class object here would not run.
///
/// # A class, never a module
///
/// The card will say *the block runs against an instance of this*, and a module has no instances. A
/// module body's blocks are `included do` and `class_methods do`, whose `self` is the **including
/// class**. The name rung keeps the module's instance method anyway, so declining costs nothing and
/// keeps the tier honest.
///
/// # Derived, never resolved
///
/// Nothing in the file says the block is rebound. `Tier::Derived` is exactly this: a convention
/// followed, correct if the convention is. The card names the class the member was found on, so a
/// reader who thinks the DSL does otherwise can check. It fires only where the answer was already a
/// name-matched list, replacing the one rung allowed to be wrong.
fn in_a_closure(
    graph: &Graph,
    reference: &MethodRef,
    in_a_closure: bool,
    member: &str,
) -> Option<Resolution> {
    if !in_a_closure {
        return None;
    }
    let owner = graph
        .name_id_to_declaration_id(reference.receiver()?)
        .copied()?;
    // A receiver with an attached class is all "rubydex called this a class object" means:
    // [`resolve_call`] makes the same test before narrowing the name list. Anything else (a call on
    // an instance, or on a receiver rubydex could not name) has no second scope to search.
    let attached = attached_class(graph, owner)?;
    // One lookup for both remaining questions: is it a class (a module has no instances; see
    // above), and what to call it on the card.
    let declaration = graph.declarations().get(&attached)?;
    if !matches!(declaration, Declaration::Namespace(Namespace::Class(_))) {
        return None;
    }
    let found =
        query::find_member_in_ancestors(graph, attached, StringId::from(member), false).ok()?;
    Some(Resolution::derived(
        found,
        Derivation {
            closure: Some(render::qualified_name(graph, declaration.name())),
            ..Derivation::default()
        },
    ))
}

/// Whether the call as written proves that `self` is the class object rubydex named: a written
/// receiver other than `self`, or a bare word that is a statement of the body.
///
/// rubydex records the same class object for a bare word in a block written into the body, which
/// whoever receives the block may run against another object ([`cursor::Cursor::in_a_closure`]).
/// `self.` is left unproven for the same reason: the cursor does not ask whether it sits in such a
/// block.
fn proves_the_class_object(context: &Context, in_a_closure: bool) -> bool {
    match context {
        Context::MethodCall { receiver } => !matches!(receiver, cursor::Receiver::SelfObject(_)),
        Context::Expression | Context::Argument { .. } => !in_a_closure,
        Context::NamespaceAccess { .. } => false,
    }
}

/// [`resolve_call`]'s name rung again, narrowed for certain ([`ClassObject::Certainly`]), where the
/// syntax proves the class object ([`proves_the_class_object`]) and rubydex named a class's or a
/// module's. `None` elsewhere, and the rung stands as drawn.
fn on_a_proven_class_object(
    graph: &Indexed,
    reference: &MethodRef,
    member: &str,
    privacy: Privacy<'_>,
) -> Option<Resolution> {
    let owner = constant_named(graph, reference.receiver()?)?;
    (ClassObject::named(graph, owner) == ClassObject::Certainly)
        .then(|| by_name(graph, member, ClassObject::Certainly, privacy))
}

/// The URI a document id names, for places that ask [`environment::fenced_from`] about the
/// **cursor** rather than a target.
///
/// `pub(super)` for the surfaces that build their own [`environment::Fence::uses`]: `references`,
/// `rename`, `documentHighlight` and the two hierarchies reach the graph through [`resolve`], and
/// each has a cursor and a layout and needs to turn one into the other.
pub(super) fn uri_of(graph: &Graph, uri_id: UriId) -> Option<&str> {
    graph.documents().get(&uri_id).map(Document::uri)
}

/// The name-matched list, minus the places only a test run loads.
///
/// - **Why fence this rung.** It is a guess from letters, and a `def` that exists only under RSpec
///   is never loaded by the application. A cursor in a model landing there is sent somewhere the
///   running program never goes. Nothing above this rung is touched: a *resolved* answer inside a
///   spec is the code saying so.
/// - **The rule, the tag and the cursor gate are [`environment`]'s**, so this and `completion`'s
///   list fence names the same way. **A declaration is kept if *any* definition is loadable, and
///   one with no definitions is kept**; `Tally::loadable` decides both.
/// - **[`references`](super::references) is not filtered, and must not be.** A work list that
///   silently omitted specs would make a rename that breaks the suite. `environment`'s docs hold
///   that table.
fn loadable_from(
    graph: &Graph,
    fence: environment::Fence<'_>,
    resolution: Resolution,
) -> Resolution {
    // No early return for the unfenced case: each question answers `true` outright where its gate
    // is off, so a fence with neither on just rebuilds the same list. This runs once per request,
    // not per candidate, so the rebuild costs nothing measurable.
    Resolution {
        declarations: resolution
            .declarations
            .iter()
            .filter(|id| fence.loadable(graph, **id) && fence.inside(graph, **id))
            .copied()
            .collect(),
        ..resolution
    }
}

/// The same list minus every declaration the project does not contain.
///
/// **Only this meaning, never the tree one**, which keeps it separate from [`loadable_from`]. A
/// *precise* answer is never fenced by a test tree: a constant rubydex resolved against the real
/// nesting is the code saying where the name comes from, and a resolved `def` under `spec/` means
/// read the code. (The receiver walk has already stepped past a member only the suite loads,
/// [`find_loaded_member`]: that is a different chain, not a fenced answer.) That does not hold for a document the application cannot load at all, and
/// [`resolve_typed`] returns precise answers directly, so this is the one narrowing a precise
/// answer gets.
fn inside_only(graph: &Graph, fence: environment::Fence<'_>, resolution: Resolution) -> Resolution {
    // No gate read here: [`environment::Fence::inside`] carries its own, including the exception
    // for the cursor's own document. The gate is only off for a cursor the graph has never held,
    // and rebuilding an unchanged list is cheaper than writing the rule twice.
    Resolution {
        declarations: resolution
            .declarations
            .iter()
            .filter(|id| fence.inside(graph, **id))
            .copied()
            .collect(),
        ..resolution
    }
}

/// `found`, unless `reference` is the superclass of a class rubydex resolved to that class itself
/// (an upstream defect). There it is the class Ruby names, or nothing when Ruby would find
/// none: `class ApplicationController < ApplicationController` inside `module Admin` names the
/// top-level class, never the class being opened.
///
/// **Only at that one reference.** rubydex keys a name by its spelling and nesting, so every other
/// `ApplicationController` written straight inside `module Admin` is the same name, and there it
/// does mean `Admin::ApplicationController`.
fn unless_its_own_superclass(
    graph: &Graph,
    reference: &ConstantReference,
    found: DeclarationId,
) -> Option<DeclarationId> {
    let its_own = definitions_of(graph, found).into_iter().any(|definition| {
        let Definition::Class(class) = definition else {
            return false;
        };
        class
            .superclass_ref()
            .and_then(|id| graph.constant_references().get(id))
            .is_some_and(|superclass| {
                superclass.uri_id() == reference.uri_id()
                    && superclass.offset() == reference.offset()
            })
    });
    if its_own {
        indexed::superclass_outside(graph, found)
    } else {
        Some(found)
    }
}

/// The declarations a target names.
///
/// **Its fence is never the tree fence, and never nothing.** `references`, `rename`,
/// `documentHighlight` and the two hierarchies reach the graph through here, and they answer *where
/// is this used*, where a use under `spec/` counts. So the three tree rules must not reach them
/// ([`environment::Fence::uses`]). The one rule that does is the other meaning: a document
/// **outside the project** is not the project's, and a work list, call tree or type tree reaching
/// into it would answer about a buffer that merely happens to be open. `environment`'s table has
/// the full argument.
#[must_use]
pub fn resolve(
    graph: &Indexed,
    located: &Located<'_>,
    fence: environment::Fence<'_>,
) -> Resolution {
    match located.target {
        // Precise, and fenced anyway, unlike the tree rules. A constant resolved against the real
        // nesting shows where the name comes from, which holds for a spec but not here: the
        // application cannot load a scratch buffer.
        Target::Constant(reference) => inside_only(
            graph,
            fence,
            Resolution::precise(
                constant_named(graph, *reference.name_id())
                    .and_then(|found| unless_its_own_superclass(graph, reference, found))
                    .into_iter()
                    .collect(),
            ),
        ),
        // **`Privacy::Allowed`, for the same reason the tree gate is off.** These callers have no
        // buffer to parse, so the gate's question cannot be asked. They also ask *where is this
        // used*, not *what does this call reach*, and a use of a private method is a use.
        Target::Call(reference) => inside_only(
            graph,
            fence,
            resolve_call(
                graph,
                reference,
                fence,
                Privacy::Allowed,
                // **No buffer to read, so rubydex is trusted** (see [`declared_on_the_root`], and
                // the paragraph above for why this caller answers every gate that way).
                None,
            ),
        ),
        // **A `def` is its own receiver's answer**, so its owner travels with it: the class or
        // module the cursor is in is exactly where the override list is taken. Singletons fall out:
        // rubydex attributes `def self.x` to `Foo::<Foo>`, whose descendants are the singletons of
        // `Foo`'s subclasses.
        Target::Definition(definition) => {
            let resolution = inside_only(
                graph,
                fence,
                Resolution::precise(declaration_of(graph, definition).into_iter().collect()),
            );
            match resolution
                .declarations
                .first()
                .and_then(|id| graph.declarations().get(id))
            {
                // **Methods only.** A `def`'s owner is the class the cursor is in, where the
                // override list is taken. A `class Foo` is its own subject, and its owner is just
                // the namespace it was written in, so filling this would name `M` for a `class Foo`
                // in `module M`.
                Some(declaration @ Declaration::Method(_)) => {
                    resolution.on(*declaration.owner_id())
                }
                _ => resolution,
            }
        }
    }
}

/// The declaration a definition makes: rubydex's answer, except for two cases it gets wrong.
///
/// 1. **`def Foo.bar` and `def Foo::bar`**, a singleton method with the class written instead of
///    `self`. rubydex attributes `def self.bar` via the enclosing namespace's singleton, and a
///    plain `def bar` via the namespace itself, but a *named* receiver falls into the second: `bar`
///    is looked up as an **instance** member of the lexically enclosing namespace. That usually
///    misses (the `def` line answers nothing; rexml writes its whole XPath surface this way), and
///    where it hits, the cursor reports `Foo#bar`, a different method. So a named receiver is
///    answered here outright: falling back would fall back onto the wrong answer.
/// 2. **A body opened under an alias.** Ruby ships `Gem::URI = Bundler::URI`, and the vendored copy
///    writes `module Gem::URI`, reopening Bundler's module. Members are attributed by the body's
///    *name*, which resolves to the alias: no members, no singleton, so the `def` line answers
///    nothing. Everything else is right (the member is declared as `Bundler::URI#self.split`, calls
///    resolve, nested modules answer), so only this reverse map is broken. The retry is a
///    **fallback, never an override**: rubydex and the named-receiver lookup go first, and only
///    silence is answered. `def Ripper.parse` needs it too, since Ruby 4 ships
///    `Ripper = Prism::Translation::Ripper`.
fn declaration_of(graph: &Graph, definition: &Definition) -> Option<DeclarationId> {
    if let Some((receiver, member)) = named_receiver(definition) {
        return singleton_member(graph, receiver, member)
            .or_else(|| singleton_member(graph, aliased_name(graph, receiver)?, member));
    }
    if let Some(declaration) = graph.definition_to_declaration_id(definition) {
        return Some(*declaration);
    }
    let (owner, member, singleton) = nested_member(graph, definition)?;
    let owner = aliased_name(graph, owner)?;
    if singleton {
        singleton_member(graph, owner, member)
    } else {
        instance_member(graph, owner, member)
    }
}

/// The member a namespace declares, by the namespace's name.
fn instance_member(graph: &Graph, owner: NameId, member: StringId) -> Option<DeclarationId> {
    graph
        .declarations()
        .get(graph.name_id_to_declaration_id(owner)?)?
        .as_namespace()?
        .member(&member)
        .copied()
}

/// The member a namespace's *singleton* class declares, by the namespace's name.
fn singleton_member(graph: &Graph, owner: NameId, member: StringId) -> Option<DeclarationId> {
    let owner = graph.name_id_to_declaration_id(owner)?;
    let singleton = *graph
        .declarations()
        .get(owner)?
        .as_namespace()?
        .singleton_class()?;
    graph
        .declarations()
        .get(&singleton)?
        .as_namespace()?
        .member(&member)
        .copied()
}

/// Which namespace a definition adds a method to, which method, and whether it is the singleton's,
/// for the five definitions that add one by being *written inside* a body.
///
/// rubydex's own pairing, read again so [`aliased_name`] can retry it. Only method-declaring kinds,
/// on purpose: a class, module or constant is attributed by its **own** name, so it already
/// resolves under an alias (`module Gem::URI::Schemes` answers `Bundler::URI::Schemes`). Variables
/// and visibility markers stay where upstream puts them.
fn nested_member(graph: &Graph, definition: &Definition) -> Option<(NameId, StringId, bool)> {
    let lexical = |nesting: &Option<DefinitionId>, member: &StringId| {
        Some((enclosing_name(graph, *nesting)?, *member, false))
    };
    let on_self = |owner: &DefinitionId, member: &StringId| {
        Some((*graph.definitions().get(owner)?.name_id()?, *member, true))
    };
    match definition {
        // A named receiver never arrives here: `declaration_of` answers it above.
        Definition::Method(it) => match it.receiver() {
            Some(Receiver::SelfReceiver(owner)) => on_self(owner, it.str_id()),
            Some(Receiver::ConstantReceiver(_)) => None,
            None => lexical(it.lexical_nesting_id(), it.str_id()),
        },
        Definition::MethodAlias(it) => match it.receiver() {
            Some(Receiver::SelfReceiver(owner)) => on_self(owner, it.new_name_str_id()),
            // `Foo.alias_method :a, :b` with `Foo` an alias never reaches here: rubydex panics
            // *while indexing* that file ("Tried to add member to a declaration that isn't a
            // namespace"), the per-file seam skips it, and no definition is left. With `Foo` a real
            // class it resolves and does not reach here either.
            Some(Receiver::ConstantReceiver(_)) => None,
            None => lexical(it.lexical_nesting_id(), it.new_name_str_id()),
        },
        Definition::AttrReader(it) => lexical(it.lexical_nesting_id(), it.str_id()),
        Definition::AttrWriter(it) => lexical(it.lexical_nesting_id(), it.str_id()),
        Definition::AttrAccessor(it) => lexical(it.lexical_nesting_id(), it.str_id()),
        _ => None,
    }
}

/// The name of the nearest enclosing definition that has one.
///
/// A walk, not one step, because upstream's `find_enclosing_namespace_name_id` climbs past every
/// nameless definition. The five kinds retried above never need to climb (their nesting is always
/// the body itself), but an instance variable's nesting is the `def` around it, and diverging would
/// be a silent difference. Those two branches are the only ones no test here takes.
fn enclosing_name(graph: &Graph, mut nesting: Option<DefinitionId>) -> Option<NameId> {
    while let Some(definition) = nesting.and_then(|id| graph.definitions().get(&id)) {
        if let Some(name) = definition.name_id() {
            return Some(*name);
        }
        nesting = *definition.lexical_nesting_id();
    }
    None
}

/// The name an alias finally stands for, and **nothing unless a hop was taken**.
///
/// `Thing = 1` is a constant too, and `def Thing.bar` on it must keep answering nothing, so the
/// target is read from a `ConstantAlias` *definition*, the only kind that records one. The chain is
/// walked because Ruby allows an alias of an alias, and capped because Ruby also allows `A = B`
/// beside `B = A`.
fn aliased_name(graph: &Graph, mut name: NameId) -> Option<NameId> {
    const HOPS: usize = 8;

    for _ in 0..HOPS {
        let declaration = graph
            .declarations()
            .get(graph.name_id_to_declaration_id(name)?)?;
        if declaration.as_namespace().is_some() {
            return Some(name);
        }
        name =
            declaration
                .definitions()
                .iter()
                .find_map(|id| match graph.definitions().get(id) {
                    Some(Definition::ConstantAlias(alias)) => Some(*alias.target_name_id()),
                    _ => None,
                })?;
    }
    None
}

/// The class or module a constant alias finally stands for, and nothing for any other declaration.
///
/// arel writes `Attribute = Attributes::Attribute`, and its own `Attribute.new` builds an
/// `Arel::Attributes::Attribute`: an object's class is the namespace the alias names, since the
/// alias itself holds no member.
pub(super) fn alias_target(graph: &Graph, id: DeclarationId) -> Option<DeclarationId> {
    let declaration = graph.declarations().get(&id)?;
    if !matches!(declaration, Declaration::ConstantAlias(_)) {
        return None;
    }
    let target =
        declaration
            .definitions()
            .iter()
            .find_map(|id| match graph.definitions().get(id) {
                Some(Definition::ConstantAlias(alias)) => Some(*alias.target_name_id()),
                _ => None,
            })?;
    graph
        .name_id_to_declaration_id(aliased_name(graph, target)?)
        .copied()
}

/// The class `def Foo.bar` names and the member it declares on it; nothing for any other definition
/// (a plain `def`, `def self.`, a constant, an `attr_reader`).
fn named_receiver(definition: &Definition) -> Option<(NameId, StringId)> {
    let Definition::Method(method) = definition else {
        return None;
    };
    match method.receiver() {
        Some(Receiver::ConstantReceiver(receiver)) => Some((*receiver, *method.str_id())),
        Some(Receiver::SelfReceiver(_)) | None => None,
    }
}

/// The method a call at `offset` resolves to, only when it resolved exactly.
///
/// - **The gate for everything that shows a *signature* for a call**, not a place to jump to:
///   completion's keyword arguments and `textDocument/signatureHelp`. A name match would show
///   another class's parameters, a wrong answer the user cannot see is wrong, since it is
///   syntactically valid.
/// - **The redirect is kept.** `Foo.new(` resolves to `Foo#initialize`, whose parameters are what
///   the call takes. `references` must not have it, and reads [`Resolution`] directly.
/// - **`privacy` is the caller's answer**, like the fence. A signature card for a method Ruby
///   refuses would disagree with the jump at the same cursor. `signature_help` passes what
///   `cursor::Call::allows_private` read from the call node; the outgoing call hierarchy has no
///   cursor and passes [`Privacy::Allowed`].
#[must_use]
pub fn precise_call(
    graph: &Indexed,
    uri_id: UriId,
    offset: u32,
    layout: environment::Layout<'_>,
    privacy: Privacy<'_>,
    blocks: &Blocks<'_>,
) -> Option<DeclarationId> {
    locate(graph, uri_id, offset)
        .into_iter()
        .find_map(|located| match located.target {
            // **Fenced like the jump, not like `resolve`.** These callers draw a signature card or
            // a call-hierarchy row for a call in *this* document, so they ask the jump's question
            // at the jump's cursor. Keeping a rooted answer `definition` refused would show a
            // signature nobody can call, and the two answers would disagree. They never fall to the
            // name rung: an exact callee is their whole contract, so the fenced case is `None` and
            // no card is drawn.
            Target::Call(reference) => {
                let resolution = resolve_call(
                    graph,
                    reference,
                    environment::Fence::at(uri_of(graph, uri_id), layout),
                    // **Passed in, not decided here, because this rung has an offset, not a text.**
                    // Each caller knows a different amount: the signature card has the `CallNode`'s
                    // receiver, completion's keyword arguments are always at an implicit receiver,
                    // and the outgoing call hierarchy walks call sites with no cursor and can only
                    // say `Allowed`. This function must not guess for them.
                    privacy,
                    // **Never `None` here**, the same rule as the fence above. All three callers
                    // have a document, so a `def` a block filed on `Object` is refused the card and
                    // the outgoing row, as the jump refuses it the place. See [`Blocks`].
                    Some(blocks),
                );
                resolution
                    .precise
                    .then(|| resolution.declarations.into_iter().next())
                    .flatten()
            }
            _ => None,
        })
}

/// Every definition of a declaration, best first, and stable.
///
/// - **Order matters twice**: goto jumps to the first entry, and hover reads its comments.
///   `Declaration::definitions` fills in parallel as documents index, so without sorting both would
///   change between runs.
/// - **Path order alone is stable but meaningless.** Ruby reopens namespaces freely, so a wide one
///   collects a definition per file that touched it (`Sidekiq`, `Rails`).
///   Path order would put an rspec helper ahead of the gem's own `sidekiq.rb`.
/// - **The rule: the file named after the constant wins; otherwise the path decides.** See
///   [`named_after`] for both halves and how weak the second is. **Namespaces only**: a class or
///   module is what files are named for, while a method's file is named after its class, so ranking
///   methods this way would reorder on noise.
/// - **It cannot make a wide namespace's list useful.** A constant reopened in hundreds of files
///   has no definition site; every entry is a `module` keyword wrapping something else. The list is
///   the right shape, and the order is all there is to get right.
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
        let uri = document_uri(graph, definition);
        (uri, definition.offset().start(), definition.offset().end())
    });
    // **Stable, and second**, so the sort above still decides between two equally ranked files
    // (answers never move between runs), and two definitions in one file stay adjacent, which lets
    // [`sites`] deduplicate against its neighbour.
    //
    // Skipped below two entries, which is nearly every declaration: an allocating sort key is not
    // worth building for a list with nothing to order.
    if definitions.len() > 1 && matches!(declaration, Declaration::Namespace(_)) {
        let wanted = squashed(unqualified(declaration.name()));
        definitions
            .sort_by_cached_key(|definition| named_after(&wanted, document_uri(graph, definition)));
    }
    definitions
}

fn document_uri<'g>(graph: &'g Graph, definition: &Definition) -> &'g str {
    graph
        .documents()
        .get(definition.uri_id())
        .map_or("", Document::uri)
}

/// How close a document's file name is to the declared name. **Smaller is better**; it is a sort
/// key, not a score anyone reads.
///
/// Both sides are squashed to lowercase letters and digits, so `active_record.rb`,
/// `ActiveRecord.rb` and `active-record.rb` all spell `ActiveRecord`. Then two tiers ask different
/// questions, because most wide namespaces have no file named after them.
///
/// 1. **Is the file named after the constant?** The name must be *in* the stem (`sidekiq.rb` and
///    `sidekiq_adapter.rb` both count, and `ruby-progressbar.rb` for `ProgressBar`, hence
///    containment, not prefix) **and be at least half of it**. Otherwise a long file name swallows
///    a short constant: an application may write `Jobs` in hundreds of files, and
///    `remove_old_auto_close_jobs.rb` would outrank everything in `app/jobs/`. Within the tier,
///    **how much longer** the stem is decides, so an exact match scores zero and wins.
/// 2. **Otherwise the file name says nothing; the path decides.** an engine monorepo writes `module Spree` in
///    539 files, none of them `spree.rb`, and comparing stem lengths (`setup` and `spree` are both
///    five letters) is noise. What means something is a directory the constant names, then the file
///    nearest the top of that tree: `plugins/search-ai/plugin.rb` for `SearchAi`,
///    `app/jobs/base.rb` for `Jobs`.
///
/// - **The second tier is weak, and says so.** A namespace reopened in hundreds of files has no
///   definition to find, so this picks a plausible door, not the right one.
/// - **The name is unqualified**: the file for `Sidekiq::Worker` is `worker.rb`. Squashing strips a
///   singleton's angle brackets (`Person::<Person>` asks about `person`), and a name that squashes
///   to nothing is *unnamed*, leaving the path to decide.
/// - **Read from the URI, not a decoded path.** Only the file name and its directories are read,
///   and percent-encoding rarely reaches them; an escaped directory just fails to match, costing a
///   tie-break, never an answer.
/// - **A non-file document sorts behind every file**, before either tier. rubydex declares Ruby's
///   object model under `rubydex:built-in`, and ya-lsp's generators use URIs of the same shape
///   (deliberately not `file:`, so the editor is never handed one). Without this,
///   `rubydex:built-in` squashes to a long word that beats the `.rbs` really declaring
///   `BasicObject`, and `preferred_definition` picks a place no request may point at.
fn named_after(wanted: &str, uri: &str) -> (bool, bool, usize, bool, usize) {
    let stem = squashed(stem_of(uri));
    // Sorting puts `false` and the smaller number first, so each question is phrased the way the
    // key reads. Three of the five fields matter in one tier only; the other tier writes the
    // neutral value, so there is one key shape.
    let unopenable = !uri.starts_with("file:");
    if stem.contains(wanted) && wanted.len() * 2 >= stem.len() {
        return (unopenable, false, stem.len() - wanted.len(), false, 0);
    }
    let path = path_of(uri);
    (
        unopenable,
        true,
        0,
        !directory_named(wanted, path),
        path.matches('/').count(),
    )
}

/// A URI with its scheme removed, so the second tier reads only directories.
///
/// `file:` squashes to `file` (the colon drops with the separators), so a scheme left on puts
/// **every** document in a directory named after `File`, and one application's two places for `class File`
/// would swap. Pinned by a test.
fn path_of(uri: &str) -> &str {
    uri.split_once("://").map_or(uri, |(_, path)| path)
}

/// Does a directory above the file carry the constant's name?
///
/// The last segment is the file, which [`named_after`]'s first tier already asked about. Everything
/// before it is the tree the file sits in: `plugins/search-ai/` says `SearchAi` lives there.
/// Compared without building a string per segment, because this runs per definition of a namespace
/// that may have hundreds.
///
/// Takes a **path**, not a URI; see [`path_of`].
fn directory_named(wanted: &str, path: &str) -> bool {
    let Some((directories, _)) = path.rsplit_once('/') else {
        return false;
    };
    directories
        .split('/')
        .any(|segment| squashes_to(segment, wanted))
}

/// Does one path segment squash to exactly this name? No allocation.
fn squashes_to(segment: &str, wanted: &str) -> bool {
    let mut wanted = wanted.chars();
    segment
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .all(|character| wanted.next() == Some(character.to_ascii_lowercase()))
        && wanted.next().is_none()
}

/// The file name a URI ends in, without its extension.
fn stem_of(uri: &str) -> &str {
    let name = uri.rsplit('/').next().unwrap_or(uri);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// A declaration's own name, without the namespaces above it: the file for `Sidekiq::Worker` is
/// `worker.rb`.
fn unqualified(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Lowercase letters and digits, nothing else.
fn squashed(text: &str) -> String {
    text.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

/// Which single definition of a declaration a list should point at.
///
/// 1. **The user's own code wins** when a name is defined in both: searching for
///    `ApplicationRecord` should land in `app/models`, not in a gem that reopens it. Otherwise the
///    first in [`definitions_of`]'s stable order, so the answer never moves.
/// 2. **Among the project's own, the copy the application loads.** A name written in both
///    environments is usually written once in the app and often under `spec/`
///    (`Object#create_category` is one `def` in a rake task and forty in specs), and which of those
///    sorts first is arbitrary. Migrations are the third tree, for consistency: the sorts above
///    already prefer a file named after the constant, so a migration wins only when the app's own
///    copy is unnamed and deep. This is a tie-break, not a fence: a declaration written *only* in a
///    test tree still gets a row, since a row pointing somewhere beats none. Surfaces that care
///    about listing it decided that already ([`environment`](super::environment)).
/// 3. **A definition outside the project is not a candidate at all**: the one place this drops
///    rather than orders. A tree the app does not load is still in the workspace; a document open
///    *beside* the project is not, and a row pointing there points out of the workspace. If every
///    definition is out there, the answer is `None`, and both callers drop unplaceable rows.
/// 4. **`here` is the one exception.** The symbol picker has no document and passes `None`. A
///    hierarchy rooted in a document outside the project is a reader asking about *that* document,
///    so the request's own root is exempt and every other outside document stays dropped:
///    `environment::Outward::Alone`, one level up.
///
/// - **The test-tree tag is read here, not through `environment::Trees`**: this runs once per *row*
///   (the picker computes sites only for survivors), so one string split per definition beats
///   building a set per request. **Load paths are the opposite** and arrive as a set built when the
///   graph settled, since each definition is checked against hundreds of prefixes.
/// - **Shared by the symbol picker and the type hierarchy**, because "which of the two hundred
///   reopenings of `ActiveRecord::Base` does this row mean" is one question, and two answers would
///   put a symbol in different files depending on the list.
#[must_use]
pub fn preferred_definition<'g>(
    graph: &'g Graph,
    declaration_id: DeclarationId,
    own: &HashSet<UriId>,
    outside: &HashSet<UriId>,
    names: environment::Names<'_>,
    here: Option<UriId>,
) -> Option<&'g Definition> {
    let definitions: Vec<&Definition> = definitions_of(graph, declaration_id)
        .iter()
        .copied()
        .filter(|definition| {
            !outside.contains(definition.uri_id()) || here == Some(*definition.uri_id())
        })
        .collect();
    let loadable = |definition: &Definition| {
        graph
            .documents()
            .get(definition.uri_id())
            .is_none_or(|document| {
                !names.in_a_test_tree(document.uri())
                    && !environment::in_a_generator_template(document.uri())
                    && !names.in_a_migration(document.uri())
            })
    };
    definitions
        .iter()
        .copied()
        .find(|definition| own.contains(definition.uri_id()) && loadable(definition))
        .or_else(|| {
            definitions
                .iter()
                .copied()
                .find(|definition| own.contains(definition.uri_id()))
        })
        .or_else(|| definitions.first().copied())
}

/// Every place a declaration is written, in [`definitions_of`]'s order, each once.
///
/// - **A place is its name, and is keyed on it.** Two definitions of one declaration can be two
///   readings of one `def`: `find_by` on a Rails model is a row on the query interface *and* a
///   `def` in activerecord's `module ClassMethods`, so two generators declare it. [`site`] maps
///   each through the side table, and they come back with the same `selection` (the name) and
///   different `full` spans (the whole `def … end` versus the header the concern generator read).
///   Two distinct definitions cannot share a name span in one document, and one `def` reached twice
///   always does.
/// - **First occurrence wins**, so the reader sees [`definitions_of`]'s ranking, and the whole
///   construct is kept rather than the header.
/// - **Why not `Vec::dedup`:** it compares whole `Site`s and only neighbours, so a pair differing
///   in `full` survives, and the reader sees *Found 2 definitions* for one line.
#[must_use]
pub fn sites(graph: &Graph, synthesized: &Synthesized, declaration_id: DeclarationId) -> Vec<Site> {
    let mut seen: HashSet<(String, (u32, u32))> = HashSet::new();
    definitions_of(graph, declaration_id)
        .into_iter()
        .filter_map(|definition| site(graph, synthesized, definition))
        .filter(|site| seen.insert((site.uri.clone(), site.selection)))
        .collect()
}

/// Where a reader is *sent*: [`sites`] narrowed four ways.
///
/// 1. **A signature is a declaration, not a definition.** `stdlib/cgi-escape/0/escape.rbs` says
///    what `CGI.escape` takes and returns; nobody wrote the method there, and a jump lands in a
///    stub. So an `.rbs` is dropped wherever real source survives beside it, and kept where none
///    does: a signature beats silence. What is dropped here is what `textDocument/declaration` asks
///    for ([`all_signatures`]).
/// 2. **A copy the project would never load is not a second place.** A bundle pinning `cgi` puts
///    the gem's `lib/cgi/escape.rb` on the load path ahead of Ruby's copy, and `require` reads one
///    of them. Both are indexed, so without this the card says *Defined in 3 places* for one
///    method. The winner is the load order: `Workspace::load_paths`, i.e. Bundler's. Common for
///    `uri/generic.rb`, `net/http/header.rb`, `prism/node.rb`.
/// 3. **A document the editor cannot open is not a place.** rubydex declares `Module` and `Kernel`
///    under `rubydex:built-in`, which [`DocUri::from_graph_uri`] refuses for every request. The
///    jump always dropped it; the count must too.
/// 4. **A copy only the suite loads is not what a reader in application code asked for.** A
///    monorepo's specs are most of the files reopening a namespace (an engine monorepo offers 539 places for
///    `Spree`, 76 under `spec/`). This is [`environment`](super::environment)'s rule on a place
///    list, like the name rung, completion and the picker. A **drop**, not a rank: a jump sends the
///    reader somewhere and a peek list is read from the top.
///
/// - **Rules 2 and 4 keep their places where nothing better survives**: a declaration only a spec
///   writes is still worth answering.
/// - **Rule 4 is off when the cursor is itself in a test or `testing_support` tree**: the spec's
///   copy is for exactly that reader.
/// - **Only *this project's* test trees count**
///   ([`environment::Fence::unloadable`](super::environment::Fence::unloadable)). The tag is four
///   directory names, and a gem shipping `lib/rack/test/`, `railties`' `rails/commands/test/`, or
///   Ruby's `minitest/test/` signatures are libraries, not suites. Every fencing surface reads that
///   one function.
/// - **`cursor` is the asking document.** `None` means *no cursor*, not *nowhere*, and
///   [`environment::fenced_from`](super::environment::fenced_from) reads it as unfenced, the
///   conservative direction. `completionItem/resolve` has none: the protocol hands back an item,
///   not a position.
/// - **`references` uses [`sites`] on purpose.** Every mention counts, and a rename skipping the
///   project's `sig/` would leave a signature naming a method that no longer exists.
#[must_use]
pub fn places(
    graph: &Graph,
    synthesized: &Synthesized,
    layout: environment::Layout<'_>,
    declaration_id: DeclarationId,
    cursor: Option<&str>,
) -> Vec<Site> {
    narrowed(
        graph,
        synthesized,
        layout,
        declaration_id,
        cursor,
        Keep::Source,
    )
}

/// Which side of the source/signature split a list is asked for.
///
/// **Only one of the four narrowings differs between the two**, which is all
/// `textDocument/declaration` is: an unopenable document, a copy `require` never reaches, and a
/// suite-only copy are excluded either way. Sharing them matters because a filter applied in one of
/// two places has a hole ([`resolve_typed`]'s rule for the fence).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Keep {
    /// Where the method is *written*, keeping a signature only where no source survives beside it.
    /// For `definition` and the gotos below it.
    Source,
    /// Where it is *declared*: an `.rbs`, never anything else.
    Signature,
}

/// [`places`] and [`all_signatures`] are one function narrowing a list four ways; [`Keep`] is where
/// they differ.
fn narrowed(
    graph: &Graph,
    synthesized: &Synthesized,
    layout: environment::Layout<'_>,
    declaration_id: DeclarationId,
    cursor: Option<&str>,
    keep: Keep,
) -> Vec<Site> {
    let mut places = sites(graph, synthesized, declaration_id);
    places.retain(|place| DocUri::from_graph_uri(&place.uri).is_some());
    let shadowed = shadowed(&places, layout.load);
    places.retain(|place| !shadowed.contains(&place.uri));
    match keep {
        Keep::Source => {
            if places.iter().any(|place| !is_signature(&place.uri)) {
                places.retain(|place| !is_signature(&place.uri));
            }
        }
        // **Unconditional: the inverse of the rule above in what it keeps, never in when it
        // applies.** The literal inverse (*drop source wherever a signature survives*) would keep
        // the `.rb` wherever no signature exists: a fallback, which would make this a second
        // `definition`. An empty list is the `null` that returns the client its own behaviour.
        Keep::Signature => places.retain(|place| is_signature(&place.uri)),
    }
    let fence = environment::Fence::at(cursor, layout);
    if fence.on_trees() && places.iter().any(|place| !fence.unloadable(&place.uri)) {
        places.retain(|place| !fence.unloadable(&place.uri));
    }
    // **No safety clause here, which is the difference between the two meanings.** A place the app
    // does not load is worse than one it does, so when every place is like that the list is kept
    // whole. A place *outside the project* is not worse, it is not the project's, and sending a
    // reader from `app/` into a scratch buffer is the defect. An empty list is honest.
    //
    // The cursor's own document is exempt, carried by `Fence::outside`: a reader in a file the
    // project does not contain is exactly who the class at the top of that file answers for. See
    // `environment::Outward`.
    places.retain(|place| !fence.outside(&place.uri));
    places
}

/// Every place a whole resolution names, each once.
///
/// - **Deduplicated across declarations, not just within one**, which [`places`] cannot do since it
///   sees one declaration at a time. A name rung answering `find_each` offers
///   `ActiveRecordRelation#find_each`, `Story::Relation#find_each` and one per relation class, and
///   they all point at the same `def` in the bundle: `Defined in N places` would overstate N.
/// - **First occurrence wins**, keeping the order [`definitions_of`] and [`places`] applied.
/// - **Keyed on the name, not the whole construct**, for [`sites`]' reason: two declarations can
///   reach one `def` at two spans (one generator read the header, the other the whole).
#[must_use]
pub fn all_places(
    graph: &Graph,
    synthesized: &Synthesized,
    layout: environment::Layout<'_>,
    declarations: impl IntoIterator<Item = DeclarationId>,
    cursor: Option<&str>,
) -> Vec<Site> {
    every(
        graph,
        synthesized,
        layout,
        declarations,
        cursor,
        Keep::Source,
    )
}

/// Every place a whole resolution names that is a **signature**: all of
/// `textDocument/declaration`'s answer.
///
/// [`all_places`] with one narrowing swapped ([`Keep`]) and nothing else changed: same
/// deduplication, ranking and other three filters. An empty list is `null`, and that is the usual
/// case: a project with no `sig/` and no `.gem_rbs_collection` has signatures only for Ruby's own
/// library.
#[must_use]
pub fn all_signatures(
    graph: &Graph,
    synthesized: &Synthesized,
    layout: environment::Layout<'_>,
    declarations: impl IntoIterator<Item = DeclarationId>,
    cursor: Option<&str>,
) -> Vec<Site> {
    every(
        graph,
        synthesized,
        layout,
        declarations,
        cursor,
        Keep::Signature,
    )
}

/// The deduplication both lists make, over the half of the split [`Keep`] names.
fn every(
    graph: &Graph,
    synthesized: &Synthesized,
    layout: environment::Layout<'_>,
    declarations: impl IntoIterator<Item = DeclarationId>,
    cursor: Option<&str>,
    keep: Keep,
) -> Vec<Site> {
    let mut seen: HashSet<(String, (u32, u32))> = HashSet::new();
    declarations
        .into_iter()
        .flat_map(|id| narrowed(graph, synthesized, layout, id, cursor, keep))
        .filter(|site| seen.insert((site.uri.clone(), site.selection)))
        .collect()
}

/// Which of a list's places are copies of a file another of them shadows.
///
/// Keyed by what `require` would be asked for (the path under a load path, the same for Ruby's
/// `uri/generic.rb` and the `uri` gem's), won by the first matching load path, since `require`
/// searches that first.
///
/// A place under no load path has no key and is never shadowed. That is most places: a project's
/// own files, a gem's `app/` and `sig/` are indexed but not on a load path, and copy nothing.
fn shadowed(places: &[Site], load: &[String]) -> HashSet<String> {
    let mut winner: HashMap<&str, (usize, &str)> = HashMap::new();
    for place in places {
        if let Some((rank, required)) = under(&place.uri, load) {
            let best = winner.entry(required).or_insert((rank, &place.uri));
            if rank < best.0 {
                *best = (rank, &place.uri);
            }
        }
    }
    places
        .iter()
        .filter(|place| {
            under(&place.uri, load).is_some_and(|(_, required)| winner[required].1 != place.uri)
        })
        .map(|place| place.uri.clone())
        .collect()
}

/// What `require` would be asked for to reach this document, and which load path answers.
///
/// The **first** matching prefix, because `require` searches in order: Ruby's platform directory
/// sits inside its library directory, so several entries can match one document.
fn under<'a>(uri: &'a str, load: &[String]) -> Option<(usize, &'a str)> {
    load.iter()
        .enumerate()
        .find_map(|(rank, prefix)| uri.strip_prefix(prefix.as_str()).map(|rest| (rank, rest)))
}

/// Whether a document is RBS rather than Ruby.
///
/// By extension, as `Workspace::indexes` decides: rubydex records `.rbs` declarations exactly like
/// `.rb` ones, so the file name is the only evidence.
fn is_signature(uri: &str) -> bool {
    uri.ends_with(".rbs")
}

/// Where a single definition is written.
///
/// **The one place a declaration becomes a place**, so the side table of everything ya-lsp
/// generated is consulted here and nowhere else. A generated definition sits at an offset into
/// bytes that are not on disk, so it answers with the line that implied it, or nothing if none was
/// recorded. Dropping it is the point: the alternative is a link into a document the editor cannot
/// open. Every list reaching here already filters the misses.
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

/// The two spans a definition contributes to a response: the whole construct, and the name inside
/// it.
///
/// - **The protocol requires the name to be inside the construct**, in both places it is sent:
///   `DocumentSymbol::selectionRange` and `LocationLink::targetSelectionRange`. VS Code enforces
///   the first by *throwing* (`selectionRange must be contained in fullRange`), which drops the
///   entire outline, not just the bad symbol.
/// - **Prism's error recovery breaks that**, and half-typed code is a buffer's normal state. A bare
///   `def` at a line's end recovers into a node spanning the three keyword bytes with a name
///   location in the whitespace *after* them. There is no name there, so the construct is the
///   honest answer, never a span the editor would reject.
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
/// One predicate for two users: `spans` above (a pair the protocol requires to nest) and
/// `ranges::selection_chain` (a chain the protocol defines as nesting). Both guard against the same
/// parser recovery, and a containment test written twice will one day disagree with itself.
pub(super) const fn nests(outer: (u32, u32), inner: (u32, u32)) -> bool {
    inner.0 >= outer.0 && inner.1 <= outer.1
}

/// The file a `require "..."` names, if the graph has indexed it.
///
/// The jump lands at the top of the file: a required file has no single definition, and the top is
/// what an editor's own file navigation shows.
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

/// rubydex's spelling of a method member: `shout()`, parentheses included.
///
/// Declarations key members by this string, but call sites are recorded under the bare name, except
/// `alias`, which records the parenthesised form. Both must reach the same key, or every method
/// lookup misses silently.
fn member_name(graph: &Graph, str_id: StringId) -> Option<String> {
    let raw = graph.strings().get(&str_id)?.as_str();
    Some(if raw.ends_with(')') {
        raw.to_owned()
    } else {
        format!("{raw}()")
    })
}

/// Whether a **private** declaration may answer the call under the cursor.
///
/// - **Ruby's rule is about how the receiver was *written*.** A private method is callable with no
///   receiver or with one spelled `self`; `other.secret` raises `NoMethodError` even inside the
///   class declaring `secret`. [`cursor::Context::allows_private`] reads that from the syntax, the
///   only place it can be read, so it travels into [`resolve_call`] as a parameter, like the
///   environment fence.
/// - **The graph cannot recover it.** rubydex's `MethodRef` carries only `Option<NameId>` for the
///   receiver, and fills it both for `Foo.bar` and for a bare call in `Foo`'s own body: the two
///   cursors with opposite answers. Upstream's own check is a looser rule: it passes a private
///   method whenever the caller's `self` is the receiver's class, while Ruby exempts only a
///   receiver *written* `self`.
#[derive(Clone, Copy)]
pub enum Privacy<'a> {
    /// The syntax permits one: an implicit receiver, or one spelled `self`. Also what every
    /// text-less caller passes, because refusing on a question it cannot ask would delete answers
    /// on a guess.
    Allowed,
    /// A receiver is written and is not `self`, so Ruby would raise here, **plus the second opinion
    /// the refusal needs**, because the record it refuses on is wrong for one ordinary shape (see
    /// [`Modifiers`]). It lives in this arm because only this arm asks: `Allowed` admits without a
    /// lookup.
    Refused(&'a Modifiers<'a>),
}

impl<'a> Privacy<'a> {
    /// The same answer from a caller that has already read the syntax.
    ///
    /// `signature_help` holds a [`cursor::Call`](super::cursor::Call), not a `Context` (they
    /// classify cursors differently on purpose; see `Call`'s docs), so it passes the bool, not the
    /// enum.
    pub(super) fn written(allows_private: bool, modifiers: &'a Modifiers<'a>) -> Self {
        if allows_private {
            Privacy::Allowed
        } else {
            Privacy::Refused(modifiers)
        }
    }

    /// What the syntax at the cursor permits.
    ///
    /// One function instead of an `if` at each of the three rungs (the resolved member, the derived
    /// one, the name list): a gate spelled three times will one day be spelled differently.
    fn at(context: &Context, modifiers: &'a Modifiers<'a>) -> Self {
        if context.allows_private() {
            Privacy::Allowed
        } else {
            Privacy::Refused(modifiers)
        }
    }

    /// Whether this declaration may be the answer.
    ///
    /// Non-methods pass: only a method has visibility, and a constant reached through a namespace
    /// is not this gate's concern.
    fn admits(self, graph: &Graph, id: DeclarationId) -> bool {
        match self {
            Privacy::Allowed => true,
            Privacy::Refused(modifiers) => !is_private(graph, modifiers, id),
        }
    }

    /// The same refusal over a candidate list, which is what the name rung is.
    ///
    /// **Needed on the list too, or the refusal has a side door.** A private declaration the
    /// ancestor walk declined still matches by name, so gating only the precise rung hands the same
    /// `def` back one tier down, as a *Guessed* card for a method Ruby raises on. If nothing is
    /// left, nothing is the answer: `RSpec.describe` has no public `describe` (rspec-core defines
    /// it dynamically), so the right card is none.
    fn keep(self, graph: &Graph, candidates: Vec<DeclarationId>) -> Vec<DeclarationId> {
        match self {
            Privacy::Allowed => candidates,
            Privacy::Refused(modifiers) => candidates
                .into_iter()
                .filter(|id| !is_private(graph, modifiers, *id))
                .collect(),
        }
    }
}

/// Whether this declaration is private, by rubydex's record and by the source it was read from.
///
/// - **The same test as [`completion::reachable`](super::completion), no more.** Ruby's five
///   always-private names are `completion`'s list because they are about what may be *offered*.
///   Applying them here would refuse `Foo.new`, which resolves to `Foo#initialize`, the redirect
///   navigation exists for. `holds_private` skips a redirect for that reason.
/// - **The second half repairs the record, not the rule.** The record is wrong for exactly one
///   shape ([`Modifiers`]). The order keeps it cheap: a declaration rubydex calls public is
///   answered on the first line without reading a byte.
/// - **`types::from_nil` asks it too**, about the member a `T?` reaches on `nil`. The call there is
///   written with a receiver, so a private member is one Ruby raises on.
pub(super) fn is_private(graph: &Graph, modifiers: &Modifiers<'_>, id: DeclarationId) -> bool {
    matches!(
        graph.visibility(&id),
        // rubydex treats `module_function`'s instance copy as private, as Ruby does.
        Some(Visibility::Private | Visibility::ModuleFunction)
    ) && modifiers.confirm(graph, id)
}

/// Whether a precise resolution rests on a private declaration, making it worth reading the syntax.
///
/// **The redirect is exempt, and that is the main risk here.** `Foo.new` resolves to
/// `Foo#initialize`, which Ruby makes private by name, so a gate without the exemption would refuse
/// every constructor, at a receiver that is written and never `self`.
fn holds_private(graph: &Graph, modifiers: &Modifiers<'_>, resolution: &Resolution) -> bool {
    !resolution.redirected
        && resolution
            .declarations
            .iter()
            .any(|id| is_private(graph, modifiers, *id))
}

/// rubydex's visibility record, and the one question it answers wrongly.
///
/// - **The bug.** A bare `private` is a **statement**, and rubydex applies it to its body until the
///   body ends. A block is not a body to rubydex: `class_methods do … private … end` sets the
///   *module's* default visibility, so every `def` below the block (public methods, in an ordinary
///   Rails concern) is recorded private. An application calls `HasCustomFields#upsert_custom_fields` on
///   explicit receivers, and the gate would refuse all of them.
/// - **Ruby agrees for one kind of block and not the other, and the syntax cannot tell which.** A
///   bare `private` sets visibility on the *cref*. A plain iterator block shares its cref, so
///   `[1].each { private }` really privatises the `def` below. `module_eval` and `class_eval` give
///   the block its own cref, so `class_methods do`, `concerning`, `included do`, `Class.new do` and
///   RSpec groups do not. Telling them apart needs to know what a gem's method does with the block.
/// - **The rule chosen is the one that does not refuse: a bare modifier governs only its own body,
///   and a block body is a body.** Right for eval forms, wrong for plain iterators, and measured
///   over the corpora, every leaking block is an eval form (mostly `class_methods do`). A refusal
///   is an assertion, and it should not rest on the reading that cannot be checked.
/// - **The other direction is not repaired.** A `public` inside a block escapes just as far, so a
///   truly private `def` below may be recorded public. That is a missing refusal, not a false one,
///   and is left alone: this type stops the gate asserting what the source does not say, never
///   asserts more than rubydex did.
pub struct Modifiers<'a> {
    /// The declaring document's own text: the open buffer if there is one, which is why this is the
    /// closure `types::Sources` carries, not a file read.
    read: &'a types::ReadText<'a>,
    /// Each text's walk, kept across requests by the text it read
    /// ([`types::HeldExits::escapes`]): what the walk finds depends on nothing else.
    held: &'a types::HeldExits,
    /// One answer per declaring document, per request, in the graph's offsets. The gate rarely
    /// refuses, but the name rung passes a *list*, and several private candidates in one file
    /// would otherwise mean several reads.
    escapes: RefCell<HashMap<UriId, HashSet<u32>>>,
}

impl<'a> Modifiers<'a> {
    #[must_use]
    pub fn new(read: &'a types::ReadText<'a>, held: &'a types::HeldExits) -> Self {
        Self {
            read,
            held,
            escapes: RefCell::new(HashMap::new()),
        }
    }

    /// Whether rubydex's `private` for this declaration survives a reread of the source.
    ///
    /// - **One truly private `def` is enough.** A declaration is every `def` of one name on one
    ///   owner, and Ruby's answer is whichever ran last. The other reading (public if any
    ///   definition escaped) would let one leaked `def` unlock a method another file makes private.
    /// - **Public because `completion` asks too.** One wrong record read by two surfaces must be
    ///   fixed for both, or a jump lands on a name the list will not offer.
    #[must_use]
    pub fn confirm(&self, graph: &Graph, id: DeclarationId) -> bool {
        let Some(declaration) = graph.declarations().get(&id) else {
            return true;
        };
        let mut any = false;
        for definition in declaration
            .definitions()
            .iter()
            .filter_map(|definition_id| graph.definitions().get(definition_id))
        {
            any = true;
            if !self.escaped(graph, definition) {
                return true;
            }
        }
        !any
    }

    /// Whether this one `def` is recorded private only because a modifier escaped a block.
    ///
    /// Public for `symbols`, which renders a *definition* (an outline row is one `def`), while
    /// every other reader asks about a name on an owner.
    #[must_use]
    pub fn escaped(&self, graph: &Graph, definition: &Definition) -> bool {
        let mut escapes = self.escapes.borrow_mut();
        let found = escapes
            .entry(*definition.uri_id())
            .or_insert_with(|| self.reread(graph, definition.uri_id()));
        found.contains(&definition.offset().start())
    }

    /// Every such `def` in one document, in graph coordinates.
    ///
    /// **Nothing readable means nothing escaped**, the direction every refusal here falls: a
    /// document with no readable text keeps rubydex's record.
    fn reread(&self, graph: &Graph, uri_id: &UriId) -> HashSet<u32> {
        let Some(document) = graph.documents().get(uri_id) else {
            return HashSet::new();
        };
        // **A signature has no blocks to escape from.** RBS has its own `private` inside a
        // declaration no block can open, and rubydex indexes it with a different indexer. Nothing
        // to repair, and parsing Ruby's core signatures as Ruby to find that out would put the
        // largest files on the path that must stay cheap.
        if document.uri().ends_with(".rbs") {
            return HashSet::new();
        }
        let Some((text, rebase)) = (self.read)(document.uri()) else {
            return HashSet::new();
        };
        self.held
            .escapes(document.uri(), &text, || escapes_in(&text))
            .iter()
            // The text read is the buffer and the offsets compared are the graph's, which differ
            // once somebody types. A `def` in the part an edit moved has no graph offset, and the
            // gate is not asked about such a `def`.
            .filter_map(|at| rebase.to_graph(*at))
            .collect()
    }
}

/// Every `def` in `text` recorded private only because a modifier escaped a block, in `text`'s own
/// offsets ([`Escapes`]).
fn escapes_in(text: &str) -> Vec<u32> {
    let parsed = ruby_prism::parse(text.as_bytes());
    let mut walk = Escapes::default();
    walk.visit(&parsed.node());
    walk.found
        .into_iter()
        .filter(|(_, name)| !walk.named.contains(name))
        .map(|(at, _)| at)
        .collect()
}

/// The walk behind [`Modifiers`]: every `def` the two readings of a bare modifier disagree about.
///
/// **No explicit stack: the visitor's recursion is one.** A construct that opens a body saves this
/// frame, walks with a fresh one and restores it. A block walks with *this* frame and restores only
/// half on the way out, which is exactly the disagreement.
#[derive(Default)]
struct Escapes {
    /// The body being walked. `.0` is rubydex's reading (a block writes through to the surrounding
    /// body); `.1` is the reading where a block's settings die with it.
    frame: (bool, bool),
    /// `(offset of the def, its name)`, in the parsed text.
    found: Vec<(u32, String)>,
    /// Every name a visibility call *named* (`private :foo`, `private def foo`). rubydex reads both
    /// correctly and no block can leak them, so such a `def` is private for a reason this walk must
    /// not overturn.
    named: HashSet<String>,
}

impl Escapes {
    /// A body of its own, for a construct that resets the default visibility to public.
    fn body(&mut self, visit: impl FnOnce(&mut Self)) {
        let outer = std::mem::take(&mut self.frame);
        visit(self);
        self.frame = outer;
    }

    fn set(&mut self, private: bool) {
        self.frame = (private, private);
    }

    fn remember(&mut self, arguments: &ArgumentsNode<'_>) {
        for argument in arguments.arguments().iter() {
            let name = if let Some(symbol) = argument.as_symbol_node() {
                symbol.unescaped().to_vec()
            } else if let Some(string) = argument.as_string_node() {
                string.unescaped().to_vec()
            } else if let Some(def) = argument.as_def_node() {
                def.name().as_slice().to_vec()
            } else {
                continue;
            };
            self.named
                .insert(String::from_utf8_lossy(&name).into_owned());
        }
    }
}

impl<'pr> Visit<'pr> for Escapes {
    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        self.body(|walk| ruby_prism::visit_class_node(walk, node));
    }

    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        self.body(|walk| ruby_prism::visit_module_node(walk, node));
    }

    fn visit_singleton_class_node(&mut self, node: &SingletonClassNode<'pr>) {
        self.body(|walk| ruby_prism::visit_singleton_class_node(walk, node));
    }

    /// The line the whole type is about: the block hands back `.0` and drops `.1`.
    fn visit_block_node(&mut self, node: &BlockNode<'pr>) {
        let outer = self.frame;
        ruby_prism::visit_block_node(self, node);
        self.frame = (self.frame.0, outer.1);
    }

    /// A `private` in a method body is a runtime call, not a modifier on this file's text, so the
    /// body gets its own frame and never writes into the class's.
    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        let (transparent, scoped) = self.frame;
        if node.receiver().is_none() && transparent && !scoped {
            self.found.push((
                node.location().start_offset() as u32,
                String::from_utf8_lossy(node.name().as_slice()).into_owned(),
            ));
        }
        self.body(|walk| ruby_prism::visit_def_node(walk, node));
    }

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        if node.receiver().is_none() {
            let name = node.name();
            // **The argument list decides, not `bare`, because a block is not an argument.**
            // `private do … end` parses, and Ruby reads it as the modifier.
            match (name.as_slice(), node.arguments()) {
                (b"private" | b"module_function", None) => return self.set(true),
                (b"public" | b"protected", None) => return self.set(false),
                (b"private" | b"module_function" | b"private_class_method", Some(arguments)) => {
                    self.remember(&arguments)
                }
                _ => {}
            }
        }
        ruby_prism::visit_call_node(self, node);
    }
}

/// Whether a `def` was written inside a block, for the one hit where that decides the answer.
///
/// - **rubydex has no namespace for a block body.** Its nesting stack holds lexical scopes,
///   `Class.new` owners and methods, so `Story.class_eval do … def find_by … end` and
///   `RSpec.describe "x" do … def create … end` push nothing and the `def` is filed on the
///   surrounding scope, `Object` at a file's top level. `Object` is an ancestor of every receiver,
///   so one such `def` answers for the whole workspace. [`ROOTS`] and [`resolve_call`]'s root arm
///   are the only place this is asked.
/// - **A place decides, not a name, and the census makes that safe.** Over the corpora's own files,
///   nearly every member filed on `Object` was written inside a block, and nearly every such name
///   is block-only; only a handful of names appear both ways. Those are answered conservatively:
///   **one `def` written as a body of its own keeps the root answer**, the direction every refusal
///   here falls.
/// - **It costs a reread**, like [`Modifiers`]: the declaring document (buffer if open), parsed
///   once per document per request, since several `def`s of one declaration can share a file. A
///   jump that never lands on a root pays nothing.
/// - **Nothing readable means nothing inside a block.** An unreadable document, and an `.rbs` (no
///   blocks, different indexer), keep rubydex's record.
pub struct Blocks<'a> {
    /// The declaring document's own text, read as [`Modifiers`] reads it.
    read: &'a types::ReadText<'a>,
    /// One parse per declaring document, per request.
    inside: RefCell<HashMap<UriId, HashSet<u32>>>,
}

impl<'a> Blocks<'a> {
    #[must_use]
    pub fn new(read: &'a types::ReadText<'a>) -> Self {
        Self {
            read,
            inside: RefCell::new(HashMap::new()),
        }
    }

    /// Whether **every** `def` of this declaration is written inside a block.
    ///
    /// - **The opposite reading from [`Modifiers::confirm`], on purpose.** That refuses on any one
    ///   truly private `def`, because Ruby's answer is whichever ran last. Here one `def` written
    ///   as a body of its own is a real root member, and the declaration keeps its answer. The
    ///   other reading would withdraw a name everywhere because of one monkey patch in the bundle.
    ///   The conservative direction keeps an answer rather than inventing a refusal.
    /// - **A declaration with no definitions is not block-written**: nothing read, nothing refused.
    fn only_in_blocks(&self, graph: &Graph, id: DeclarationId) -> bool {
        let Some(declaration) = graph.declarations().get(&id) else {
            return false;
        };
        let mut any = false;
        for definition in declaration
            .definitions()
            .iter()
            .filter_map(|definition_id| graph.definitions().get(definition_id))
        {
            any = true;
            if !self.written_in_a_block(graph, definition) {
                return false;
            }
        }
        any
    }

    /// Whether this one `def` has a block between it and the body it is filed on.
    fn written_in_a_block(&self, graph: &Graph, definition: &Definition) -> bool {
        let mut inside = self.inside.borrow_mut();
        let found = inside
            .entry(*definition.uri_id())
            .or_insert_with(|| self.reread(graph, definition.uri_id()));
        found.contains(&definition.offset().start())
    }

    /// Every such `def` in one document, in graph coordinates.
    fn reread(&self, graph: &Graph, uri_id: &UriId) -> HashSet<u32> {
        let Some(document) = graph.documents().get(uri_id) else {
            return HashSet::new();
        };
        // **A signature has no blocks**, and parsing Ruby's core signatures as Ruby to find that
        // out would put the largest files on this path. Same as [`Modifiers::reread`].
        if document.uri().ends_with(".rbs") {
            return HashSet::new();
        }
        let Some((text, rebase)) = (self.read)(document.uri()) else {
            return HashSet::new();
        };
        let parsed = ruby_prism::parse(text.as_bytes());
        let mut walk = InBlocks::default();
        walk.visit(&parsed.node());
        // The text read is the buffer and the offsets compared are the graph's. A `def` an
        // unindexed edit moved has no graph offset, and the gate is not asked about such a `def`.
        walk.found
            .iter()
            .filter_map(|at| rebase.to_graph(*at))
            .collect()
    }
}

/// The walk behind [`Blocks`]: every `def` a block encloses, within the body it is filed on.
///
/// - **A `class`, `module` or `class << self` resets the count**, so this asks rubydex's own
///   question: a `def` inside a block inside a class body is filed on the *class*, never on a root,
///   so the root arm never asks about it.
/// - **No explicit stack**: the visitor's recursion is one, as in [`Escapes`], with a depth instead
///   of a pair of flags.
#[derive(Default)]
struct InBlocks {
    /// How many blocks enclose the visited node, within this body.
    depth: u32,
    /// The offset of every `def` with at least one enclosing block, in the parsed text.
    found: Vec<u32>,
}

impl InBlocks {
    /// A body of its own, for a construct rubydex gives a namespace to.
    fn body(&mut self, visit: impl FnOnce(&mut Self)) {
        let outer = std::mem::take(&mut self.depth);
        visit(self);
        self.depth = outer;
    }
}

impl<'pr> Visit<'pr> for InBlocks {
    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        self.body(|walk| ruby_prism::visit_class_node(walk, node));
    }

    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        self.body(|walk| ruby_prism::visit_module_node(walk, node));
    }

    fn visit_singleton_class_node(&mut self, node: &SingletonClassNode<'pr>) {
        self.body(|walk| ruby_prism::visit_singleton_class_node(walk, node));
    }

    fn visit_block_node(&mut self, node: &BlockNode<'pr>) {
        self.depth += 1;
        ruby_prism::visit_block_node(self, node);
        self.depth -= 1;
    }

    /// `->() { def x; end }` files the `def` exactly as `each do` does.
    fn visit_lambda_node(&mut self, node: &LambdaNode<'pr>) {
        self.depth += 1;
        ruby_prism::visit_lambda_node(self, node);
        self.depth -= 1;
    }

    /// **A `def` body keeps the count**, unlike [`Escapes`], which gives a `def` its own frame. A
    /// nested `def` is filed on the same owner as the outer one, so a block above both encloses
    /// both.
    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        if self.depth > 0 {
            self.found.push(node.location().start_offset() as u32);
        }
        ruby_prism::visit_def_node(self, node);
    }
}

/// Whether an answer the ancestor walk found on one of [`ROOTS`] is really a root member.
///
/// **`None` trusts rubydex**, for callers with no buffer to read: `resolve` and everything behind
/// it (`references`, `rename`, the two hierarchies). They ask *where is this used*, not *what does
/// this call reach*, and are neither fenced nor privacy-gated for the same reason.
///
/// An answer from the extend repair is never on a root and is never withdrawn here.
pub(super) fn declared_on_the_root(
    graph: &Graph,
    blocks: Option<&Blocks<'_>>,
    answer: DeclarationId,
) -> bool {
    if !owned_by_a_root(graph, answer) {
        return true;
    }
    blocks.is_none_or(|blocks| !blocks.only_in_blocks(graph, answer))
}

fn resolve_call(
    graph: &Indexed,
    reference: &MethodRef,
    fence: environment::Fence<'_>,
    privacy: Privacy<'_>,
    blocks: Option<&Blocks<'_>>,
) -> Resolution {
    let Some(member) = member_name(graph, *reference.str()) else {
        return Resolution::precise(Vec::new());
    };
    let member_id = StringId::from(&member);

    // Whether `self` here is a **class object**: `Foo.bar`, or a bare call written as a statement
    // of a class or module body. rubydex gives both the singleton class as receiver, and a bare
    // call inside a `def` the class itself, so this is one question with one answer, not a syntax
    // test. The name rung's filter depends on it. **Never certain here**: a bare word in a block
    // written into the body gets the same receiver, and only [`typed`], which reads the buffer, can
    // tell the two apart ([`proves_the_class_object`]). Every caller without the buffer keeps the
    // narrowing that never empties.
    let mut class_object = ClassObject::No;

    // A receiver rubydex could name is the only precise path. For `Foo.bar` and for an implicit
    // `self` in a class body, it resolves to the *singleton* class, where singleton methods live.
    if let Some(receiver) = reference.receiver()
        && let Some(owner) = constant_named(graph, receiver)
    {
        if member == NEW
            && let Some(constructor) = constructor(graph, blocks, owner)
        {
            // **The attached class, not the singleton the call was written on.** The answer is
            // `Foo#initialize`, an *instance* method: a subclass's own `initialize` is a member of
            // an ordinary descendant of `Foo`, not of `Foo::<Foo>`. The declaration's owner would
            // not do either: a class writing no `initialize` inherits one, and `Object`'s
            // descendants are every class. The one place where what the call was written on and
            // what the answer is about are different namespaces.
            return Resolution::redirected(constructor)
                .on(attached_class(graph, owner).unwrap_or(owner));
        }

        // **Past a member only the suite loads** ([`find_loaded_member`]): the one fence on an
        // ordinary ancestor hit, which is otherwise precise and never fenced.
        match find_loaded_member(graph, fence, owner, member_id) {
            // A hit on one of the three [`ROOTS`] is worth a second question, because of **Ruby's
            // method resolution order**: a module `extend`ed onto a class object sits in the
            // singleton chain above `Class`, `Module` and `Object`, so it should be reached first.
            // Asking it only after the ancestor walk inverts that, which shows only when something
            // lands on a root.
            //
            // Something does. `Object` is an ancestor of every class object, so a top-level `def`
            // in any file answers here for every class, and a `def` written inside a **block** is
            // recorded the same way (rubydex has no notion of a block; `describe "x" do` pushes
            // nothing). Unrepaired, an RSpec helper named `validate` would shadow
            // `ActiveModel::Validations::ClassMethods#validate` in every model.
            //
            // Where the extend repair finds nothing, the root answer stands, so a genuine top-level
            // `def` called from a class body resolves unchanged.
            Ok(found) if owned_by_a_root(graph, found) => {
                let answer = extended_member(graph, fence, owner, member_id).unwrap_or(found);
                // **The one precise answer the test-tree fence applies to, and it must be applied
                // here, not in `loadable_from`.** A root hit is a hit on every receiver, so a name
                // only the suite declares (via `Object`, most of what a spec's top-level `def`
                // produces) would answer confidently for application code that could never call it:
                // a migration's `execute` landing in some plugin's spec.
                //
                // **The fall-through is the name rung, not silence.** The list below holds the
                // gem's real `ActiveRecord::Migration#execute`, and `resolve_typed` fences the
                // spec's copy out again, so the reader gets a *Guessed* card naming the right
                // method instead of a *Resolved* card naming the wrong one.
                if fence.loadable_on_a_root(graph, answer)
                    && declared_on_the_root(graph, blocks, answer)
                    && privacy.admits(graph, answer)
                {
                    return Resolution::precise(vec![answer]).on(owner);
                }
                return by_name(
                    graph,
                    &member,
                    ClassObject::named(graph, owner).unproven(),
                    privacy,
                );
            }
            // **A private hit is refused, and the walk is not resumed above it.** Ruby's lookup
            // stops at the first match and raises; continuing to the next ancestor would answer
            // with a method the interpreter never reaches, a second wrong answer.
            Ok(found) if privacy.admits(graph, found) => {
                return Resolution::precise(vec![found]).on(owner);
            }
            Ok(_) | Err(FindMemberError::MemberNotFound) => {}
            Err(error) => {
                tracing::debug!("receiver {owner} is not searchable: {error:?}");
            }
        }
        if let Some(found) = extended_member(graph, fence, owner, member_id)
            && privacy.admits(graph, found)
        {
            return Resolution::precise(vec![found]).on(owner);
        }
        class_object = ClassObject::named(graph, owner).unproven();
    }

    // No receiver, a receiver without the method, or one that has it but may not call it.
    by_name(graph, &member, class_object, privacy)
}

/// Every declaration whose name ends in this method: the whole name rung.
///
/// `Person#shout()` and `Person::<Person>#shout()` both contain `#shout()`, and nothing else does.
/// A separate function because two callers reach it: the tail of [`resolve_call`] (no receiver
/// named), and its root arm (a receiver named, but the answer is a `def` the application never
/// loads). [`typed`] asks again where the syntax proves a class object ([`on_a_proven_class_object`]).
fn by_name(
    graph: &Indexed,
    member: &str,
    class_object: ClassObject,
    privacy: Privacy<'_>,
) -> Resolution {
    let candidates = graph.members_named(member);
    let candidates = match class_object {
        ClassObject::No => candidates.to_vec(),
        // **A root's instance method is the one class-owned candidate a class object reaches.**
        // `Object` is on every class object's chain, so the walk found it and a gate refused it: a
        // `def` written in a block, a test tree, privacy. It stays the guess it was, behind any
        // module's, and those gates still apply to it.
        ClassObject::Certainly => match reachable_on_a_class_object(graph, candidates) {
            kept if kept.is_empty() => candidates
                .iter()
                .copied()
                .filter(|&candidate| owned_by_a_root(graph, candidate))
                .collect(),
            kept => kept,
        },
        ClassObject::Perhaps => match reachable_on_a_class_object(graph, candidates) {
            kept if kept.is_empty() => candidates.to_vec(),
            kept => kept,
        },
    };
    Resolution {
        // **Last, after the class-object filter.** Both class-object arms fall back to a wider
        // list when nothing survives, so the two orders give different lists, and `typed`
        // reapplies this refusal to a list this function returned, which is only valid if it is the
        // outermost step here too.
        declarations: privacy.keep(graph, candidates),
        precise: false,
        redirected: false,
        derivation: Derivation::default(),
        // This *is* the rung below a receiver: the list is a guess, so there is none to report.
        receiver: None,
    }
}

/// The three namespaces that are an ancestor of everything.
///
/// A member found on one of them is found on *every* receiver, so a hit there says little, and
/// rubydex's one mis-attribution costs a lot. `BasicObject` is included for completeness: nothing
/// reopens it, but a rule naming two of the three roots would have a hole.
const ROOTS: [&str; 3] = ["Object", "Kernel", "BasicObject"];

/// Whether the member the ancestor walk found is declared on one of [`ROOTS`].
///
/// **The instance side only, on evidence.** A `def self.x` inside a block looks like the other half
/// of the object model but is not: rubydex has no namespace for the block, so the body has no
/// `self` to hang a singleton on and the member arrives as an ordinary `Object#x`. Widening this to
/// the roots' singletons changed nothing against the fixture below.
fn owned_by_a_root(graph: &Graph, found: DeclarationId) -> bool {
    ROOTS.iter().any(|root| owned_by(graph, found, root))
}

/// A member of a module the receiver's class object really `extend`s, where rubydex missed the
/// edge.
///
/// - **A repair, not a convention.** Every `extend` rubydex linearizes is found by the ordinary
///   ancestor search, which runs first. What reaches here is the one case it misses: an `extend`
///   indexed after its namespace was declared (see [`extends_written_on`]). Rails concerns' class
///   methods are **declared** by `workspace/rails/concerns.rs` onto every including class, so the
///   ordinary walk finds them and this module knows no Rails word.
/// - **Asked only after the ordinary search found nothing**, so a method a class really declares is
///   never displaced by one it extends.
/// - **Past a member only the suite loads**, as the ordinary walk steps past one
///   ([`find_loaded_member`]): a spec reopening a module to `extend` a helper is the late `extend`
///   this repair exists for.
fn extended_member(
    graph: &Graph,
    fence: environment::Fence<'_>,
    singleton: DeclarationId,
    member: StringId,
) -> Option<DeclarationId> {
    extended_modules(graph, singleton)
        .into_iter()
        .filter_map(|found| declared_by(graph, found.module, member))
        .find(|&found| fence.loadable(graph, found))
}

/// What the module itself declares, deliberately **not** its ancestors.
///
/// `extend M` really installs `M`'s ancestors' instance methods too, so an ancestor walk is the
/// right reading of Ruby but the wrong reading of the graph: rubydex records an `include` written
/// **inside a `def`** as a mixin of the enclosing namespace, and that is how the modules reaching
/// here are written:
///
/// ```ruby
/// module ClassMethods
///   def has_secure_password(...)
///     include ActiveModel::Validations   # the *record's*, when the macro is called
///   end
/// end
/// ```
///
/// In Rails' core gems, such in-`def` mixins far outnumber module-body ones, and every one means
/// the class the macro was called on. Following them made `Category.valid?` (which raises in Ruby)
/// answer `ActiveModel::Validations#valid?`, and put that module's *instance* methods into a class
/// object's completion list.
fn declared_by(graph: &Graph, module: DeclarationId, member: StringId) -> Option<DeclarationId> {
    graph
        .declarations()
        .get(&module)?
        .as_namespace()?
        .member(&member)
        .copied()
}

/// One module a class object `extend`s, and how far out the class that extended it sits.
pub(super) struct Extension {
    /// The module, where the members are declared.
    pub module: DeclarationId,
    /// How many classes the receiver's chain passes before the one whose body wrote the `extend`:
    /// the module's position in Ruby's *singleton* chain.
    ///
    /// Not the module's step in the instance ancestor list, and the ranking depends on that. A
    /// Rails model's instance ancestors are forty rungs of concerns while its singleton chain is
    /// five classes, so a module at instance step 12 would rank its members *below* `Object`'s.
    /// Counting only classes gives the position of the singleton the `extend` really landed on.
    pub step: usize,
}

/// Every module a class object `extend`s that rubydex did not linearize, nearest first.
///
/// Shared by both halves of the repair: [`extended_member`] takes the first module answering one
/// member, and `completion` collects every member of all of them, so [`extends_written_on`]'s table
/// is stated once. Empty is the ordinary case: a receiver that is not a class object, or a chain
/// whose `extend`s the graph already holds.
pub(super) fn extended_modules(graph: &Graph, singleton: DeclarationId) -> Vec<Extension> {
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
    let mut found = Vec::new();
    let mut classes = 0;
    // Ancestor order: the linearization of the `include`s, and the order the `extend`s ran, since
    // each happens when its `include` does.
    for ancestor in namespace.ancestors() {
        let Ancestor::Complete(id) = ancestor else {
            continue;
        };
        // **Only where the receiver's singleton chain really passes through this declaration's
        // singleton**: the attached declaration and the classes above it. An `extend` in an
        // *included module* lands on that module's singleton, never the includer's, so walking it
        // would install methods Ruby does not.
        if *id == attached || !is_module(graph, *id) {
            for extended in extends_written_on(graph, *id) {
                found.push(Extension {
                    module: extended,
                    step: classes,
                });
            }
        }
        if is_module(graph, *id) {
            continue;
        }
        classes += 1;
    }
    found
}

/// Whether a declaration is a module, whose instances are some class's.
pub(crate) fn is_module(graph: &Graph, declaration: DeclarationId) -> bool {
    matches!(
        graph.declarations().get(&declaration),
        Some(Declaration::Namespace(Namespace::Module(_)))
    )
}

/// The modules one declaration's own bodies `extend`, resolved by name.
///
/// **rubydex reads `extend` (qualified or not, Ruby or RBS) and attaches it to the singleton as
/// Ruby does. What it loses is timing.** An `extend` indexed together with its namespace is linked.
/// One indexed **after** the namespace was declared never is: upstream's `handle_definition_unit`
/// schedules a singleton's ancestors only for a declaration it creates.
///
/// | the `extend` arrives in | rubydex links it |
/// | --- | --- |
/// | any file indexed with the namespace | yes |
/// | an edit to the one file declaring the namespace | yes — the edit re-creates the declaration |
/// | an edit to one of several files declaring it | **no** |
/// | a new file reopening it | **no** |
/// | a new signature in `sig/` reopening it | **no** |
///
/// `completion`'s `an_extend_written_after_the_first_resolve_is_still_read` holds the last four
/// rows; the three **no** rows fail without this repair.
///
/// A repair, not a second walk, and it only ever adds: [`extended_member`] reaches it **after** the
/// ordinary search came back empty, and `completion`'s `Extended` drops every member the ordinary
/// walk already offers. A linearized `extend` is found by the search and never gets here, so
/// nothing is counted twice.
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

/// The `const_missing` that hands every constant a class lacks to the top level: Ruby's
/// `Delegator`, which `SimpleDelegator` and `DelegateClass` inherit ([`constant_named`]).
const DELEGATED_CONSTANTS: &str = "Delegator::<Delegator>#const_missing()";

/// The declaration a constant's name resolved to, and where rubydex resolved nothing, the
/// top-level one `Delegator` hands the name to.
///
/// - **rubydex follows Ruby's lookup:** the nesting, then the ancestors of the class the name is
///   written in. A class under `BasicObject` has no `Object` among them, so a top-level `Current`
///   written in `class Presenter < SimpleDelegator` resolves to nothing there.
/// - **Ruby asks `const_missing` next**, and `Delegator` defines it as `::Object.const_get(n)`: the
///   top level after all ([`DELEGATED_CONSTANTS`]). Only that one; another `const_missing` may
///   answer anything.
/// - **Why it shows:** Ruby's library is indexed after the workspace, so the first resolve, which
///   does not know `SimpleDelegator` yet, falls back to the top level and answers. An edit
///   re-resolves the file against the whole chain, and the constant was lost.
/// - **A class object's name** (`Current.account`'s receiver, rubydex's `<Current>`) is the
///   singleton of the constant it is attached to, resolved the same way.
pub(crate) fn constant_named(graph: &Graph, name_id: NameId) -> Option<DeclarationId> {
    if let Some(found) = graph.name_id_to_declaration_id(name_id) {
        return Some(*found);
    }
    let name = graph.names().get(&name_id)?;
    if let ParentScope::Attached(attached) = name.parent_scope() {
        return types::singleton_of(graph, constant_named(graph, *attached)?);
    }
    let written_in = graph.name_id_to_declaration_id((*name.nesting())?)?;
    // `Presenter.const_missing` is found up the class side of `Presenter`'s own ancestors, and a
    // class with nothing on its class side has no singleton to ask: so each ancestor's is asked,
    // in order, and the first that declares one decides.
    let handler = graph
        .declarations()
        .get(written_in)?
        .as_namespace()?
        .ancestors()
        .iter()
        .filter_map(|ancestor| match ancestor {
            Ancestor::Complete(id) => graph.declarations().get(id),
            Ancestor::Partial(_) => None,
        })
        .find_map(|ancestor| {
            let handler = format!(
                "{}::<{}>#const_missing()",
                ancestor.name(),
                last_segment(ancestor.name())
            );
            graph
                .declarations()
                .get(&DeclarationId::from(handler.as_str()))
                .map(|_| handler)
        })?;
    (handler == DELEGATED_CONSTANTS)
        .then(|| types::declared(graph, &constant_path(graph, name_id)?))
        .flatten()
}

/// The last segment of a constant's path: `Delegator` for `Delegator`, `Thing` for `A::Thing`, the
/// name rubydex gives a class's singleton (`A::Thing::<Thing>`).
fn last_segment(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

/// A name's whole path, `A::B::C`, from the chain of parent scopes rubydex interned it as.
///
/// `Random::Formatter` is two `Name`s, each holding only its last segment, so a lookup by name must
/// reassemble them. A leading `::` is dropped: absolute and top-level paths name the same
/// declaration, and declarations are keyed without it.
fn constant_path(graph: &Graph, name_id: NameId) -> Option<String> {
    let name = graph.names().get(&name_id)?;
    let segment = graph.strings().get(name.str())?.as_str();
    Some(match name.parent_scope() {
        ParentScope::Some(parent) => format!("{}::{segment}", constant_path(graph, *parent)?),
        // `Foo::<Foo>` is rubydex's singleton spelling, never something an `extend` names; `::Foo`
        // and `Foo` are one declaration.
        ParentScope::Attached(_) | ParentScope::None | ParentScope::TopLevel => segment.to_owned(),
    })
}

/// A constant resolved the way Ruby resolves one: outwards through the nesting, then the top level.
///
/// [`types::declared`] is the lookup and this is the lexical half around it, the same pair `types`'
/// guess rung uses. There is no reference to read (the one rubydex recorded is the one that
/// failed), so the scoping rule is spelled out.
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

/// The candidates a call on a class object could possibly mean, out of everything sharing the name.
///
/// The other half of the class-object answer, and the one users see first: the name-based list is
/// every declaration ending in that name, so `scope` in a model body would be headed by a *routing*
/// method, and `belongs_to` by a migration method.
///
/// - **A class's instance method is never it.** A class object answers from its singleton chain
///   (its ancestors' singletons, anything `extend`ed onto it, plus `Class`, `Module`, `Object` and
///   `Kernel` instance methods), and rubydex puts all of those in the singleton's ancestors, which
///   the search above already walked. So a surviving candidate owned by a `class` is provably
///   unreachable. One owned by a `module` must be kept: a module's instance method reached through
///   an `extend` is the right answer where the walk failed.
/// - **What an empty list means is [`ClassObject`]'s to say, not this filter's.** Where the class
///   object is certain, only a root's method may still be the answer, and otherwise nothing:
///   a `Settings::General.app_domain`, made by a gem's macro, was sent to the one public
///   `app_domain` left, a configuration setting, once the privacy gate refused a private one in an
///   unrelated class. Where it is not certain, the whole list is still a guess worth making.
fn reachable_on_a_class_object(graph: &Graph, candidates: &[DeclarationId]) -> Vec<DeclarationId> {
    candidates
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
        .collect()
}

/// What the name rung may assume about `self`, which decides the candidates it may name
/// ([`reachable_on_a_class_object`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClassObject {
    /// Not a class object: an instance, or a receiver nobody named. Every candidate stays.
    No,
    /// The class object of a class or module, for certain: rubydex named it, and the syntax proves
    /// nothing between the call and its body can change what `self` is
    /// ([`proves_the_class_object`]). A candidate a class other than a root owns is dropped, even
    /// when that leaves nothing.
    Certainly,
    /// A class object by rubydex's reading that nothing proves: a caller without the buffer, a bare
    /// word in a block written into the body, which whoever receives the block may run against
    /// anything ([`cursor::Cursor::in_a_closure`]), or a singleton attached to something other
    /// than a class or module (a constant holding a value, or one no file defines). A candidate a
    /// class owns is dropped unless that would leave nothing.
    Perhaps,
}

impl ClassObject {
    /// rubydex's receiver, as far as the graph proves it; the syntax is the caller's to add.
    ///
    /// rubydex gives a singleton to constants that hold a value too: the test suite's
    /// `MYSTERY.upcase` is asked on one, where `MYSTERY` is declared a class no file defines. Only a
    /// class's or a module's singleton can be certainly a class object.
    fn named(graph: &Graph, owner: DeclarationId) -> Self {
        match attached_class(graph, owner).map(|attached| graph.declarations().get(&attached)) {
            None => Self::No,
            Some(Some(Declaration::Namespace(Namespace::Class(_) | Namespace::Module(_)))) => {
                Self::Certainly
            }
            Some(_) => Self::Perhaps,
        }
    }

    /// What a caller may assume without the buffer, which cannot tell a statement of the body from
    /// a bare word in a block of it: never certain.
    fn unproven(self) -> Self {
        match self {
            Self::Certainly => Self::Perhaps,
            other => other,
        }
    }
}

/// rubydex's spelling of the two members this file redirects between.
const NEW: &str = "new()";
const INITIALIZE: &str = "initialize()";

/// `Foo#initialize`, for a cursor on the `new` in `Foo.new`.
///
/// `Foo.new` really is `Class#new`, so resolving it exactly is correct but useless: the constructor
/// a human means is `Foo#initialize`. Hover has the same problem
/// (`Class#new(*args, **kwargs, &block)` and RDoc about `allocate`), and `Foo.new(` would complete
/// against the wrong signature.
///
/// It stays out of the way in three cases where the honest answer is better:
///
/// 1. a class that writes its own `def self.new`, which is reached instead;
/// 2. a class whose only `initialize` is the empty one every object inherits: nothing to show, so
///    `Class#new` stands;
/// 3. a class whose only `initialize` is a `def` written inside a `class_eval` block, which rubydex
///    files on `Object` and would offer as *every* class's constructor.
///
/// **Case 3 is reached before [`resolve_call`]'s root arm and must make the same refusal**, or the
/// answer the root arm withdraws comes back through `new`. Real cases come from gems:
/// `fast_sqlite`'s `SQLite3::Database.class_eval` and honeycomb-beeline's
/// `::Faraday::Connection.module_eval`. See [`Blocks`].
fn constructor(
    graph: &Graph,
    blocks: Option<&Blocks<'_>>,
    receiver: DeclarationId,
) -> Option<DeclarationId> {
    let attached = attached_class(graph, receiver)?;

    // This finds nothing when rbs is not indexed (rubydex's built-ins have no `Class#new`), which
    // is the same "nobody wrote one" answer.
    if let Some(new) = member_of(graph, receiver, NEW)
        && !owned_by(graph, new, "Class")
    {
        return None;
    }

    let initialize = member_of(graph, attached, INITIALIZE)?;
    (!owned_by(graph, initialize, "BasicObject") && declared_on_the_root(graph, blocks, initialize))
        .then_some(initialize)
}

/// The class a singleton class belongs to, and nothing for anything else.
///
/// rubydex records it as the singleton's owner, so `<Foo>` reaches `Foo` without taking a name
/// apart.
///
/// Shared with [`completion`](super::completion), like [`extended_modules`]: the closure rung and
/// the list beside it must mean the same class by "the class this block's `self` is an instance
/// of", and stating it once keeps that true.
pub(super) fn attached_class(graph: &Graph, receiver: DeclarationId) -> Option<DeclarationId> {
    let declaration = graph.declarations().get(&receiver)?;
    matches!(
        declaration,
        Declaration::Namespace(Namespace::SingletonClass(_))
    )
    .then(|| *declaration.owner_id())
}

fn member_of(graph: &Graph, owner: DeclarationId, member: &str) -> Option<DeclarationId> {
    find_member(graph, owner, StringId::from(member)).ok()
}

/// rubydex's `find_member_in_ancestors`, and the one step Ruby adds for a **module**: an instance
/// of a module is an instance of some class, and every class descends from `Object`, so what the
/// module's own ancestors lack is looked up on `Object`'s. RBS says the same with a module's
/// default self type, `module M : Object`. `self.class` in a concern's method is `Kernel#class`.
///
/// A hit found there is a hit on a root like any other ([`owned_by_a_root`]), so every gate a
/// root answer passes still applies.
pub(crate) fn find_member(
    graph: &Graph,
    owner: DeclarationId,
    member: StringId,
) -> Result<DeclarationId, FindMemberError> {
    match query::find_member_in_ancestors(graph, owner, member, false) {
        Err(FindMemberError::MemberNotFound) if is_module(graph, owner) => {
            query::find_member_in_ancestors(graph, DeclarationId::from(ROOTS[0]), member, false)
        }
        found => found,
    }
}

/// [`find_member`] as the application finds it: **a member only the test suite loads is not the
/// application's**.
///
/// A spec helper that `extend`s a module into a gem's class puts the module in that class's
/// linearization, so the plain walk answers from a method the application never loads. One
/// application's `MessageBus.publish` in a plugin went to `spec/support/diagnostics_helper.rb` as a *Resolved*
/// card, instead of `MessageBus::Implementation#publish`. This walk skips such a member and goes on
/// up, as Ruby does when that file is never loaded.
///
/// **The fence decides, not the caller.** It is off for a cursor inside the suite, where the helper
/// is really in the chain ([`environment`]'s cursor rule), and for every surface that never fences a
/// test tree (`references`, `rename`), which then walk exactly as rubydex does.
pub(crate) fn find_loaded_member(
    graph: &Graph,
    fence: environment::Fence<'_>,
    owner: DeclarationId,
    member: StringId,
) -> Result<DeclarationId, FindMemberError> {
    match loaded_member_in(graph, fence, owner, member) {
        Err(FindMemberError::MemberNotFound) if is_module(graph, owner) => {
            loaded_member_in(graph, fence, DeclarationId::from(ROOTS[0]), member)
        }
        found => found,
    }
}

/// [`find_loaded_member`] in `owner`'s own linearization, with no `Object` behind a module: for a
/// walk of a class's ancestors one at a time (`types::from_super`).
pub(crate) fn loaded_member_in(
    graph: &Graph,
    fence: environment::Fence<'_>,
    owner: DeclarationId,
    member: StringId,
) -> Result<DeclarationId, FindMemberError> {
    if !fence.on_trees() {
        return query::find_member_in_ancestors(graph, owner, member, false);
    }
    let namespace = graph
        .declarations()
        .get(&owner)
        .ok_or(FindMemberError::DeclarationNotFound)?
        .as_namespace()
        .ok_or(FindMemberError::NotNamespace)?;
    namespace
        .ancestors()
        .iter()
        .find_map(|ancestor| {
            let Ancestor::Complete(id) = ancestor else {
                return None;
            };
            let found = *graph
                .declarations()
                .get(id)
                .and_then(Declaration::as_namespace)?
                .members()
                .get(&member)?;
            fence.loadable(graph, found).then_some(found)
        })
        .ok_or(FindMemberError::MemberNotFound)
}

/// The five owners meaning the ancestor walk ran out of the project and into Ruby.
///
/// [`ROOTS`] plus the two only a class object reaches. One list for both sides of the object model,
/// because a macro's symbol is looked up on both.
const OBJECT_MODEL: [&str; 5] = ["Object", "Kernel", "BasicObject", "Module", "Class"];

/// Whether the member found is Ruby's own rather than this project's.
///
/// - **The one filter on [`resolve_symbol`].** Every class inherits `Object` and `Kernel`, so an
///   ancestor walk always ends somewhere, and `Kernel` alone declares `format`, `p`, `print`,
///   `select`, `system`, `test`, `open`, `sub` and `warn`, all common column names. Without this,
///   `validates :format` on a model with no `format` column would jump into `vendor/rbs`: a
///   Resolved answer about a method nobody asked about.
/// - **It never loses a real answer.** A member the class really has is found on the class before
///   the walk reaches a root, so this only fires where the honest answer was already nothing.
pub(super) fn ruby_s_own(graph: &Graph, found: DeclarationId) -> bool {
    OBJECT_MODEL
        .iter()
        .any(|owner| owned_by(graph, found, owner))
}

/// Whether a declaration belongs to a named class, by rubydex's own back-pointer.
///
/// `Class` and `BasicObject` mean "nobody wrote this": they are Ruby's object model, named
/// identically whether from rbs or rubydex's built-ins. Asking the owner, not matching the method
/// name, keeps this independent of rubydex's member spelling.
fn owned_by(graph: &Graph, declaration_id: DeclarationId, owner: &str) -> bool {
    graph
        .declarations()
        .get(&declaration_id)
        .is_some_and(|declaration| *declaration.owner_id() == DeclarationId::from(owner))
}

/// rubydex fabricates a constant reference to `<Foo>` for every call with an implicit or constant
/// receiver, so `Foo.bar` can resolve against `Foo`'s singleton class.
///
/// The user never wrote those bytes, and for an implicit receiver the reference spans the *whole
/// call*, so `alias_method :a, :b` would navigate to `class << self`. Only singleton names use
/// angle brackets, so they are safe to recognise and drop.
fn is_synthetic(graph: &Graph, reference: &ConstantReference) -> bool {
    graph
        .names()
        .get(reference.name_id())
        .and_then(|name| graph.strings().get(name.str()))
        .is_some_and(|string| string.starts_with('<'))
}

fn covers(offset: &Offset, at: u32) -> bool {
    // Inclusive of the end, so a cursor just past an identifier's last character (where editors put
    // it after a double-click) still counts.
    //
    // This admits a candidate; it does not rank it. A span that merely reaches the cursor loses to
    // any span beginning there (`locate` tiers them before comparing widths), so this generosity
    // costs nothing where a better answer exists and is the only answer where none does.
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

    /// Whether the span **begins** at the cursor, which is [`locate`]'s first question.
    fn begins_at(&self, at: u32) -> bool {
        self.start == at
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::locator;
    use crate::analysis::testing::*;

    #[test]
    fn a_top_level_constant_under_simple_delegator_is_the_one_delegator_hands_it_to() {
        // `Delegator < BasicObject`, so a class under `SimpleDelegator` has no `Object` among its
        // ancestors and rubydex resolves a top-level `Current` written there to nothing. Ruby asks
        // `Delegator.const_missing` next, which is `::Object.const_get(n)`, so the constant is the
        // top-level one. It must stay so after an edit: the corpus answered it once only because
        // `delegate.rb` is indexed after the workspace, and lost it when the file was re-indexed.
        //
        // A `BasicObject` subclass with no such `const_missing` raises there, and answers nothing.
        let delegate = "class Delegator < BasicObject\n  def self.const_missing(n)\n    \
                        ::Object.const_get(n)\n  end\nend\n\nclass SimpleDelegator < Delegator\nend\n\n\
                        class Bare < BasicObject\nend\n";
        for (superclass, wanted) in [
            ("SimpleDelegator", vec!["current.rb:0:7"]),
            ("Bare", Vec::new()),
        ] {
            let mut harness = Harness::new();
            harness.write("lib/delegate.rb", delegate);
            harness.write(
                "lib/current.rb",
                "module Current\n  def self.account\n    1\n  end\nend\n",
            );
            let source = format!(
                "class Presenter < {superclass}\n  def token\n    Current.account\n  end\nend\n"
            );
            let uri = harness.write("app/presenter.rb", &source);
            harness.index();
            let before = linked(&harness.definition_at(&uri, &source, "Current.acc"));
            let edited = format!("\n{source}");
            harness.change(&uri, &edited);
            let after = linked(&harness.definition_at(&uri, &edited, "Current.acc"));
            assert_eq!(before, wanted, "{superclass}");
            assert_eq!(after, wanted, "{superclass}, after an edit");
            // The call on it is the class object's, found the same way.
            let account: Vec<String> = if superclass == "Bare" {
                Vec::new()
            } else {
                vec!["current.rb:1:11".to_owned()]
            };
            let called = linked(&harness.definition_at(&uri, &edited, "account\n"));
            assert!(
                superclass == "Bare" || called == account,
                "{superclass}: {called:?}"
            );
        }
    }

    #[test]
    fn the_held_spans_locate_what_the_walk_does_at_every_byte() {
        // `locate_held` must answer exactly `locate`, ties in the same order. The shapes whose
        // spans nest, touch or share bytes: a namespace, a constant path, a call on a constant
        // (with rubydex's made-up `<Bar>` over it), a receiverless call with a block (whose made-up
        // reference spans the whole call), `a.b ||= c`, `!x` and `&&`.
        let unit = "  def m{i}(a)\n    Foo::Bar.baz(a) && !a.b\n    helper(a) do |x|\n      \
                    x.c ||= Qux.new\n    end\n  end\n";
        let mut source = String::from("module Outer\nclass Foo\n");
        for i in 0..60 {
            source.push_str(&unit.replace("{i}", &i.to_string()));
        }
        source.push_str("end\nend\n");
        let mut indexed = Indexed::default();
        for (uri, text) in [
            ("file:///p/big.rb", source.as_str()),
            (
                "file:///p/small.rb",
                "class Small\n  def go = Foo.new\nend\n",
            ),
        ] {
            assert!(crate::analysis::indexer::index_source(
                indexed.graph_mut(),
                uri,
                text,
                &rubydex::indexing::LanguageId::Ruby,
            ));
        }
        rubydex::resolution::Resolver::new(indexed.graph_mut()).resolve();
        let drawn = |found: Vec<Located<'_>>| format!("{found:?}");

        let big = UriId::from("file:///p/big.rb");
        let document = indexed.documents().get(&big).unwrap();
        assert!(
            Spans::worth_holding(document),
            "the file is large enough to be held"
        );
        let length = u32::try_from(source.len()).unwrap();
        for offset in 0..=length + 1 {
            assert_eq!(
                drawn(locate_held(&indexed, big, offset)),
                drawn(locate(&indexed, big, offset)),
                "at {offset}"
            );
        }
        assert_eq!(indexed.spans_held(), 1);

        // A small file is walked and not held, and a document the graph lacks is nothing.
        let small = UriId::from("file:///p/small.rb");
        let at = u32::try_from("class Small\n  def go = Foo".len()).unwrap();
        assert_eq!(
            drawn(locate_held(&indexed, small, at)),
            drawn(locate(&indexed, small, at))
        );
        assert!(!locate_held(&indexed, small, at).is_empty());
        assert!(locate_held(&indexed, UriId::from("file:///p/none.rb"), 0).is_empty());
        assert_eq!(indexed.spans_held(), 1);

        // A write drops them with every other index beside the graph.
        rubydex::resolution::Resolver::new(indexed.graph_mut()).resolve();
        assert_eq!(indexed.spans_held(), 0);
    }

    #[test]
    fn an_id_the_graph_does_not_hold_is_answered_with_nothing() {
        // Real, not hypothetical: `completionItem/resolve` takes its `DeclarationId` from the
        // client, which echoes whatever its list carried, and a config reload drops the graph that
        // list came from. A rubydex id is a 64-bit hash, so a stale one looks like a live one, and
        // the whole chain from id to place must answer "nowhere" rather than resolve against a hash
        // collision.
        let graph = Graph::new();
        let nowhere = DeclarationId::new(1_234_567_890_123_456_789);

        assert!(definitions_of(&graph, nowhere).is_empty());
        assert!(sites(&graph, &Synthesized::new(), nowhere).is_empty());
    }

    /// A workspace with the bundle off, so the load path is exactly the two directories the default
    /// config names and nothing the machine happens to have installed.
    fn unbundled(extra: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!("[gems]\nenabled = false\n{extra}"),
        )
        .unwrap();
        let root = dir.path().to_owned();
        (dir, root.display().to_string())
    }

    const REOPENED: &str = "class Thing\n  def spin\n  end\nend\n";

    #[test]
    fn a_wide_namespace_answers_from_the_file_named_after_it() {
        // Ruby reopens namespaces freely, so a wide one collects a definition per file that touched
        // it (`module Sidekiq`, `module Rails`). Path order is stable but
        // meaningless: it would put an rspec helper above the gem's own `sidekiq.rb`. Both halves
        // of the order are asserted because both are read: `definition` jumps to the first entry,
        // and the card takes its prose from it.
        let (dir, _) = unbundled("");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("lib/aaa_helper.rb", "module Widget\nend\n");
        let named = harness.write(
            "lib/widget.rb",
            "# The widget itself.\nmodule Widget\nend\n",
        );
        harness.write("lib/widget_extensions.rb", "module Widget\nend\n");
        harness.write("lib/zzz_patch.rb", "module Widget\nend\n");
        let source = "Widget\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "Widget");
        let targets = targets.as_array().expect("an array");
        assert_eq!(targets.len(), 4, "{targets:?}");
        assert_eq!(
            targets[0]["targetUri"],
            serde_json::json!(named.as_str()),
            "the file named after it, not the one that sorts first: {targets:?}"
        );

        let markdown = harness.hover_at(&caller, source, "Widget")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(
            markdown.contains("The widget itself."),
            "the card reads the first entry's comments: {markdown}"
        );
    }

    #[test]
    fn a_namespace_no_file_is_named_after_is_ranked_by_where_it_lives() {
        // Most wide namespaces land here: an engine monorepo writes `module Spree` in 539 files, none of them
        // `spree.rb`, so every candidate ties at *unnamed* and the tie-break is the whole answer.
        // Comparing stem lengths is noise (`setup` and `spree` are both five letters). What means
        // something is the path: a directory the constant names, then the file nearest the top of
        // that tree.
        //
        // Both files below would tie on stem length (`sixxxx` and `config` are six letters, like
        // `Widget`), and path order would put `aaa` first.
        let (dir, _) = unbundled("");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("lib/aaa/deep/sixxxx.rb", "module Widget\nend\n");
        let near = harness.write("lib/widget/config.rb", "module Widget\nend\n");
        harness.write("lib/widget/inner/thing.rb", "module Widget\nend\n");
        let source = "Widget\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "Widget");
        assert_eq!(targets.as_array().unwrap().len(), 3, "{targets}");
        assert_eq!(
            targets[0]["targetUri"],
            serde_json::json!(near.as_str()),
            "the directory the constant names, nearest the top of it: {targets}"
        );
    }

    #[test]
    fn the_uri_scheme_is_not_a_directory_named_file() {
        // `file:` squashes to `file` (the colon drops with the separators), so a scheme left on the
        // path puts **every** document in a directory named after `File`. The second tier's
        // directory test then answers `true` everywhere and only depth decides, which swaps
        // one application's two places for `class File`.
        let (dir, _) = unbundled("");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/pp.rb", "class File\nend\n");
        let inside = harness.write("app/file/atomic.rb", "class File\nend\n");
        let source = "File\n";
        let caller = harness.write("app/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "File");
        assert_eq!(
            targets[0]["targetUri"],
            serde_json::json!(inside.as_str()),
            "the directory called `file`, not the scheme: {targets}"
        );
    }

    #[test]
    fn a_file_name_that_merely_holds_the_constant_is_not_named_after_it() {
        // Containment alone lets a long file name swallow a short constant. an application writes `Jobs`
        // in hundreds of files, and `remove_old_auto_close_jobs.rb`'s stem *contains* `jobs`, which
        // would outrank every file merely in `app/jobs/`, `base.rb` included. The name must be at
        // least half the stem, which sends this one to the second tier, where its path is judged.
        let (dir, _) = unbundled("");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let base = harness.write("app/jobs/base.rb", "module Jobs\nend\n");
        harness.write(
            "app/jobs/onceoff/remove_old_auto_close_jobs.rb",
            "module Jobs\nend\n",
        );
        let source = "Jobs\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "Jobs");
        assert_eq!(
            targets[0]["targetUri"],
            serde_json::json!(base.as_str()),
            "a word buried in a file name does not name it: {targets}"
        );
    }

    #[test]
    fn a_file_name_the_constant_is_most_of_is_still_named_after_it() {
        // The other side of the rule, and why it is a proportion, not a prefix:
        // `ruby-progressbar.rb` **is** where `ProgressBar` is declared, and a prefix test would
        // send it to the path tier. The name is eleven of the stem's fifteen characters, so it
        // stays.
        let (dir, _) = unbundled("");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let named = harness.write("lib/ruby-progressbar.rb", "module ProgressBar\nend\n");
        harness.write(
            "lib/ruby-progressbar/components/percentage.rb",
            "module ProgressBar\nend\n",
        );
        let source = "ProgressBar\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "ProgressBar");
        assert_eq!(
            targets[0]["targetUri"],
            serde_json::json!(named.as_str()),
            "{targets}"
        );
    }

    #[test]
    fn a_place_only_the_suite_loads_is_not_where_a_jump_sends_a_reader() {
        // A monorepo's specs are most of the files reopening a namespace: an engine monorepo offers 539 places
        // for `Spree`, 76 under `spec/`. The reader is in application code, which loads none of
        // them. **The card must agree**: a count taken without the fence would disagree with the
        // jump.
        let mut harness = Harness::new();
        let app = harness.write("app/models/shop.rb", "module Shop\nend\n");
        harness.write("spec/models/shop_spec.rb", "module Shop\nend\n");
        let source = "Shop\n";
        let caller = harness.write("app/models/thing.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "Shop");
        assert_eq!(
            targets.as_array().map(|places| places
                .iter()
                .map(|place| place["targetUri"].clone())
                .collect::<Vec<_>>()),
            Some(vec![serde_json::json!(app.as_str())]),
            "{targets}"
        );
        let markdown = harness.hover_at(&caller, source, "Shop")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            !markdown.contains("Defined in"),
            "one place left is no list to count: {markdown}"
        );
    }

    #[test]
    fn a_gem_s_library_under_a_directory_called_test_answers_the_name_rung() {
        // The same rule as the test below, on the name rung. Both halves (the cursor gate and the
        // load paths) travel as one `environment::Fence`, so no surface can have one without the
        // other. Without the load paths, a method defined only in rack-test's
        // `lib/rack/test/utils.rb` would answer **null**, while the same file one directory up
        // answers.
        for relative in ["lib/shouty/test/utils.rb", "lib/shouty/utils.rb"] {
            let (dir, _gem_home, env) = project_with_gem_file(
                relative,
                "module Shouty\n  module Utils\n    def build_headers\n    end\n  end\nend\n",
            );
            let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
            let source = "class Store\n  def go\n    helper.build_headers\n  end\nend\n";
            let caller = harness.write("app/models/store.rb", source);
            harness.index();
            harness.index_gems();

            let targets = harness.definition_at(&caller, source, "build_headers");
            assert_eq!(
                targets.as_array().map(Vec::len),
                Some(1),
                "a gem's `{relative}` is a library either way: {targets}"
            );
        }
    }

    #[test]
    fn a_gem_s_generator_template_is_not_a_place_a_jump_sends_a_reader() {
        // pundit ships `class ApplicationPolicy` in the tree `rails generate` copies from, and
        // every project that installed it has the same class in `app/policies/`. Both are real
        // declarations; only one runs. The load-path clause cannot settle it (the template is under
        // the gem's own `lib/`, so `require` could name it, though nothing does), which is why the
        // tag reads what the tree is *for*.
        let (dir, _gem_home, env) = project_with_gem_file(
            "lib/generators/shouty/install/templates/application_policy.rb",
            "class ApplicationPolicy\n  def initialize(user, record)\n  end\nend\n",
        );
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let own = harness.write(
            "app/policies/application_policy.rb",
            "class ApplicationPolicy\nend\n",
        );
        let source = "class MacroPolicy < ApplicationPolicy\nend\n";
        let caller = harness.write("app/policies/macro_policy.rb", source);
        harness.index();
        harness.index_gems();

        let targets = harness.definition_at(&caller, source, "ApplicationPolicy\n");
        assert_eq!(
            targets.as_array().map(|places| places
                .iter()
                .map(|place| place["targetUri"].clone())
                .collect::<Vec<_>>()),
            Some(vec![serde_json::json!(own.as_str())]),
            "the project's own copy is the one that runs: {targets}"
        );
    }

    #[test]
    fn rspec_describe_answers_with_no_card_rather_than_minitest_s() {
        // **The real cursor.** `vendor/rbs/stdlib/minitest/0/kernel.rbs` declares
        // `private def describe` on `Kernel`, which `Object` includes and every class object
        // inherits. So the ancestor walk reaches it from `RSpec`, and the first line of nearly
        // every spec file would get a **Resolved** card naming minitest's signature.
        //
        // The signature is written out here, not taken from the binary, because the vendored copy
        // is extracted at run time and no unit test indexes it. The declaration is minitest's own;
        // the ancestry around it is `TYPED_RBS`'s.
        //
        // **No card is the correct answer, not a degraded one.** rspec-core defines
        // `RSpec.describe` dynamically (no `def describe` anywhere in the gem), so there is no
        // better place to name, and the fall-through finds nothing public.
        let mut harness = signed(
            &[(
                "core/core.rbs",
                "module Kernel\n  \
             private def describe: (untyped desc) { (?) -> untyped } -> untyped\n\
             end\n\n\
             class Object\n  include Kernel\nend\n",
            )],
            "",
        );
        harness.write("app/models/story.rb", "class Story\nend\n");
        harness.write("app/lib/rspec.rb", "module RSpec\nend\n");
        let source = "RSpec.describe Story do\nend\n";
        let caller = harness.write("spec/models/story_spec.rb", source);
        harness.index();
        harness.index_gems();

        let targets = harness.definition_at(&caller, source, "describe Story");
        assert!(
            targets.as_array().is_none_or(Vec::is_empty),
            "minitest's `private Kernel#describe` is not where this call goes: {targets}"
        );
        let card = harness.hover_at(&caller, source, "describe Story");
        assert!(
            card.get("contents").is_none(),
            "and no card at all, because there is no `def describe` to name: {card}"
        );
    }

    #[test]
    fn a_private_method_does_not_answer_a_receiver_that_is_written() {
        // **The defect this gate exists for.** rubydex files a top-level `private def` on `Object`,
        // an ancestor of every class object, so the ancestor walk finds it for any constant
        // receiver and returns it as **precise**: the tier claiming the code names the type. Ruby
        // raises `NoMethodError: private method called` there.
        //
        // The real case is `RSpec.describe` on the first line of nearly every spec file: the walk
        // reaches minitest's `private Kernel#describe` from the vendored signatures, and the card
        // even printed *private*.
        let mut harness = Harness::new();
        harness.write(
            "app/lib/support.rb",
            "private def describe_thing(name)\nend\n",
        );
        harness.write("app/models/widgets.rb", "module Widgets\nend\n");
        let source = "class Store\n  def go\n    Widgets.describe_thing(\"x\")\n  end\nend\n";
        let caller = harness.write("app/models/store.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "describe_thing(\"x\")");
        assert!(
            targets.as_array().is_none_or(Vec::is_empty),
            "no public `describe_thing` exists, so the honest answer is no place: {targets}"
        );
        let markdown =
            harness.hover_at(&caller, source, "describe_thing(\"x\")")["contents"]["value"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
        assert!(
            !markdown.contains("describe_thing"),
            "and no card rather than a better one — rspec-core defines `describe` dynamically, \
             so there is no correct place to name: {markdown}"
        );
    }

    #[test]
    fn a_private_method_still_answers_where_ruby_would_let_it_be_written() {
        // The other half, which decides whether this gate is a fix or a regression. Ruby allows a
        // private call with **no receiver**, and with one spelled `self` (through `.` and `::`,
        // since 2.7). All three look the same to rubydex, whose `MethodRef` records `Some(name)`
        // for the receiver in each, so the question is asked of the syntax instead.
        let mut harness = Harness::new();
        let source = concat!(
            "class Vault\n",
            "  def peek\n",
            "    secret_value\n",
            "  end\n",
            "\n",
            "  def peek_on_self\n",
            "    self.secret_value\n",
            "  end\n",
            "\n",
            "  private\n",
            "\n",
            "  def secret_value\n",
            "  end\n",
            "end\n",
        );
        let uri = harness.write("app/models/vault.rb", source);
        harness.index();

        for needle in [
            "secret_value\n  end\n\n  def peek_on_self",
            "secret_value\n  end\n\n  private",
        ] {
            let targets = harness.definition_at(&uri, source, needle);
            assert_eq!(
                targets[0]["targetUri"],
                serde_json::json!(uri.as_str()),
                "an implicit receiver and one spelled `self` both reach it: {targets}"
            );
        }
    }

    #[test]
    fn a_private_method_on_another_object_is_refused_even_inside_its_own_class() {
        // Upstream's looseness, asked of the jump rather than the list. rubydex passes a private
        // method whenever the caller's `self` is the receiver's **class**; Ruby exempts only a
        // receiver *written* `self`, so `Vault.new.secret` raises even inside `Vault`. Every
        // surface must apply the stricter rule, as `completion::reachable` does.
        let mut harness = Harness::new();
        let source = concat!(
            "class Vault\n",
            "  def peek\n",
            "    Vault.secret_value\n",
            "  end\n",
            "\n",
            "  private_class_method def self.secret_value\n",
            "  end\n",
            "end\n",
        );
        let uri = harness.write("app/models/vault.rb", source);
        harness.index();

        let targets = harness.definition_at(
            &uri,
            source,
            "secret_value\n  end\n\n  private_class_method",
        );
        assert!(
            targets.as_array().is_none_or(Vec::is_empty),
            "written receiver, not `self`, so Ruby raises and the jump has nowhere to go: \
             {targets}"
        );
    }

    /// A real concern, trimmed to the three things that make the shape: a block holding a
    /// bare `private`, a `def` inside it below that `private`, and a `def` in the module body after
    /// the block.
    const BLOCK_SCOPED_PRIVATE: &str = concat!(
        "module HasCustomFields\n",
        "  class_methods do\n",
        "    def custom_fields_for_ids(ids)\n",
        "      ids\n",
        "    end\n",
        "\n",
        "    private\n",
        "\n",
        "    def custom_field_meta_data\n",
        "      @custom_field_meta_data\n",
        "    end\n",
        "  end\n",
        "\n",
        "  def upsert_custom_fields(fields)\n",
        "    fields\n",
        "  end\n",
        "end\n",
    );

    /// The same escape, with visibility toggled inside the block instead of set once.
    ///
    /// `protected` and `public` turn the default back *off*, and modules that group their members
    /// use them. What escapes is the state at the end of the block, so a `protected` in the middle
    /// changes which `def`s the block hides, not the repair below it.
    const BLOCK_SCOPED_TOGGLE: &str = concat!(
        "module HasSettings\n",
        "  class_methods do\n",
        "    def settings_for_ids(ids)\n",
        "      ids\n",
        "    end\n",
        "\n",
        "    protected\n",
        "\n",
        "    def settings_peer\n",
        "      @settings_peer\n",
        "    end\n",
        "\n",
        "    private\n",
        "\n",
        "    def settings_meta_data\n",
        "      @settings_meta_data\n",
        "    end\n",
        "  end\n",
        "\n",
        "  def upsert_settings(fields)\n",
        "    fields\n",
        "  end\n",
        "end\n",
    );

    #[test]
    fn a_visibility_reset_inside_the_block_does_not_change_what_escapes_it() {
        // The repair rereads the declaring file for the modifier rubydex applied to the wrong body,
        // so it must read *every* modifier there, not only the one that set the default. A bare
        // `protected` or `public` sets it back to public, and that arm is reachable exactly where
        // this one is. The asserted property is the fixture's: the `def` after the block is public.
        let mut harness = Harness::new();
        let concern = harness.write("app/models/concerns/has_settings.rb", BLOCK_SCOPED_TOGGLE);
        let source = concat!(
            "class Preference\n",
            "  include HasSettings\n",
            "end\n",
            "\n",
            "Preference.new.upsert_settings({})\n",
        );
        let uri = harness.write("app/models/preference.rb", source);
        harness.index();

        let targets = harness.definition_at(&uri, source, "upsert_settings({})");
        assert_eq!(
            targets[0]["targetUri"],
            serde_json::json!(concern.as_str()),
            "the modifiers are all inside the block and this `def` is below it: {targets}"
        );
        let markdown = harness.hover_at(&uri, source, "upsert_settings({})")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            !markdown.contains("private"),
            "and the card does not call a public method private: {markdown}"
        );
    }

    #[test]
    fn an_edit_that_leaves_the_def_where_it_was_is_walked_again_and_not_answered_from_the_held_walk()
     {
        // The walk behind the repair outlives the request, held by the text it read. This edit
        // moves the `private` out of the block and leaves the `def` at the same offset, so a walk
        // held by the document alone would still call it escaped and open a method Ruby now hides.
        let before = concat!(
            "module Fields\n",
            "  class_methods do\n",
            "    private\n",
            "  end\n",
            "\n",
            "  def upsert(fields)\n",
            "    fields\n",
            "  end\n",
            "end\n",
        );
        let after = concat!(
            "module Fields\n",
            "  private\n",
            "##########################\n",
            "\n",
            "  def upsert(fields)\n",
            "    fields\n",
            "  end\n",
            "end\n",
        );
        assert_eq!(before.find("def upsert"), after.find("def upsert"));
        let mut harness = Harness::new();
        let concern = harness.write("app/models/concerns/fields.rb", before);
        let source = "class Record\n  include Fields\nend\n\nRecord.new.upsert({})\n";
        let uri = harness.write("app/models/record.rb", source);
        harness.index();
        harness.open(&concern, before);

        let targets = harness.definition_at(&uri, source, "upsert({})");
        assert_eq!(
            targets[0]["targetUri"],
            serde_json::json!(concern.as_str()),
            "the `private` escaped the block, so the `def` is public: {targets}"
        );

        harness.change(&concern, after);
        let targets = harness.definition_at(&uri, source, "upsert({})");
        assert!(
            targets.as_array().is_none_or(Vec::is_empty),
            "the `private` is the module's own now, and the receiver is written: {targets}"
        );
    }

    /// The same escape, written with the other word that sets the default.
    ///
    /// - **`module_function` is the second half of [`is_private`]'s first line.** rubydex records
    ///   its instance copy as `Visibility::ModuleFunction`, and Ruby agrees that copy is private.
    ///   So a bare `module_function` in a block escapes exactly like a bare `private`, and the
    ///   repair must read it too.
    /// - **`module_eval do` gives the block its own cref**, so Ruby's answer is that nothing
    ///   escapes.
    /// - **The last line names a member**, which the repair must *not* touch: rubydex reads
    ///   `module_function :to_euros` correctly and no block leaked it (`Escapes::remember` guards
    ///   this). The veto takes away only the precise instance `def`; the rungs below still reach
    ///   the singleton copy the same macro filed, so the card names `Conversions.to_euros`, as a
    ///   guess.
    const BLOCK_SCOPED_MODULE_FUNCTION: &str = concat!(
        "module Conversions\n",
        "  module_eval do\n",
        "    module_function\n",
        "\n",
        "    def to_cents(amount)\n",
        "      amount\n",
        "    end\n",
        "  end\n",
        "\n",
        "  def to_dollars(amount)\n",
        "    amount\n",
        "  end\n",
        "\n",
        "  def to_euros(amount)\n",
        "    amount\n",
        "  end\n",
        "\n",
        "  module_function :to_euros\n",
        "end\n",
    );

    #[test]
    fn a_module_function_inside_a_block_escapes_it_no_further_than_a_private_does() {
        // Both arms of `Escapes::visit_call_node` in one file: a bare `module_function` that sets
        // the default, and one that names a member. They are `private`'s two shapes, read by the
        // same lines, so the second word must give the same two answers.
        let mut harness = Harness::new();
        let module = harness.write(
            "app/models/concerns/conversions.rb",
            BLOCK_SCOPED_MODULE_FUNCTION,
        );
        let source = concat!(
            "class Money\n",
            "  include Conversions\n",
            "end\n",
            "\n",
            "Money.new.to_dollars(1)\n",
            "Money.new.to_euros(1)\n",
        );
        let uri = harness.write("app/models/money.rb", source);
        harness.index();

        // The bare `module_function` is inside the block and this `def` is below it.
        let targets = harness.definition_at(&uri, source, "to_dollars(1)");
        assert_eq!(
            targets[0]["targetUri"],
            serde_json::json!(module.as_str()),
            "a `module_function` in a block body governs that body: {targets}"
        );
        let markdown = harness.hover_at(&uri, source, "to_dollars(1)")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            !markdown.contains("private"),
            "and the card does not call a public method private: {markdown}"
        );

        // The named one is left as rubydex recorded it: nothing leaked it. The surviving veto shows
        // in the card as a guess, not as an empty answer, because the same macro files a public
        // singleton copy the name rung can reach.
        let markdown = harness.hover_at(&uri, source, "to_euros(1)")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            markdown.contains("Conversions.to_euros"),
            "the private instance copy is refused and the singleton one is what is left: {markdown}"
        );
        assert!(
            markdown.contains("Guessed from name alone."),
            "`module_function :to_euros` is the file saying so, not a block escaping: {markdown}"
        );
    }

    #[test]
    fn a_public_method_below_a_block_holding_private_answers_at_a_written_receiver() {
        // **The defect the gate introduced.** A bare `private` is a statement, and rubydex applies
        // it to its body until the body ends; a block is not a body to it. So
        // `class_methods do … private … end` sets the *module's* default, and every `def` after the
        // block is recorded private. An application calls `HasCustomFields#upsert_custom_fields` on
        // explicit receivers, and the gate would refuse all of them.
        //
        // `class_methods` is `module_eval`, and this file knows no Rails word: the repair is right
        // because a block has a body, not because of which gem opened it. See `Modifiers`.
        let mut harness = Harness::new();
        let concern = harness.write(
            "app/models/concerns/has_custom_fields.rb",
            BLOCK_SCOPED_PRIVATE,
        );
        let source = concat!(
            "class Category\n",
            "  include HasCustomFields\n",
            "end\n",
            "\n",
            "Category.new.upsert_custom_fields({})\n",
        );
        let uri = harness.write("app/models/category.rb", source);
        harness.index();

        let targets = harness.definition_at(&uri, source, "upsert_custom_fields({})");
        assert_eq!(
            targets[0]["targetUri"],
            serde_json::json!(concern.as_str()),
            "the `private` is inside the block and this `def` is below it: {targets}"
        );
        let markdown =
            harness.hover_at(&uri, source, "upsert_custom_fields({})")["contents"]["value"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
        assert!(
            !markdown.contains("Guessed from name alone"),
            "and resolved rather than guessed, because the refusal never happened: {markdown}"
        );
        // **The card prints the word too, from the same wrong record.** A jump landing on a card
        // reading `private HasCustomFields#upsert_custom_fields` has moved the false sentence, not
        // removed it.
        assert!(
            !markdown.contains("private"),
            "and the card does not call a public method private: {markdown}"
        );
    }

    #[test]
    fn a_private_inside_a_block_still_applies_inside_that_block() {
        // The repair narrows the modifier to its own body; it does not discard it. A `def` *under*
        // that `private` and *inside* the same block is private in every reading of Ruby.
        let mut harness = Harness::new();
        harness.write(
            "app/models/concerns/has_custom_fields.rb",
            BLOCK_SCOPED_PRIVATE,
        );
        let source = concat!(
            "class Category\n",
            "  include HasCustomFields\n",
            "end\n",
            "\n",
            "Category.new.custom_field_meta_data\n",
        );
        let uri = harness.write("app/models/category.rb", source);
        harness.index();

        let targets = harness.definition_at(&uri, source, "custom_field_meta_data\n");
        assert!(
            targets.as_array().is_none_or(Vec::is_empty),
            "written receiver, and this one really is under the `private`: {targets}"
        );
    }

    #[test]
    fn a_private_in_a_body_still_applies_to_every_def_below_it() {
        // The repair is a narrowing, not a deletion: in the ordinary shape, a bare `private` in a
        // class body governs every later `def`, block or not.
        let mut harness = Harness::new();
        harness.write(
            "app/models/vault.rb",
            concat!(
                "class Vault\n",
                "  [1].each do |n|\n",
                "    n\n",
                "  end\n",
                "\n",
                "  private\n",
                "\n",
                "  def secret_value\n",
                "  end\n",
                "end\n",
            ),
        );
        let source = "Vault.new.secret_value\n";
        let uri = harness.write("app/models/caller.rb", source);
        harness.index();

        let targets = harness.definition_at(&uri, source, "secret_value\n");
        assert!(
            targets.as_array().is_none_or(Vec::is_empty),
            "the `private` is the body's own and a block above it changes nothing: {targets}"
        );
    }

    #[test]
    fn a_def_a_visibility_call_names_is_refused_however_the_body_reads() {
        // The one way the repair could overreach. It overturns rubydex's record only where a bare
        // modifier is the whole reason, so the walk must know the *named* forms exist
        // (`private def name`, `private :name`) without interpreting them: a `def` those pick out
        // is private for a reason no block could have leaked.
        //
        // The inline form needs the walk's own guard. `private :name` writes a second definition
        // into the graph at the call, and a declaration is confirmed private by any non-escaping
        // definition, so that spelling is already refused by the line above the guard.
        let mut harness = Harness::new();
        harness.write(
            "app/models/vault.rb",
            concat!(
                "class Vault\n",
                "  [1].each do |n|\n",
                "    private\n",
                "    n\n",
                "  end\n",
                "\n",
                "  private def secret_value\n",
                "  end\n",
                "\n",
                "  def quiet_value\n",
                "  end\n",
                "\n",
                "  private \"quiet_value\"\n",
                "end\n",
            ),
        );
        let source = "Vault.new.secret_value\nVault.new.quiet_value\n";
        let uri = harness.write("app/models/caller.rb", source);
        harness.index();

        let targets = harness.definition_at(&uri, source, "secret_value\n");
        assert!(
            targets.as_array().is_none_or(Vec::is_empty),
            "`private def` names this one, which the block did not do: {targets}"
        );
        // The string spelling, recognised for the same reason as the symbol: a name is a name
        // however it is quoted.
        let targets = harness.definition_at(&uri, source, "quiet_value\n");
        assert!(
            targets.as_array().is_none_or(Vec::is_empty),
            "`private \"quiet_value\"` names this one: {targets}"
        );
    }

    #[test]
    fn a_constructor_is_not_refused_by_the_privacy_gate() {
        // **The exemption, without which this change would break every workspace.** `Foo.new`
        // resolves to `Foo#initialize`, which Ruby makes private by name, so a gate reading
        // visibility off the redirect would refuse every constructor at a written, non-`self`
        // receiver. `holds_private` skips redirected resolutions for this.
        let mut harness = Harness::new();
        let source = concat!(
            "class Vault\n",
            "  def initialize(name)\n",
            "    @name = name\n",
            "  end\n",
            "end\n",
            "\n",
            "Vault.new(\"x\")\n",
        );
        let uri = harness.write("app/models/vault.rb", source);
        harness.index();

        let targets = harness.definition_at(&uri, source, "new(\"x\")");
        assert_eq!(
            targets[0]["targetUri"],
            serde_json::json!(uri.as_str()),
            "the redirect to `#initialize` survives: {targets}"
        );
    }

    #[test]
    fn a_private_method_on_a_derived_receiver_is_refused_for_a_guess() {
        // **Upstream's looseness, at the second rung.** `Vault.new` is not a constant, so rubydex
        // records no receiver and the resolved rung never runs. `types::method_receiver` types it
        // from the constructor, and the member lookup finds a `def` Ruby refuses. The card would be
        // *Derived*: weaker than *Resolved*, but still a claim the call can be made.
        //
        // `Other#secret_value` gives the name rung something public to answer with, which makes the
        // refusal observable: with only the private `def`, the list is empty and there is no card.
        let mut harness = Harness::new();
        let source = concat!(
            "class Vault\n",
            "  def peek\n",
            "    Vault.new.secret_value\n",
            "  end\n",
            "\n",
            "  private\n",
            "\n",
            "  def secret_value\n",
            "  end\n",
            "end\n",
            "\n",
            "class Other\n",
            "  def secret_value\n",
            "  end\n",
            "end\n",
        );
        let uri = harness.write("app/models/vault.rb", source);
        harness.index();

        let needle = "secret_value\n  end\n\n  private";
        let markdown = harness.hover_at(&uri, source, needle)["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            markdown.contains("Other#secret_value"),
            "the public one on another class is what is left: {markdown}"
        );
        assert!(
            markdown.contains("Guessed from name alone."),
            "and the card says it is only a name match: {markdown}"
        );
    }

    #[test]
    fn a_private_method_is_still_found_by_references_and_by_rename() {
        // `resolve` passes `Privacy::Allowed`, and this asserts why that is a decision: those
        // callers ask *where is this used*, where a use of a private method is a use, and they have
        // no text, so the gate's question cannot be asked.
        let mut harness = Harness::new();
        let source = concat!(
            "class Vault\n",
            "  def peek\n",
            "    secret_value\n",
            "  end\n",
            "\n",
            "  private\n",
            "\n",
            "  def secret_value\n",
            "  end\n",
            "end\n",
        );
        let uri = harness.write("app/models/vault.rb", source);
        harness.index();

        let found = harness.reference_list(&uri, source, "secret_value\n  end\nend", true);
        assert_eq!(found.len(), 2, "the declaration and the call: {found:?}");
    }

    /// A module's `self` is some class's instance, and every class descends from `Object`: what
    /// the module's own ancestors lack is `Object`'s, as `self.class` in a concern is `Kernel`'s.
    #[test]
    fn a_module_s_self_has_object_s_methods() {
        let mut harness = signed(&[("core/core.rbs", TYPED_RBS)], "");
        harness.write(
            "lib/object_ext.rb",
            "class Object\n  def described\n    \"x\"\n  end\nend\n",
        );
        let source = "\
module Named
  def label
    a = described
    b = self.described
    c = missing
  end
end
";
        let uri = harness.write("app/models/concerns/named.rb", source);
        harness.index();
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "    a: String = described\n    b: String = self.described"
        );
        let card = harness.hover_at(&uri, source, "described\n    b")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(card.contains("Object#described"), "{card}");
        assert!(!card.contains("Guessed from name alone"), "{card}");
    }

    #[test]
    fn a_top_level_def_in_a_generator_template_does_not_answer_the_root_rung() {
        // The tag's worst case, and why the root rung reads it though it ignores the layout.
        // fabrication ships
        // `lib/rails/generators/fabrication/cucumber_steps/templates/fabrication_steps.rb`, whose
        // top-level `def with_ivars` rubydex files on `Object`, a member of every receiver.
        // `resolve_call`'s root arm would return it as **precise**, so the card would say
        // *Resolved* and name a file nothing loads.
        //
        // active_model_serializers' `.id` template has the same shape; with the fence, those
        // cursors fall through and answer the model's real column from `db/structure.sql`.
        let (dir, _gem_home, env) = project_with_gem_file(
            "lib/generators/shouty/cucumber_steps/templates/shouty_steps.rb",
            "def with_ivars(name)\nend\n",
        );
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let real = harness.write(
            "app/lib/builder.rb",
            "class Builder\n  def with_ivars(name)\n  end\nend\n",
        );
        let source = "class Store\n  def go\n    with_ivars(\"x\")\n  end\nend\n";
        let caller = harness.write("app/models/store.rb", source);
        harness.index();
        harness.index_gems();

        let targets = harness.definition_at(&caller, source, "with_ivars(");
        assert_eq!(
            targets.as_array().map(|places| places
                .iter()
                .map(|place| place["targetUri"].clone())
                .collect::<Vec<_>>()),
            Some(vec![serde_json::json!(real.as_str())]),
            "the template's `Object#with_ivars` is out and the name rung is what is left: {targets}"
        );
        let markdown = harness.hover_at(&caller, source, "with_ivars(")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(markdown.contains("Builder#with_ivars"), "{markdown}");
        assert!(!markdown.contains("Object#with_ivars"), "{markdown}");
    }

    #[test]
    fn a_library_directory_called_test_is_not_a_test_tree() {
        // The tag is four directory names read from a path, written for *the project's* trees. A
        // gem shipping `lib/rack/test/` (or `railties`' `rails/commands/test/`) is publishing a
        // library, and dropping it would delete the answer. What `require` can name is what the
        // application loads, whatever the directory is called, and `load` is the list that knows.
        // Without this, the fence would empty lists on an application that has no project test tree in
        // any place list.
        let (dir, _) = unbundled("");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let library = harness.write("lib/rack/test/utils.rb", "module Suite\nend\n");
        harness.write("spec/models/thing_spec.rb", "module Suite\nend\n");
        let source = "Suite\n";
        let caller = harness.write("app/models/thing.rb", source);
        harness.index();
        harness.index_gems();

        let targets = harness.definition_at(&caller, source, "Suite");
        assert_eq!(
            targets.as_array().map(|places| places
                .iter()
                .map(|place| place["targetUri"].clone())
                .collect::<Vec<_>>()),
            Some(vec![serde_json::json!(library.as_str())]),
            "the library survives and the spec does not: {targets}"
        );
    }

    #[test]
    fn a_namespace_only_a_spec_declares_keeps_the_place_it_has() {
        // The safety clause, as for signatures above: dropped wherever a loadable place survives
        // beside it, kept where none does. A declaration written only under `spec/` is still better
        // answered than not, and in practice this never empties a list.
        let mut harness = Harness::new();
        let only = harness.write("spec/support/fixtures.rb", "module OnlyHere\nend\n");
        let source = "OnlyHere\n";
        let caller = harness.write("app/models/thing.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "OnlyHere");
        assert_eq!(
            targets[0]["targetUri"],
            serde_json::json!(only.as_str()),
            "{targets}"
        );
    }

    #[test]
    fn a_cursor_in_a_test_tree_is_still_sent_to_the_suite_s_own_copy() {
        // The gate is the cursor's, as for the name rung and `resolve_call`'s root arm: a developer
        // in a spec is who the spec's copy answers, and firing there would remove the place they
        // most want.
        let mut harness = Harness::new();
        harness.write("app/models/shop.rb", "module Shop\nend\n");
        harness.write("spec/support/shop_extras.rb", "module Shop\nend\n");
        let source = "Shop\n";
        let caller = harness.write("spec/models/thing_spec.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "Shop");
        assert_eq!(
            targets.as_array().unwrap().len(),
            2,
            "both, because the cursor is in a test tree: {targets}"
        );
    }

    #[test]
    fn a_methods_places_are_not_reordered_by_a_file_name() {
        // Namespaces only, because a file is named after the class in it. A method's file is named
        // after its class too, so ranking a method's places this way would reorder on nothing:
        // `spin.rb` is no better a place for `Shop.spin` than the file that sorts first.
        let (dir, _) = unbundled("");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let first = harness.write("lib/aaa.rb", "class Shop\n  def self.spin\n  end\nend\n");
        harness.write("lib/spin.rb", "class Shop\n  def self.spin\n  end\nend\n");
        let source = "Shop.spin\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "spin");
        assert_eq!(targets.as_array().unwrap().len(), 2, "{targets}");
        assert_eq!(
            targets[0]["targetUri"],
            serde_json::json!(first.as_str()),
            "still alphabetical: {targets}"
        );
    }

    #[test]
    fn the_root_rung_reads_the_directory_name_and_not_the_load_path() {
        // The one rung the library exemption deliberately does **not** reach. `rbs` ships
        // `lib/rbs/test/setup.rb`, a script `require` can name, with a real top-level `def match`.
        // With the exemption, the root arm answered it as **precise**, so `match` in one application's
        // `config/routes.rb` became a one-place *Resolved* card pointing at an RBS test harness,
        // instead of a *Guessed* list holding the right answer. A root hit is a hit on every
        // receiver, so this rung takes the crudest reading of the path, and the list rungs take the
        // better one.
        let (dir, _gem_home, env) =
            project_with_gem_file("lib/shouty/test/setup.rb", "def calibrate(filter)\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let real = harness.write(
            "app/lib/tuner.rb",
            "class Tuner\n  def calibrate(filter)\n  end\nend\n",
        );
        let source = "class Post\n  def bake\n    calibrate(\"x\")\n  end\nend\n";
        let post = harness.write("app/models/post.rb", source);
        harness.index();
        harness.index_gems();

        let targets = harness.definition_at(&post, source, "calibrate(");
        let uris: Vec<String> = targets
            .as_array()
            .expect("a list")
            .iter()
            .map(|place| place["targetUri"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert!(
            uris.len() > 1 && uris.contains(&real.as_str().to_owned()),
            "not one precise place in the gem's script, but the name rung's list with the real \
             `Tuner#calibrate` in it: {targets}"
        );
        // The tier is what matters. A candidate list says *matched on the method name alone*, and
        // the reader can see the gem's script for what it is; a one-place **Resolved** card
        // pointing at it cannot be seen through.
        let markdown = harness.hover_at(&post, source, "calibrate(")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            harness
                .candidates_at(&post, source, "calibrate(")
                .contains(&"Tuner#calibrate".to_owned()),
            "{markdown}"
        );
        assert!(markdown.contains("Guessed from name alone"), "{markdown}");
    }

    #[test]
    fn a_root_answer_only_the_suite_declares_falls_through_to_the_name_rung() {
        // `Object` is an ancestor of every receiver, so a top-level `def` in a spec (where rubydex
        // also files a `def` written inside an `RSpec.describe` block) answers for the whole
        // workspace, and `resolve_call`'s root arm returns it as **precise**. `loadable_from` never
        // sees it, because `resolve_typed` fences only imprecise answers. Every such answer in the
        // corpora was wrong.
        //
        // The fall-through is the point. Silence would also beat the wrong jump, but the name rung
        // holds the method the reader actually meant.
        let mut harness = Harness::new();
        let real = harness.write(
            "app/lib/cooker.rb",
            "class Cooker\n  def cook(raw)\n  end\nend\n",
        );
        harness.write(
            "spec/lib/email_cook_spec.rb",
            "def cook(raw, expected)\nend\n",
        );
        let source = "class Post\n  def bake\n    cook(\"x\")\n  end\nend\n";
        let post = harness.write("app/models/post.rb", source);
        harness.index();

        let targets = harness.definition_at(&post, source, "cook(");
        assert_eq!(
            targets.as_array().map(|places| places
                .iter()
                .map(|place| place["targetUri"].clone())
                .collect::<Vec<_>>()),
            Some(vec![serde_json::json!(real.as_str())]),
            "the spec's `Object#cook` is out and the name rung's `Cooker#cook` is what is left: {targets}"
        );
        // The card says it is a guess, the honest half of the trade: a *Guessed* card naming the
        // right method beats a *Resolved* one naming the wrong one.
        let markdown = harness.hover_at(&post, source, "cook(")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(markdown.contains("Cooker#cook"), "{markdown}");
        assert!(!markdown.contains("Object#cook"), "{markdown}");
    }

    #[test]
    fn a_module_only_the_suite_extends_is_walked_past_from_application_code() {
        // A real application's shape: a spec helper reopens a library's module and `extend`s a wrapper whose
        // `publish` calls `super`. rubydex links the extend, so the plain walk reached the wrapper
        // first and answered a plugin's `MessageBus.publish` with a *Resolved* card in
        // `spec/support`, a file the application never loads. Only from the suite is the wrapper
        // really in the chain.
        let mut harness = Harness::new();
        let library = harness.write(
            "lib/message_bus.rb",
            "module MessageBus\n  module Implementation\n    def publish(channel, data)\n    end\n  \
             end\n\n  extend Implementation\nend\n",
        );
        let helper = harness.write(
            "spec/support/diagnostics_helper.rb",
            "module MessageBus\n  module DiagnosticsHelper\n    def publish(channel, data)\n      \
             super\n    end\n  end\n\n  extend DiagnosticsHelper\nend\n",
        );
        let source = "class Notice\n  def deliver\n    MessageBus.publish(\"/x\", 1)\n  end\nend\n";
        let notice = harness.write("app/models/notice.rb", source);
        let spec = harness.write("spec/models/notice_spec.rb", source);
        harness.index();

        let uris = |targets: serde_json::Value| {
            targets.as_array().map(|places| {
                places
                    .iter()
                    .map(|place| place["targetUri"].clone())
                    .collect::<Vec<_>>()
            })
        };
        let targets = harness.definition_at(&notice, source, "publish");
        assert_eq!(
            uris(targets.clone()),
            Some(vec![serde_json::json!(library.as_str())]),
            "the application reaches the library's `publish`: {targets}"
        );
        let card = harness.hover_at(&notice, source, "publish")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            card.contains("MessageBus::Implementation#publish"),
            "{card}"
        );
        assert!(!card.contains("DiagnosticsHelper"), "{card}");
        assert!(!card.contains("Guessed from name alone"), "{card}");

        let targets = harness.definition_at(&spec, source, "publish");
        assert_eq!(
            uris(targets.clone()),
            Some(vec![serde_json::json!(helper.as_str())]),
            "the suite runs with its helper in the chain: {targets}"
        );
    }

    #[test]
    fn an_extend_a_spec_writes_late_is_not_the_application_s_either() {
        // The extend repair's road to the same wrong card. A spec file written after the graph
        // settled reopens the module to `extend` its helper; rubydex never links that edge, so
        // `extended_member` finds it, and it answered application code with the helper's method.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("lib/message_bus.rb", "module MessageBus\nend\n");
        harness.write(
            "spec/support/diagnostics_helper.rb",
            "module DiagnosticsHelper\n  def publish(channel, data)\n  end\nend\n",
        );
        let source = "class Notice\n  def deliver\n    MessageBus.publish(\"/x\", 1)\n  end\nend\n";
        let notice = harness.write("app/models/notice.rb", source);
        let spec = harness.write("spec/models/notice_spec.rb", source);
        harness.index();
        let late = harness.write(
            "spec/support/extend_bus.rb",
            "module MessageBus\n  extend DiagnosticsHelper\nend\n",
        );
        harness.watch(&[&late]);

        let targets = harness.definition_at(&notice, source, "publish");
        assert!(targets.is_null(), "no loaded code answers it: {targets}");
        let card = harness.hover_at(&notice, source, "publish");
        assert!(card.is_null(), "{card}");

        let card = harness.hover_at(&spec, source, "publish")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(card.contains("DiagnosticsHelper#publish"), "{card}");
        assert!(!card.contains("Guessed from name alone"), "{card}");
    }

    #[test]
    fn a_top_level_def_in_a_migration_is_not_a_member_of_every_receiver() {
        // The root rung, one tree beyond the spec. rubydex has no notion of a script, so a
        // top-level `def` in a migration (a helper above `def change`) lands on `Object` and
        // answers every receiver *precisely*. The root rung reads the directory name and nothing
        // else, on purpose: a fence loosened on the rung where being wrong is worst is loosened
        // backwards.
        let mut harness = Harness::new();
        let real = harness.write(
            "app/lib/cooker.rb",
            "class Cooker\n  def cook(raw)\n  end\nend\n",
        );
        harness.write(
            "db/migrate/20180101000000_bake_everything.rb",
            "def cook(raw, expected)\nend\n",
        );
        let source = "class Post\n  def bake\n    cook(\"x\")\n  end\nend\n";
        let post = harness.write("app/models/post.rb", source);
        harness.index();

        let targets = harness.definition_at(&post, source, "cook(");
        assert_eq!(
            targets.as_array().map(|places| places
                .iter()
                .map(|place| place["targetUri"].clone())
                .collect::<Vec<_>>()),
            Some(vec![serde_json::json!(real.as_str())]),
            "the migration's `Object#cook` is out and the name rung's `Cooker#cook` is left: {targets}"
        );
    }

    #[test]
    fn a_block_written_def_is_not_a_member_of_every_receiver_and_a_top_level_one_is() {
        // Both `def`s here are filed on `Object` by rubydex (neither has an enclosing `class`,
        // `module` or `class << self`), so both answer every receiver and both come back from
        // `resolve_call`'s root arm as **precise**. Only one is really a root member.
        //
        // The fence does not reach this: `app/lib/` is neither a test tree nor a migration. What
        // decides is where the `def` is written; see `Blocks`.
        let mut harness = Harness::new();
        let real = harness.write(
            "app/lib/cooker.rb",
            "class Cooker\n  def cook(raw)\n  end\nend\n",
        );
        let patches = harness.write(
            "app/lib/patches.rb",
            "String.class_eval do\n  def cook(raw, expected)\n  end\nend\n\ndef roast(raw)\nend\n",
        );
        let source = "class Post\n  def bake\n    cook(\"x\")\n    roast(\"y\")\n  end\nend\n";
        let post = harness.write("app/models/post.rb", source);
        harness.index();

        // The `def` inside `class_eval` belongs to `String`, whatever rubydex filed it under, so
        // the name rung's `Cooker#cook` is what is left: the same fall-through as the test tree,
        // with the same honest tier.
        let targets = harness.definition_at(&post, source, "cook(");
        let places: Vec<serde_json::Value> = targets
            .as_array()
            .map(|places| {
                places
                    .iter()
                    .map(|place| place["targetUri"].clone())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            places.contains(&serde_json::json!(real.as_str())),
            "`Cooker#cook` is in the list, which is what the root answer was standing in front \
             of: {targets}"
        );
        // **The block's own `def` stays in the list.** What is withdrawn is the claim that it is a
        // member of *every* receiver, not the fact that someone wrote `def cook`: it really
        // declares `String#cook`, so a call on an unknown receiver could mean it. The name rung
        // lists every `def` of the name and says so on its card, the tier allowed to be wrong (see
        // `by_name`).
        assert_eq!(places.len(), 2, "{targets}");
        let markdown = harness.hover_at(&post, source, "cook(")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            markdown.contains("2 possible definitions")
                && harness
                    .candidates_at(&post, source, "cook(")
                    .contains(&"Cooker#cook".to_owned()),
            "a *Guessed* list holding the right method beats a *Resolved* card naming the wrong \
             one: {markdown}"
        );
        assert!(
            markdown.contains("Guessed from name alone."),
            "and the tier is the honest one: {markdown}"
        );

        // A `def` written as its own body, in the *same file*, answers unchanged. This is what the
        // root arm's comment protects, and why the rule is about a place, not a name.
        let targets = harness.definition_at(&post, source, "roast(");
        assert_eq!(
            targets.as_array().map(|places| places
                .iter()
                .map(|place| place["targetUri"].clone())
                .collect::<Vec<_>>()),
            Some(vec![serde_json::json!(patches.as_str())]),
            "a genuine top-level `def` is still a member of every receiver: {targets}"
        );
        let markdown = harness.hover_at(&post, source, "roast(")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(markdown.contains("Object#roast"), "{markdown}");
    }

    #[test]
    fn one_def_written_as_a_body_of_its_own_keeps_the_whole_declaration() {
        // A name written in both shapes, and which way it is answered. A declaration is every `def`
        // of one name on one owner, so `Object#cook` here is two `def`s, one in a block and one
        // not. The rule withdraws a root answer only where *every* `def` is in a block.
        //
        // The other reading would let one monkey patch in one gem withdraw a name the application
        // really writes at the top level: trading one wrong answer for another.
        let mut harness = Harness::new();
        harness.write(
            "app/lib/cooker.rb",
            "class Cooker\n  def cook(raw)\n  end\nend\n",
        );
        harness.write(
            "app/lib/patches.rb",
            "String.class_eval do\n  def cook(raw, expected)\n  end\nend\n",
        );
        let helpers = harness.write("app/lib/helpers.rb", "def cook(raw)\nend\n");
        let source = "class Post\n  def bake\n    cook(\"x\")\n  end\nend\n";
        let post = harness.write("app/models/post.rb", source);
        harness.index();

        let markdown = harness.hover_at(&post, source, "cook(")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            markdown.contains("Object#cook"),
            "one `def` outside a block keeps the root answer: {markdown}"
        );
        let targets = harness.definition_at(&post, source, "cook(");
        let places: Vec<serde_json::Value> = targets
            .as_array()
            .map(|places| {
                places
                    .iter()
                    .map(|place| place["targetUri"].clone())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            places.contains(&serde_json::json!(helpers.as_str())),
            "and the places are the declaration's own, both of them: {targets}"
        );
    }

    #[test]
    fn a_declaring_document_that_cannot_be_read_keeps_the_record_rubydex_wrote() {
        // **Nothing readable means nothing inside a block**, the direction every refusal here
        // falls: the reread is evidence for *withdrawing* an answer, so an unreadable document
        // leaves rubydex's record standing. `Modifiers::reread` does the same.
        //
        // It is reachable, not defensive: a file deleted since the last settle is still in the
        // graph, gone from disk, and not open in any client.
        //
        // The card, not the jump, because a place needs a `Range`, and an unreadable file has no
        // line numbers either; both answers would be empty and the test would pass for the wrong
        // reason.
        let mut harness = Harness::new();
        harness.write(
            "app/lib/cooker.rb",
            "class Cooker\n  def cook(raw)\n  end\nend\n",
        );
        harness.write(
            "app/lib/patches.rb",
            "String.class_eval do\n  def cook(raw, expected)\n  end\nend\n",
        );
        let source = "class Post\n  def bake\n    cook(\"x\")\n  end\nend\n";
        let post = harness.write("app/models/post.rb", source);
        harness.index();
        std::fs::remove_file(harness.root.path().join("app/lib/patches.rb")).unwrap();

        let markdown = harness.hover_at(&post, source, "cook(")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            markdown.contains("Object#cook") && !markdown.contains("possible definitions"),
            "the root answer stands where the evidence to withdraw it could not be read: \
             {markdown}"
        );
    }

    #[test]
    fn a_constructor_written_inside_a_block_is_not_every_class_constructor() {
        // `Foo.new` never reaches the root arm: it is redirected to `Foo#initialize` first, and
        // `constructor` walks the ancestors itself. So the same misfiling has a second road (a gem
        // writing `def initialize` inside `SQLite3::Database.class_eval` hands `Object#initialize`
        // to every `.new`), and that road must make the same refusal. Most surviving real cases are
        // this.
        let mut harness = Harness::new();
        harness.write(
            "app/lib/patches.rb",
            "String.class_eval do\n  def initialize(*args)\n  end\nend\n",
        );
        harness.write("app/models/widget.rb", "class Widget\nend\n");
        let source = "class Post\n  def build\n    Widget.new\n  end\nend\n";
        let post = harness.write("app/models/post.rb", source);
        harness.index();

        let targets = harness.definition_at(&post, source, "new");
        assert!(
            targets.as_array().is_none_or(Vec::is_empty),
            "`Widget` has no constructor to show, and a gem's monkey patch is not one: {targets}"
        );

        // The redirect itself is untouched: a class writing its own `initialize` as its own body is
        // still where `.new` jumps.
        let real = harness.write(
            "app/models/gadget.rb",
            "class Gadget\n  def initialize(size)\n  end\nend\n",
        );
        let source = "class Post\n  def build\n    Gadget.new(1)\n  end\nend\n";
        let post = harness.write("app/models/post.rb", source);
        harness.index();

        let targets = harness.definition_at(&post, source, "new(");
        assert_eq!(
            targets[0]["targetUri"],
            serde_json::json!(real.as_str()),
            "{targets}"
        );
    }

    #[test]
    fn a_typed_receiver_does_not_walk_back_to_a_block_written_def() {
        // **The third road to the same mis-attribution, which the other two gates do not cover.**
        // `resolve_call`'s root arm refuses a hit whose every `def` is in a block, and
        // `constructor` refuses the same for `.new`, but both only see receivers *rubydex* named.
        // Where the receiver is **typed** (a constant, an assignment, a signature), the rung below
        // walks from a declaration neither held, reaches the same `def`, and answers `precise`,
        // which `resolve_typed` does not fence either.
        //
        // Without this gate, the fixture answers ***Resolved***, one place, `Object#configure(x)`,
        // in a file whose `def` is inside someone's `String.class_eval`.
        let mut harness = Harness::new();
        harness.write(
            "app/lib/patches.rb",
            "String.class_eval do\n  def self.configure(x)\n  end\nend\n",
        );
        harness.write(
            "app/models/widget.rb",
            "class Widget\n  def initialize\n  end\n\n  def spin(x)\n  end\nend\n",
        );
        let source = "class Post\n  def build\n    Widget.configure(1)\n  end\n\n  def turn\n                          Widget.new.spin(1)\n  end\nend\n";
        let post = harness.write("app/models/post.rb", source);
        harness.index();

        let markdown = harness.hover_at(&post, source, "configure(1)");
        let markdown = markdown["contents"]["value"].as_str().unwrap_or_default();
        assert!(
            markdown.contains("Guessed from name alone"),
            "a block's `def` is not `Widget`'s, whoever named the receiver: {markdown}"
        );

        // **The place is unchanged, and that is the honest half.** The name rung claims only *some
        // `def` with this name*, and `String.class_eval { def self.configure }` really writes one.
        // What is withdrawn is the tier and the membership claim, as on the other two roads.
        let places = harness.definition_at(&post, source, "configure(1)");
        assert_eq!(places.as_array().map_or(0, Vec::len), 1, "{places}");

        // The rung itself is untouched: a member the receiver really has is still the precise
        // answer for a typed receiver.
        let markdown = harness.hover_at(&post, source, "spin(1)");
        let markdown = markdown["contents"]["value"].as_str().unwrap_or_default();
        assert!(
            markdown.contains("Widget#spin") && !markdown.contains("Guessed from name alone"),
            "the derived rung still answers what the class declares: {markdown}"
        );

        // **Two different refusals on this rung; only one is this rule.** A top-level `def` is a
        // *private* method of `Object` (Ruby's rule, which rubydex records), so an explicit
        // receiver may not reach it, block or not. That refusal belongs to the privacy gate. It is
        // asserted here so the withdrawal above is not mistaken for it, since both sit on the same
        // arm.
        harness.write("app/lib/tools.rb", "def polish(x)\nend\n");
        let source = "class Post\n  def sand\n    Widget.new.polish(1)\n  end\nend\n";
        let post = harness.write("app/models/post.rb", source);
        harness.index();

        let places = harness.definition_at(&post, source, "polish(1)");
        assert!(
            places.as_array().is_none_or(Vec::is_empty),
            "a top-level `def` is private to `Object` and an explicit receiver cannot call it: \
             {places}"
        );
    }

    #[test]
    fn a_cursor_in_a_testing_support_tree_keeps_the_answer_the_suite_declares() {
        // The cursor gate is wider than the target tag on purpose. an engine monorepo ships Ruby files under a
        // `testing_support` segment that is in no test tree (shared examples and factories under
        // `core/lib/spree/testing_support/`, for other people's suites), and a developer reading
        // one of those is exactly who a spec's `def` answers.
        let mut harness = Harness::new();
        let spec = harness.write("spec/support/factory.rb", "def create(kind)\nend\n");
        let source = "def build_one\n  create(:order)\nend\n";
        let shared = harness.write("lib/spree/testing_support/shared.rb", source);
        harness.index();

        let targets = harness.definition_at(&shared, source, "create(");
        assert_eq!(
            targets[0]["targetUri"],
            serde_json::json!(spec.as_str()),
            "{targets}"
        );
    }

    #[test]
    fn a_signature_is_a_place_only_where_no_source_is() {
        // An `.rbs` says what a method takes and returns; nobody wrote the method there, and a jump
        // there lands in a stub. So it stands aside wherever source survives beside it, and stands
        // where none does, since a signature beats silence. Both halves occur often in the corpora.
        let (dir, root) = unbundled("");
        let core = dir.path().join("sig/core");
        std::fs::create_dir_all(&core).unwrap();
        std::fs::write(
            core.join("core.rbs"),
            "class Thing\n  def spin: () -> void\nend\n\nclass OnlySigned\nend\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                format!("{root}/sig")
            ),
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let written = harness.write("lib/thing.rb", REOPENED);
        let source = "Thing.new\nOnlySigned.new\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();
        harness.index_gems();

        let targets = harness.definition_at(&caller, source, "Thing");
        assert_eq!(targets.as_array().unwrap().len(), 1, "{targets}");
        assert_eq!(targets[0]["targetUri"], serde_json::json!(written.as_str()));

        let targets = harness.definition_at(&caller, source, "OnlySigned");
        assert_eq!(targets.as_array().unwrap().len(), 1, "{targets}");
        assert!(
            targets[0]["targetUri"]
                .as_str()
                .expect("a uri")
                .ends_with("core.rbs"),
            "a signature is better than silence: {targets}"
        );
    }

    #[test]
    fn a_copy_the_project_would_not_load_is_not_a_second_place() {
        // Two files at the same require-relative path under two load paths are one file to
        // `require "thing"`: `lib/` is searched first, so `app/`'s copy can never be loaded. In a
        // real bundle this is a pinned `cgi` against Ruby's own copy, which needs a Ruby
        // installation to set up; this tests the same rule on the two load paths the default config
        // names.
        let (dir, _) = unbundled("");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let loaded = harness.write("lib/thing.rb", REOPENED);
        harness.write("app/thing.rb", REOPENED);
        let source = "Thing.new\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();
        harness.index_gems();

        let targets = harness.definition_at(&caller, source, "Thing");
        assert_eq!(targets.as_array().unwrap().len(), 1, "{targets}");
        assert_eq!(targets[0]["targetUri"], serde_json::json!(loaded.as_str()));

        let markdown = harness.hover_at(&caller, source, "Thing")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(
            !markdown.contains("Defined in"),
            "the count is the list, and the list is one: {markdown}"
        );
    }

    #[test]
    fn two_files_on_no_load_path_are_two_places() {
        // The other half of the rule, and why it is keyed on the load path, not a file name: a
        // project's `db/` and `script/` are indexed but are not load paths, so two `thing.rb` files
        // there are not copies of each other. Both are real places and the card counts both.
        let (dir, _) = unbundled("");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("db/thing.rb", REOPENED);
        harness.write("script/thing.rb", REOPENED);
        let source = "Thing.new\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();
        harness.index_gems();

        let targets = harness.definition_at(&caller, source, "Thing");
        assert_eq!(targets.as_array().unwrap().len(), 2, "{targets}");

        // A class's card does not count its places (decided 2026-09-29); the jump lists them.
        let markdown = harness.hover_at(&caller, source, "Thing")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(!markdown.contains("Defined in"), "{markdown}");
    }

    #[test]
    fn a_document_the_editor_cannot_open_is_not_a_place() {
        // rubydex declares Ruby's object model under `rubydex:built-in`, and
        // `DocUri::from_graph_uri` refuses that URI for every request, so a jump always dropped it.
        // The count must drop it too, or the card reads `Defined in 18 places` over a jump offering
        // 17.
        let (dir, _) = unbundled("");
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let reopened = harness.write("lib/ext.rb", "class Object\n  def blank?\n  end\nend\n");
        let source = "Object.new\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();
        harness.index_gems();

        let targets = harness.definition_at(&caller, source, "Object");
        assert_eq!(targets.as_array().unwrap().len(), 1, "{targets}");
        assert_eq!(
            targets[0]["targetUri"],
            serde_json::json!(reopened.as_str())
        );

        let markdown = harness.hover_at(&caller, source, "Object")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(
            !markdown.contains("Defined in"),
            "a built-in is not a place the count may carry: {markdown}"
        );
    }

    #[test]
    fn a_member_is_looked_up_under_the_spelling_rubydex_filed_it_by() {
        // rubydex keys *members* by the parenthesised `shout()` but records a call as bare `shout`,
        // so every method lookup goes through here. An empty graph holds neither spelling, and must
        // answer `None` rather than fabricate the parenthesised form from an absent interned
        // string.
        let graph = Graph::new();
        assert_eq!(member_name(&graph, StringId::from("shout")), None);
    }

    #[test]
    fn goto_definition_follows_a_call_with_a_known_receiver() {
        let mut harness = Harness::new();
        let library = harness.write("lib/person.rb", LIBRARY);
        let source = "Person.build(\"x\")\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "build");
        assert_eq!(targets[0]["targetUri"], serde_json::json!(library.as_str()));
        // The name, not the whole method body, is where the cursor should land.
        assert_eq!(targets[0]["targetSelectionRange"]["start"]["line"], 9);
        assert_eq!(targets.as_array().unwrap().len(), 1, "{targets}");
    }

    #[test]
    fn a_cursor_on_a_name_is_not_the_operator_one_byte_in_front_of_it() {
        // `!shout` records two calls over adjoining bytes: `!` over the bang, `shout` over the
        // name. `covers` is end-inclusive, so with the cursor on the `s` both reach it, and width
        // alone would pick the bang (one byte against five), answering every `#!` in the graph for
        // a cursor on a visible method.
        let mut harness = Harness::new();
        let library = harness.write("lib/person.rb", LIBRARY);
        let source = "class Person\n  def loud?\n    !shout\n  end\nend\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "shout");
        assert_eq!(targets[0]["targetUri"], serde_json::json!(library.as_str()));
        assert_eq!(targets.as_array().unwrap().len(), 1, "{targets}");
    }

    #[test]
    fn a_cursor_parked_just_past_a_name_still_answers_it() {
        // The other half, and why `covers` is end-inclusive at all: a span that only *ends* at the
        // cursor stays a candidate and merely loses to one that *begins* there. Nothing begins
        // here, so `build` wins. That also keeps `a.b += c` answering, where rubydex records the
        // call over the `.` and no span covers the message.
        let mut harness = Harness::new();
        let library = harness.write("lib/person.rb", LIBRARY);
        let source = "Person.build(\"x\")\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "(\"x\")");
        assert_eq!(targets[0]["targetUri"], serde_json::json!(library.as_str()));
        assert_eq!(targets[0]["targetSelectionRange"]["start"]["line"], 9);
    }

    #[test]
    fn goto_definition_on_a_constant_ignores_the_synthetic_singleton_reference() {
        // rubydex records a *second*, invented constant reference over the same bytes as `Person`
        // in `Person.build`, pointing at the singleton class, so the call can resolve. Following it
        // would jump to the wrong thing, or, for an implicit receiver, to a `class << self` block
        // elsewhere in the file.
        let mut harness = Harness::new();
        let library = harness.write("lib/person.rb", LIBRARY);
        let source = "Person.build(\"x\")\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "Person");
        assert_eq!(
            targets.as_array().unwrap().len(),
            2,
            "both `class Person`: {targets}"
        );
        for target in targets.as_array().unwrap() {
            assert_eq!(target["targetUri"], serde_json::json!(library.as_str()));
            assert_eq!(target["targetSelectionRange"]["start"]["character"], 6);
        }
    }

    #[test]
    fn new_navigates_to_the_constructor_rather_than_to_class_new() {
        // `Foo.new` really is `Class#new`, so the exact answer is a signature file nobody asked
        // for. Goto-definition and hover both redirect to the constructor; hover matters most,
        // because it supplies the parameter list.
        let mut harness = Harness::new();
        let library = harness.write("lib/shop.rb", CONSTRUCTORS);
        let source = "Money.new(1)\nRegistry.new\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "new(1)");
        assert_eq!(targets.as_array().unwrap().len(), 1, "{targets}");
        assert_eq!(targets[0]["targetUri"], serde_json::json!(library.as_str()));
        assert_eq!(
            targets[0]["targetSelectionRange"]["start"]["line"], 1,
            "the `initialize` on line 2, not the class: {targets}"
        );

        let markdown = harness.hover_at(&caller, source, "new(1)")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Money#initialize(cents)"), "{markdown}");
        assert!(
            !markdown.contains("Guessed from name alone"),
            "a redirect is still exact: {markdown}"
        );

        // A class that writes its own `new` is reached by that method, and `initialize` is one
        // `super` further on. Redirecting would skip the code the call actually runs.
        let markdown = harness.hover_at(&caller, source, "new\n")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Registry.new(*args)"), "{markdown}");
    }

    #[test]
    fn a_class_with_no_constructor_keeps_the_honest_answer() {
        // Every object inherits `BasicObject#initialize`, so with rbs indexed there is always *an*
        // `initialize` to redirect to. For a class that defines none, it is as useless as
        // `Class#new` and less true. The guard keeps the redirect meaningful.
        let dir = tempfile::tempdir().expect("tempdir");
        let core = dir.path().join("sig/core");
        std::fs::create_dir_all(&core).unwrap();
        std::fs::write(
            core.join("core.rbs"),
            "\
class BasicObject
  def initialize: () -> void
end

class Class
  def new: (*untyped) -> untyped
end
",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n",
                dir.path().join("sig").display().to_string()
            ),
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("lib/shop.rb", CONSTRUCTORS);
        let source = "Plain.new\nMoney.new(1)\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();
        harness.index_gems();

        assert!(
            harness.has("BasicObject#initialize()"),
            "the signature root was not indexed"
        );

        let markdown = harness.hover_at(&caller, source, "new\n")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(
            markdown.contains("Class#new"),
            "an inherited empty constructor is not a constructor to redirect to: {markdown}"
        );

        // The guard has not turned the redirect off for everyone.
        let markdown = harness.hover_at(&caller, source, "new(1)")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Money#initialize(cents)"), "{markdown}");
    }

    #[test]
    fn a_call_on_a_local_is_answered_as_a_guess() {
        // Without type inference `thing.shout` can only be matched by name. Still worth answering
        // (usually right), but it must not be presented as certain.
        let mut harness = Harness::new();
        harness.write("lib/person.rb", LIBRARY);
        let source = "thing = something\nthing.shout\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "shout");
        assert_eq!(targets.as_array().unwrap().len(), 1, "{targets}");

        let markdown = harness.hover_at(&caller, source, "shout")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Person#shout"), "{markdown}");
        assert!(markdown.contains("Guessed from name alone."), "{markdown}");
    }

    #[test]
    fn several_classes_with_the_same_method_are_listed_rather_than_guessed_between() {
        let mut harness = Harness::new();
        harness.write("lib/a.rb", "class Alpha\n  def ping; end\nend\n");
        harness.write("lib/b.rb", "class Beta\n  def ping; end\nend\n");
        let source = "thing.ping\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        let markdown = harness.hover_at(&caller, source, "ping")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("2 possible definitions"), "{markdown}");
        assert_eq!(
            harness.candidates_at(&caller, source, "ping"),
            ["Alpha#ping", "Beta#ping"]
        );
    }

    #[test]
    fn a_class_object_is_never_answered_with_an_instance_method_of_a_class() {
        // Five wrong answers in one application. `Settings::General.app_domain` is made by a gem's `setting`
        // macro, which nothing reads, so the name rung ran; the privacy gate refused a private
        // `app_domain` in an unrelated class, and the one left was the configuration's reader. A
        // class object reaches no class's instance method, so the answer is nothing, even where
        // nothing else shares the name.
        let mut harness = Harness::new();
        harness.write(
            "lib/settings.rb",
            "class Settings\n  setting :app_domain\nend\n",
        );
        harness.write(
            "lib/renderer.rb",
            "class Renderer\n  private\n\n  def app_domain\n  end\nend\n",
        );
        harness.write(
            "lib/configuration.rb",
            "class Configuration\n  def app_domain\n  end\nend\n",
        );
        let source = "Settings.app_domain\n";
        let caller = harness.write("lib/mailer.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "app_domain");
        assert!(targets.is_null(), "{targets}");
        let card = harness.hover_at(&caller, source, "app_domain");
        assert!(card.is_null(), "{card}");
    }

    #[test]
    fn a_statement_of_a_body_is_certain_of_its_class_object_and_a_block_in_it_is_not() {
        // The same bare word twice. As a statement, `self` is `Story`'s class object and nothing
        // can change that, so another class's instance method is no answer. In a block, whoever
        // receives the block may run it against a `Paginator`, so the name match is still a guess
        // worth making.
        let mut harness = Harness::new();
        harness.write(
            "lib/paginator.rb",
            "class Paginator\n  def paginates_per(count)\n  end\nend\n",
        );
        let source = "class Story\n  paginates_per 10\n  [1].each { paginates_per 20 }\nend\n";
        let caller = harness.write("lib/story.rb", source);
        harness.index();

        let targets = harness.definition_at(&caller, source, "paginates_per 10");
        assert!(targets.is_null(), "{targets}");

        let targets = harness.definition_at(&caller, source, "paginates_per 20");
        let found = targets.as_array().expect("link targets");
        assert_eq!(found.len(), 1, "{targets}");
        let card = harness.hover_at(&caller, source, "paginates_per 20")["contents"]["value"]
            .as_str()
            .unwrap_or("null")
            .to_owned();
        assert!(card.contains("Paginator#paginates_per"), "{card}");
        assert!(card.contains("Guessed from name alone"), "{card}");
    }

    /// A class whose body writes blocks, the shape of every Ruby DSL: Parslet's
    /// `rule(:colon) { str(':') }`, Rails' `scope :recent, -> { … }`, a concern's `included do`.
    ///
    /// `Base` owns `solo` as an ordinary instance method, and an unrelated module owns the same
    /// name: the pair the name rung gets backwards, dropping the class's (a class object provably
    /// cannot reach it) and keeping the module's.
    const CLOSURES: &str = "\
class Thing < Base
  include Helpers
  extend ClassHelpers

  [1].each { only_instance }
  [1].each { only_class }
  [1].each { on_both }
  [1].each { solo }
  only_instance

  def self.run
    [1].each { only_instance if true }
  end
end
";

    fn closures() -> (Harness, DocUri) {
        let mut harness = Harness::new();
        harness.write(
            "lib/helpers.rb",
            "module Helpers\n  def only_instance\n  end\n  def on_both\n  end\nend\n",
        );
        harness.write(
            "lib/class_helpers.rb",
            "module ClassHelpers\n  def only_class\n  end\n  def on_both\n  end\nend\n",
        );
        harness.write("lib/base.rb", "class Base\n  def solo\n  end\nend\n");
        harness.write(
            "lib/unrelated.rb",
            "module Unrelated\n  def solo\n  end\nend\n",
        );
        let caller = harness.write("lib/thing.rb", CLOSURES);
        harness.index();
        (harness, caller)
    }

    fn closure_card(harness: &mut Harness, caller: &DocUri, needle: &str) -> String {
        harness.hover_at(caller, CLOSURES, needle)["contents"]["value"]
            .as_str()
            .unwrap_or("null")
            .to_owned()
    }

    #[test]
    fn a_closure_in_a_class_body_reaches_the_instance_side() {
        // rubydex has no notion of a block, so a bare call inside one in a class body gets the
        // receiver a statement of that body gets: the singleton class. Right for a block nobody
        // rebinds, wrong for every DSL block, and the class object has no `only_instance`, so
        // without this rung the answer falls to the name rung.
        let (mut harness, caller) = closures();

        let card = closure_card(&mut harness, &caller, "only_instance }");
        assert!(card.contains("Helpers#only_instance"), "{card}");
        // The rung it replaces is the one allowed to be wrong, and the card must stop saying so.
        assert!(!card.contains("Guessed from name alone"), "{card}");

        // The jump gives the same answer: both requests go through one function so they cannot
        // disagree, and this is a case where a card could have improved without navigation
        // following.
        let targets = harness.definition_at(&caller, CLOSURES, "only_instance }");
        let found = targets.as_array().expect("link targets");
        assert_eq!(found.len(), 1, "{targets}");
        assert!(
            found[0]["targetUri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("lib/helpers.rb")),
            "{targets}"
        );
    }

    #[test]
    fn a_closure_answers_the_class_object_first_where_both_scopes_have_the_name() {
        // The singleton side is searched first, and this rung runs only if it found nothing, so a
        // name on both keeps the lexical, *resolved* answer. Nothing to derive: the file as written
        // would run.
        let (mut harness, caller) = closures();

        let card = closure_card(&mut harness, &caller, "on_both }");
        assert!(card.contains("ClassHelpers#on_both"), "{card}");
        assert!(!card.contains("Found on an instance"), "{card}");

        // The control: a name only the class object has, which never needed this rung.
        let card = closure_card(&mut harness, &caller, "only_class }");
        assert!(card.contains("ClassHelpers#only_class"), "{card}");
        assert!(!card.contains("Found on an instance"), "{card}");
    }

    #[test]
    fn a_closure_reaches_a_method_its_superclass_owns() {
        // The case that was *wrong*, not merely vague. `reachable_on_a_class_object` drops every
        // candidate a `class` owns (correct for a class object, applied to a cursor whose `self` is
        // not one), so the name rung would throw away `Base#solo` and answer an unrelated module's
        // `solo`.
        let (mut harness, caller) = closures();

        let card = closure_card(&mut harness, &caller, "solo }");
        assert!(card.contains("Base#solo"), "{card}");
        assert!(!card.contains("Unrelated#solo"), "{card}");
        assert!(!card.contains("Guessed from name alone"), "{card}");
    }

    #[test]
    fn a_block_in_a_module_body_keeps_the_guess_it_had() {
        // A module has no instances, so this rung's sentence is not true of one. A module body's
        // blocks are `included do` and `class_methods do`, which Rails runs against the **including
        // class**, not the module or anything reachable from it. The name rung keeps the same
        // declaration; it just does not call it derived.
        let mut harness = Harness::new();
        let source = "\
module Countable
  included do
    recount
  end

  def recount
  end
end
";
        let caller = harness.write("lib/countable.rb", source);
        harness.index();

        let card = harness.hover_at(&caller, source, "recount\n  end")["contents"]["value"]
            .as_str()
            .unwrap_or("null")
            .to_owned();
        assert!(card.contains("Countable#recount"), "{card}");
        assert!(card.contains("Guessed from name alone"), "{card}");
        assert!(!card.contains("Found on an instance"), "{card}");
    }

    #[test]
    fn a_block_in_a_module_body_does_not_run_against_the_module() {
        // Rails runs `included do` against each including class and the callback block
        // inside it against a record, so the module's class object is never `self` here. The name
        // list is not narrowed to what that object reaches.
        let source = "\
module Stamped
  included do
    after_initialize do
      self.stamp ||= 1
      self.stamp
      stamp
    end
  end

  def self.helper; end
  included do
    Stamped.helper
  end
end

class Visit
  include Stamped
  def stamp; end
end

module Other
  def self.stamp; end
end

module Orphaned
  included do
    self.stamp
  end
end
";
        let mut harness = Harness::new();
        let uri = harness.write("app/models/stamped.rb", source);
        harness.index();

        // The last is a concern nothing includes: its block's `self` is refused, not guessed at.
        for needle in [
            "stamp ||=",
            "stamp\n      stamp",
            "stamp\n    end",
            "stamp\n  end\nend\n",
        ] {
            let card = harness.hover_at(&uri, source, needle)["contents"]["value"]
                .as_str()
                .unwrap_or("null")
                .to_owned();
            let named = harness.candidates_at(&uri, source, needle);
            assert!(
                named.contains(&"Visit#stamp".to_owned()),
                "{needle}: {named:?}"
            );
            assert!(card.contains("Guessed from name alone"), "{needle}: {card}");
        }
        // Completion after that `self.` agrees with the card: a name match, not the module object's
        // members offered as the receiver's.
        let offered = harness.complete(
            &uri,
            &source.replacen("self.stamp ||=", "self.~stamp ||=", 1),
        );
        let items = offered["items"].as_array().cloned().unwrap_or_default();
        assert!(!items.is_empty(), "{offered}");
        assert!(
            items.iter().all(|item| item["data"]["precise"] == false),
            "{offered}"
        );
        // Written out, the module is the receiver, whatever block the call sits in.
        let explicit = harness.hover_at(&uri, source, "helper\n  end\nend");
        assert!(
            explicit.to_string().contains("Stamped.helper"),
            "{explicit}"
        );
        assert!(!explicit.to_string().contains("name alone"), "{explicit}");
    }

    #[test]
    fn a_statement_in_a_class_body_is_not_a_closure_and_a_block_in_a_def_is_not_either() {
        // Both of these have a `self` nothing can rebind: a body statement runs with the class
        // object, and a block inside a `def` closes over the method's `self`. A name only an
        // instance has is unreachable from both, so the honest answer is the guess.
        let (mut harness, caller) = closures();

        for needle in ["only_instance\n", "only_instance if true"] {
            let card = closure_card(&mut harness, &caller, needle);
            assert!(card.contains("Helpers#only_instance"), "{needle}: {card}");
            assert!(card.contains("Guessed from name alone"), "{needle}: {card}");
            assert!(!card.contains("Found on an instance"), "{needle}: {card}");
        }
    }

    #[test]
    fn a_call_on_a_constant_that_was_never_declared_still_answers() {
        // `Nowhere` is a receiver rubydex can name but not resolve: normal mid-refactor, and for
        // every constant from an unindexed gem. The precise path must stand down rather than
        // resolve against a missing owner.
        let mut harness = Harness::new();
        let person = harness.write(
            "app/person.rb",
            "class Person\n  def frobnicate\n  end\nend\n",
        );
        let source = "Nowhere.frobnicate\n";
        let main = harness.write("app/main.rb", source);
        harness.index();

        // Name-based, so the one declaration spelled this way is still the answer, and *which*
        // declaration is the assertion: "not null" would pass just as happily on a jump to the
        // wrong file, the failure this degradation risks.
        let found = harness.definition_at(&main, source, "frobnicate");
        let targets = found.as_array().expect("link targets");
        assert_eq!(targets.len(), 1, "{found}");
        assert_eq!(targets[0]["targetUri"], serde_json::json!(person.as_str()));
        assert_eq!(
            targets[0]["targetSelectionRange"]["start"],
            serde_json::json!({ "line": 1, "character": 6 }),
            "the name in `def frobnicate`, not the class or the body: {found}"
        );
    }

    #[test]
    fn a_singleton_def_that_names_its_class_declares_a_singleton_method() {
        // `def Functions::floor` instead of `def self.floor`, as rexml writes its whole XPath
        // surface. rubydex attributes a named receiver through the plain-`def` arm, looks the name
        // up as an instance member, and misses.
        let mut harness = Harness::new();
        let source = "module Functions\n  def Functions::floor(number)\n  end\nend\n";
        let unit = harness.write("lib/functions.rb", source);
        harness.index();

        let markdown = harness.hover_at(&unit, source, "floor")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Functions.floor(number)"), "{markdown}");
    }

    #[test]
    fn a_singleton_def_that_names_its_class_is_not_the_instance_method_of_that_name() {
        // The same miss where the namespace happens to declare an instance method of that name: the
        // lookup then lands on a different method, and the card says so with no hedge. That is why
        // a named receiver is answered outright, before rubydex can answer wrongly.
        let mut harness = Harness::new();
        let source =
            "module Functions\n  def twin\n  end\n\n  def Functions::twin(node)\n  end\nend\n";
        let unit = harness.write("lib/functions.rb", source);
        harness.index();

        let markdown = harness.hover_at(&unit, source, "twin(node)")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Functions.twin(node)"), "{markdown}");
        assert!(!markdown.contains("Functions#twin"), "{markdown}");
    }

    #[test]
    fn a_body_opened_under_an_aliased_name_declares_on_the_namespace_the_alias_names() {
        // Ruby ships `Gem::URI = Bundler::URI`, and the vendored copy writes `module Gem::URI`. A
        // body's members are attributed by the body's *name*, and this name resolves to the alias,
        // which has no members and no singleton class, so the walk stops there.
        let mut harness = Harness::new();
        let bundler = "module Bundler\n  module URI\n  end\nend\n";
        let aliased = "module Gem\nend\n\nGem::URI = Bundler::URI\n";
        let common = "\
module Gem::URI
  def escape(uri)
  end

  alias unescape escape
  attr_reader :port

  def self.split(uri)
  end

  def outer
    def inner
    end
  end

  module Schemes
  end

  ELSEWHERE = 1
end
";
        // `self.alias_method` is the second receiver a `MethodAlias` can have, and upstream pairs
        // it with the *instance* member, as `Module#alias_method` does.
        let reopened = "module Gem::URI\n  self.alias_method :encode, :escape\nend\n";
        harness.write("lib/bundler_uri.rb", bundler);
        harness.write("lib/gem_uri.rb", aliased);
        let reopened_uri = harness.write("lib/reopened.rb", reopened);
        let unit = harness.write("lib/uri_common.rb", common);
        harness.index();

        let card = |harness: &mut Harness, uri: &DocUri, source: &str, needle: &str| -> String {
            harness.hover_at(uri, source, needle)["contents"]["value"]
                .as_str()
                .unwrap_or_else(|| panic!("no hover on {needle:?}"))
                .to_owned()
        };

        // Every member the body declares by being inside it: the kinds owned by the body's *name*,
        // not a name of their own.
        assert!(
            card(&mut harness, &unit, common, "escape(uri)").contains("Bundler::URI#escape(uri)")
        );
        assert!(card(&mut harness, &unit, common, "unescape").contains("Bundler::URI#unescape"));
        assert!(card(&mut harness, &unit, common, "port").contains("Bundler::URI#port"));
        assert!(
            card(&mut harness, &unit, common, "split(uri)").contains("Bundler::URI.split(uri)")
        );
        assert!(
            card(&mut harness, &reopened_uri, reopened, "encode").contains("Bundler::URI#encode")
        );

        // A `def` inside a `def` is why the walk is a *walk*: the recorded nesting is the outer
        // method, which names nothing, so the name comes from two steps out.
        assert!(card(&mut harness, &unit, common, "inner").contains("Bundler::URI#inner"));

        // The two that were never wrong, showing the reverse map alone is broken: a nested
        // namespace and a constant carry their own names and already resolve.
        assert!(
            card(&mut harness, &unit, common, "Schemes").contains("module Bundler::URI::Schemes")
        );
        assert!(card(&mut harness, &unit, common, "ELSEWHERE").contains("Bundler::URI::ELSEWHERE"));
    }

    #[test]
    fn a_named_receiver_that_is_an_alias_declares_on_the_class_the_alias_names() {
        // Ruby 4 ships `Ripper = Prism::Translation::Ripper` in `prism/translation/ripper/shim.rb`,
        // and `ripper/core.rb` writes `def Ripper.parse`. So the named receiver needs the retry
        // too: its own lookup asks the name for a namespace, and the name is an alias. The retry
        // runs only where that found nothing, which is why `Thing = 1` below still answers nothing.
        let mut harness = Harness::new();
        let translation =
            "module Prism\n  module Translation\n    class Ripper\n    end\n  end\nend\n";
        let shim = "Ripper = Prism::Translation::Ripper\n";
        let source = "def Ripper.parse(src)\nend\n";
        harness.write("lib/prism_translation.rb", translation);
        harness.write("lib/shim.rb", shim);
        let unit = harness.write("lib/ripper_core.rb", source);
        harness.index();

        let markdown = harness.hover_at(&unit, source, "parse(src)")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(
            markdown.contains("Prism::Translation::Ripper.parse(src)"),
            "{markdown}"
        );
    }

    #[test]
    fn an_alias_that_leads_back_to_itself_answers_nothing_rather_than_spinning() {
        // Ruby allows `Fore = Aft` beside `Aft = Fore`, and neither becomes a namespace. The walk
        // is capped, so a cursor on a `def self.` under one costs a fixed number of lookups, then
        // gives up.
        let mut harness = Harness::new();
        let fore = "Fore = Aft\n";
        let source = "Aft = Fore\nclass Aft\n  def self.spin; end\nend\n";
        harness.write("lib/fore.rb", fore);
        let unit = harness.write("lib/aft.rb", source);
        harness.index();

        assert!(harness.definition_at(&unit, source, "spin").is_null());
    }

    #[test]
    fn a_named_receiver_that_is_not_a_class_answers_nothing_rather_than_something_near_it() {
        // `def Thing.bar` on a constant holding a number. There is no singleton class to ask, and
        // the enclosing namespace's `bar` (if any) is not what was written. The only honest answer
        // is none; above all, no jump.
        let mut harness = Harness::new();
        let source = "Thing = 1\ndef Thing.bar\nend\n";
        let unit = harness.write("lib/thing.rb", source);
        harness.index();

        assert!(
            harness.definition_at(&unit, source, "bar").is_null(),
            "a constant holding an Integer has no singleton class to declare `bar` on"
        );
    }

    #[test]
    fn a_namespace_the_resolver_invented_is_not_somewhere_to_jump() {
        // `Missing::Thing` makes the resolver record a `Missing` it never saw defined: a
        // placeholder with no definitions, so listing it in the picker would offer a destination
        // that does not exist.
        let mut harness = Harness::new();
        let uri = harness.write("app/main.rb", "Missing::Thing.new\n");
        harness.index();

        // Nor a hover. The cursor is on a real reference, but it resolves to a name the resolver
        // invented, and a card saying `Missing` over `Missing` tells the reader only that the
        // server knows nothing either. Silence says the same in no space.
        let source = "Missing::Thing.new\n";
        assert!(harness.hover_at(&uri, source, "Missing").is_null());

        let names: Vec<String> = harness
            .ask(
                "workspace/symbol",
                serde_json::json!({ "query": "Missing" }),
            )
            .as_array()
            .into_iter()
            .flatten()
            .map(|symbol| symbol["name"].as_str().unwrap_or("?").to_owned())
            .collect();
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn a_file_the_index_never_took_answers_nothing_rather_than_guessing() {
        // `index.include` is `**/*.rb`, so a Ruby-looking buffer with another extension has text on
        // disk and no graph document. Every request must survive that: the text is readable, so
        // only the graph lookup can tell.
        let mut harness = Harness::new();
        let source = "class Person\nend\n";
        let uri = harness.write("app/notes.txt", source);
        harness.index();

        assert!(harness.outline(&uri).is_null(), "{}", harness.outline(&uri));
        assert!(harness.hover_at(&uri, source, "Person").is_null());
        assert!(
            harness
                .ask(
                    "textDocument/definition",
                    serde_json::json!({
                        "textDocument": { "uri": uri.as_str() },
                        "position": position_of(source, "Person"),
                    }),
                )
                .is_null()
        );
    }

    #[test]
    fn a_recovered_span_that_does_not_contain_its_name_is_widened_to_fit() {
        // The other half of `a_half_typed_def_does_not_take_the_outline_down_with_it`. The outline
        // drops a nameless `def` before building a range; goto-definition does not, since a name
        // lookup per definition would cost every hover in the file. So `locator::spans` is the one
        // place the containment rule is enforced, and this request reaches it.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();

        // Asked of `spans` directly: containment holds for every pair it returns, and the requests
        // carrying one filter half-typed definitions out earlier.
        let mut broken = 0;
        for source in [
            "class A\n def\n",
            "class A\n def \n",
            "def \n",
            "class A\n  private def \n",
            "module M\n  class B\n    def\n",
        ] {
            harness.change(&uri, source);
            for definition in harness.analysis.graph.definitions().values() {
                let (full, selection) = locator::spans(definition);
                assert!(
                    selection.0 >= full.0 && selection.1 <= full.1,
                    "{source:?}: selection {selection:?} escapes {full:?}"
                );
                if definition.name_offset().is_some_and(|name| {
                    let raw = definition.offset();
                    name.start() < raw.start() || name.end() > raw.end()
                }) {
                    // The shape VS Code threw on: Prism recovered `def` into a node spanning the
                    // three keyword bytes, with its name span in the whitespace after them.
                    assert_eq!(
                        selection, full,
                        "{source:?}: a name outside the span widens"
                    );
                    broken += 1;
                }
            }
        }
        assert!(
            broken > 0,
            "no fixture here recovered the pair the rule exists for"
        );
    }

    #[test]
    fn an_alias_is_navigable_from_its_call_sites() {
        // rubydex records a method call under its bare name, except an `alias`, recorded with
        // parentheses. Both must reach the same member key, or the lookup misses and `yell`
        // navigates nowhere.
        let mut harness = Harness::new();
        let library = "class Person\n  def shout\n  end\n  alias yell shout\nend\n";
        let person = harness.write("app/person.rb", library);
        let source = "Person.new.yell\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        let link = harness.ask(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": position_of(source, "yell"),
            }),
        );
        assert!(!link.is_null(), "an alias call site navigates nowhere");
        assert_eq!(link[0]["targetUri"], person.as_str(), "{link}");

        // From inside the `alias` statement itself: the reference rubydex records with parentheses.
        let from_alias = harness.ask(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": person.as_str() },
                "position": position_of(library, "shout\nend"),
            }),
        );
        assert!(!from_alias.is_null(), "{from_alias}");
    }

    #[test]
    fn a_definition_link_never_points_outside_the_construct_it_names() {
        // `LocationLink::targetSelectionRange` has the same containment rule, from the same pair of
        // spans. VS Code does not validate it, so the symptom is quieter (goto parks the cursor on
        // a newline outside the construct), but it is the same defect, fixed in the same place.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();
        let source = "class A\n def\n";
        harness.open(&uri, source);

        // The whitespace after the keyword, where the recovered name span sits.
        let targets = harness.ask(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 1, "character": 4 },
            }),
        );

        let point = |value: &serde_json::Value| {
            (
                value["line"].as_u64().unwrap_or_default(),
                value["character"].as_u64().unwrap_or_default(),
            )
        };
        assert!(
            targets.as_array().is_some_and(|links| !links.is_empty()),
            "nothing to check: {targets}"
        );
        for target in targets.as_array().into_iter().flatten() {
            let (range, selection) = (&target["targetRange"], &target["targetSelectionRange"]);
            assert!(
                point(&selection["start"]) >= point(&range["start"])
                    && point(&selection["end"]) <= point(&range["end"]),
                "{targets}"
            );
        }
    }

    #[test]
    fn a_hover_on_an_untyped_receiver_counts_every_candidate() {
        // With no inference there is nothing to narrow `thing.call` to. Listing all would be a
        // page; the count tells the user it is a guess.
        let mut harness = Harness::new();
        let mut classes = String::new();
        for index in 0..12 {
            classes.push_str(&format!("class Holder{index}\n  def call\n  end\nend\n"));
        }
        harness.write("app/holders.rb", &classes);
        let source = "thing.call\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let markdown = harness.hover_at(&uri, source, "call")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert_eq!(
            markdown,
            "**12 possible definitions**\n\n*Guessed from name alone.*"
        );
    }

    /// A project whose only `def shout` is in its spec tree, with two cursors on `x.shout`: one in
    /// the application, one in a spec.
    fn a_spec_only_method(source: &str) -> (Harness, DocUri, DocUri) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("ya-lsp.toml"), "[gems]\nenabled = false\n").unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "spec/support/loud.rb",
            "module Loud\n  def shout\n    1\n  end\nend\n",
        );
        let from_app = harness.write("app/models/story.rb", source);
        let from_spec = harness.write("spec/models/story_spec.rb", source);
        harness.index();
        (harness, from_app, from_spec)
    }

    #[test]
    fn a_guess_does_not_leave_the_application_for_a_test_tree() {
        // The one fence on the name rung. `x` types to nothing, so `shout` reaches the name list,
        // and the only `def shout` is one RSpec loads and the application never does. A jump from a
        // model into a spec is useless, so the honest answer is nothing.
        let source = "x.shout\n";
        let (mut harness, from_app, from_spec) = a_spec_only_method(source);

        assert!(
            harness.definition_at(&from_app, source, "shout").is_null(),
            "a cursor in app/ was sent into the spec tree"
        );
        assert!(
            harness.hover_at(&from_app, source, "shout").is_null(),
            "and the card followed it"
        );

        // …and the same guess from inside the spec tree keeps it: someone editing a spec is who the
        // `def` in the next spec file answers.
        let definition = harness.definition_at(&from_spec, source, "shout");
        assert!(
            definition[0]["targetUri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("spec/support/loud.rb")),
            "{definition}"
        );
    }

    /// A project whose only `def rollout` is inside a migration, with two cursors on `x.rollout`.
    ///
    /// A class declared at the top of a migration, so a data script runs against the schema of its
    /// day. `db/migrate` is on no autoload path, so nothing outside that file can reach it.
    fn a_migration_only_method(source: &str) -> (Harness, DocUri, DocUri) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("ya-lsp.toml"), "[gems]\nenabled = false\n").unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write(
            "db/migrate/20180528141303_backfill_stories.rb",
            "class BackfillStories
  class Story
    def rollout
      1
    end
  end
end
",
        );
        let from_app = harness.write("app/models/story.rb", source);
        let from_migration = harness.write("db/migrate/20190101000000_later.rb", source);
        harness.index();
        (harness, from_app, from_migration)
    }

    #[test]
    fn a_guess_does_not_leave_the_application_for_a_migration() {
        // The spec-tree fence, one tree on. `x` types to nothing, so `rollout` reaches the name
        // list, and the only `def rollout` is loaded by the migration task, by path, in its own
        // process. Without the fence, such a place can come **first**, which is where the jump
        // lands.
        let source = "x.rollout
";
        let (mut harness, from_app, from_migration) = a_migration_only_method(source);

        assert!(
            harness
                .definition_at(&from_app, source, "rollout")
                .is_null(),
            "a cursor in app/ was sent into a migration"
        );
        assert!(
            harness.hover_at(&from_app, source, "rollout").is_null(),
            "and the card followed it"
        );

        // …and a reader inside a migration keeps it, as a reader in a spec does: the copy at the
        // top of that file is what they are reading.
        let definition = harness.definition_at(&from_migration, source, "rollout");
        assert!(
            definition[0]["targetUri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("20180528141303_backfill_stories.rb")),
            "{definition}"
        );
    }

    /// A jump lands in the same place whether the target is open or not.
    ///
    /// **The equivalence `requests::Analysis::indexed_ranges`' fast path rests on.** An unopened
    /// target is placed from the line index rubydex built when indexing; an open one is read and
    /// rebased. The two must give the same range, or a jump would move depending on whether the
    /// destination is in another tab.
    ///
    /// The declaration follows an emoji and an accent *on its own line*, in all three encodings,
    /// because only there can the two column counts differ: one measures the line's prefix in the
    /// file's text, the other reads what the index remembered.
    #[test]
    fn a_jump_lands_in_the_same_place_whether_the_target_is_open_or_not() {
        let target =
            "class Person\n  ROCKET = \"\u{1f680}\"; CAF\u{c9} = 2; def latte\n    1\n  end\nend\n";
        let source = "Person.new.latte\n";
        for encoding in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            let mut harness = Harness::with_encoding(encoding);
            let person = harness.write("app/person.rb", target);
            let main = harness.write("app/main.rb", source);
            harness.index();

            // The *method*, so the placed declaration follows the emoji and accent on its line. The
            // class above is on plain ASCII, where the encodings agree and the assertion would be
            // vacuous.
            let closed = harness.definition_at(&main, source, "latte");
            assert!(!closed.is_null(), "{encoding:?}: nothing to compare");
            harness.open(&person, target);
            let opened = harness.definition_at(&main, source, "latte");

            assert_eq!(closed, opened, "{encoding:?}");
        }
    }

    /// A jump *into a template* is placed in the markup, not the blanked view.
    ///
    /// The one target `requests::Analysis::indexed_ranges` must decline: a template is indexed as
    /// [`erb::ruby_view`](super::erb::ruby_view), so rubydex's index counts columns in a line whose
    /// markup became spaces, and one `\u{1f600}` in the markup is two UTF-16 units the client has
    /// and the view does not. The declaration follows exactly that.
    #[test]
    fn a_jump_into_a_template_is_placed_in_the_markup_the_client_has() {
        let template = "<p>caf\u{e9} \u{1f600}</p><% VIEW_TOTAL = 1 %>\n";
        let source = "VIEW_TOTAL\n";
        let mut harness = Harness::with_encoding(PositionEncoding::Utf16);
        harness.write("app/views/stories/show.html.erb", template);
        let main = harness.write("app/main.rb", source);
        harness.index();

        let answer = harness.definition_at(&main, source, "VIEW_TOTAL");
        let place = &answer[0];
        assert!(
            place["targetUri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("show.html.erb")),
            "{answer}"
        );
        let column = place["targetSelectionRange"]["start"]["character"]
            .as_u64()
            .expect("a column");
        let expected = template
            .find("VIEW_TOTAL")
            .map(|at| template[..at].encode_utf16().count() as u64)
            .expect("the constant");
        assert_eq!(
            column, expected,
            "the markup's own column, counted in the units the client counts: {answer}"
        );
    }

    #[test]
    fn a_definition_whose_file_is_gone_answers_nothing_rather_than_a_dead_link() {
        // The graph still holds the declaration and its file. Turning that into a `LocationLink`
        // needs the text to place two ranges, and a file deleted since indexing has none (as
        // `diagnostics_are_skipped_for_a_file_that_has_gone_from_disk` covers from the other end).
        // Every site failing leaves an empty list, which must be answered `null`: a client given
        // `[]` opens an empty peek window instead of saying nothing was found.
        let mut harness = Harness::new();
        let person = harness.write("app/person.rb", "class Person\nend\n");
        let source = "Person.new\n";
        let main = harness.write("app/main.rb", source);
        harness.index();
        assert!(
            !harness.definition_at(&main, source, "Person").is_null(),
            "the jump works while the file is there"
        );

        std::fs::remove_file(person.to_file_path().expect("a path")).unwrap();

        assert!(
            harness.definition_at(&main, source, "Person").is_null(),
            "and answers nothing once it is not"
        );
    }

    /// Every spelling of an instance variable [`scopes`] handles, plus a second `self` holding one
    /// of the same name.
    ///
    /// Prism reduces them to six nodes (a write, `||=`, `&&=`, an operator write, a destructuring
    /// target, a read), and `definition` must find the others from each. The `def self.reset` at
    /// the bottom must *not* be found: its `@total` belongs to the class object.
    const IVARS: &str = "\
class Counter
  def initialize
    @total = 0
    @seen ||= []
    @open &&= true
    @total += 1
    @first, @last = 1, 2
  end

  def report
    label = 'totals'
    [@total, @seen, @open, @first, @last]
  end

  def self.reset
    @total = -1
  end
end
";

    #[test]
    fn definition_on_an_instance_variable_answers_every_write_that_shares_its_self() {
        // The graph models none of this: rubydex files an instance variable's assignment as a
        // declaration and records no references, so `definition` at a *read* would answer nothing,
        // at a cursor `documentHighlight` lights up across the file.
        //
        // One picture per spelling, pinned *together*, for `GALLERY`'s reason. `w`/`r` is what the
        // highlight lit, and an uppercase cell is a definition target landing on it, so the
        // property (every jump lands on a span the highlight lit) is the absence of a lowercase `d`
        // anywhere.
        let mut harness = Harness::new();
        let uri = harness.write("lib/counter.rb", IVARS);
        harness.index();

        // Every cursor is on the *read* in `report`, and each needle is the shortest run of the
        // line naming one position.
        let drawn: String = [
            ("@total", "[@total"),
            ("@seen", "[@total, @seen"),
            ("@open", "@seen, @open"),
            ("@first", "@open, @first"),
            ("@last", "@open, @first, @last"),
        ]
        .into_iter()
        .map(|(name, needle)| {
            let marked = cursor_after(IVARS, needle);
            format!("{name}\n{}\n", harness.agreement_map(&uri, &marked))
        })
        .collect();

        assert_eq!(
            drawn,
            "\
@total
    @total = 0
    WWWWWW
    @total += 1
    WWWWWW
    [@total, @seen, @open, @first, @last]
     rrrrrr
@seen
    @seen ||= []
    WWWWW
    [@total, @seen, @open, @first, @last]
             rrrrr
@open
    @open &&= true
    WWWWW
    [@total, @seen, @open, @first, @last]
                    rrrrr
@first
    @first, @last = 1, 2
    WWWWWW
    [@total, @seen, @open, @first, @last]
                           rrrrrr
@last
    @first, @last = 1, 2
            WWWWW
    [@total, @seen, @open, @first, @last]
                                   rrrrr
"
        );
    }

    #[test]
    fn an_instance_variable_on_the_class_object_does_not_reach_an_instances() {
        // `@total` in `def self.reset` and in `def initialize` are two variables, kept apart only
        // because this answer uses the same `SelfContext` logic as `documentHighlight` and
        // `rename`. The `-1` keeps this picture distinct from the other `@total = 0`.
        let mut harness = Harness::new();
        let uri = harness.write("lib/counter.rb", IVARS);
        harness.index();

        assert_eq!(
            harness.agreement_map(&uri, &cursor_after(IVARS, "def self.reset\n    @total")),
            "    @total = -1\n\
             \u{20}   WWWWWW"
        );
    }

    #[test]
    fn a_local_is_not_this_question_and_both_requests_say_so() {
        // The scope walk handles two kinds of variable, and this answer covers one.
        // `person = Person.new` is visible from where the reader stands, so nothing jumps to or
        // types it, leaving a local as the one cursor where `documentHighlight` answers and
        // `definition` does not. Pinned as a known gap, not a settled decision: the lowercase `w`
        // shows it.
        let mut harness = Harness::new();
        let uri = harness.write("lib/counter.rb", IVARS);
        harness.index();

        assert_eq!(
            harness.agreement_map(&uri, &cursor_after(IVARS, "    label")),
            "    label = 'totals'\n\
             \u{20}   wwwww"
        );
        assert_eq!(
            harness.hover_at(&uri, IVARS, "label = "),
            serde_json::Value::Null
        );
    }

    /// A model whose macros cover the three answers a `:symbol` can have.
    ///
    /// - **Declares the name**: `belongs_to`, `has_many`, `scope`, `delegate` and `enum`; a
    ///   `workspace/rails/` generator already wrote it down. `validates` names a column
    ///   `db/schema.rb` declared.
    /// - **Names an existing `def`**: `validate` and `before_save` name a `def` further down and
    ///   declare nothing.
    /// - **Names nothing**: `validates :absent`, the decline.
    const MACROS: &str = "\
class Story < ApplicationRecord
  belongs_to :user
  has_many :comments, dependent: :destroy
  validates :title, presence: true
  validates :absent, presence: true
  validate :title_is_short
  before_save :normalise
  scope :recent, -> { order(created_at: :desc) }
  delegate :email, to: :user
  enum :status, [:draft, :live]

  def title_is_short
    normalise
  end

  def normalise
    send(:title_is_short)
  end
end
";

    /// A controller with a controller-specific callback, plus the one symbol in this application
    /// that names nothing.
    const CALLBACKS: &str = "\
class StoriesController < ApplicationController
  before_action :authenticate, only: [:index]
  skip_before_action :verify

  def index
  end

  private

  def authenticate
  end
end
";

    /// The application `MACROS` lives in: a schema, the two models it names, and a controller with
    /// a controller-specific callback.
    fn macros_project() -> (Harness, DocUri, DocUri) {
        // Enough of Ruby's object model for the ancestor walk to run off the project's end into it:
        // the case `ruby_s_own` exists for, unreachable without a core signature.
        let mut harness = signed(
            &[
                ("core/core.rbs", TYPED_RBS),
                (
                    "core/kernel.rbs",
                    "module Kernel\n  def format: (String) -> String\n  \
                     def send: (Symbol, *untyped) -> untyped\nend\n",
                ),
            ],
            "",
        );
        harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[8.0].define(version: 1) do\n  \
             create_table \"stories\", force: :cascade do |t|\n    \
             t.string \"title\"\n  \
             end\n\
             end\n",
        );
        let story = harness.write("app/models/story.rb", MACROS);
        harness.write(
            "app/models/user.rb",
            "class User < ApplicationRecord\nend\n",
        );
        harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\nend\n",
        );
        let controller = harness.write("app/controllers/stories_controller.rb", CALLBACKS);
        harness.index();
        harness.index_gems();
        (harness, story, controller)
    }

    #[test]
    fn a_macro_symbol_answers_the_member_the_macro_names() {
        // The two answers a macro's symbol can have, drawn at the cursor. A macro that *declares* a
        // name (`belongs_to`, `scope`, `enum`) answers with the macro line, where
        // `workspace/rails/` recorded the declaration. One that only *names* an existing method
        // answers with the `def`.
        //
        // Every definition target is an uppercase mark: the audit's containment check, drawn.
        // `definition` lands nowhere `documentHighlight` did not light. Both read one resolution,
        // so that is a property, not a coincidence.
        let (mut harness, story, controller) = macros_project();

        // `belongs_to :user` declares `Story#user`, so the macro *is* the declaration's place and
        // the jump is to the cursor's own line. The card carries the answer; the agreement keeps
        // the two requests from drifting.
        assert_eq!(
            harness.agreement_map(&story, &cursor_after(MACROS, "belongs_to :us")),
            "  belongs_to :user\n\
             \u{20}             WWWW"
        );
        // `scope :recent` likewise, on the other side of the object: it declares a singleton
        // method, which is why the lookup asks the class object after the instance.
        assert_eq!(
            harness.agreement_map(&story, &cursor_after(MACROS, "scope :rec")),
            "  scope :recent, -> { order(created_at: :desc) }\n\
             \u{20}        WWWWWW"
        );
        // `before_save :normalise` declares nothing. The `def` is the target, the call in
        // `title_is_short` is lit beside it, and the symbol is lit as the read it is; the graph
        // records none of this.
        assert_eq!(
            harness.agreement_map(&story, &cursor_after(MACROS, "before_save :norm")),
            "  before_save :normalise\n\
             \u{20}              rrrrrrrrr\n\
             \u{20}   normalise\n\
             \u{20}   rrrrrrrrr\n\
             \u{20} def normalise\n\
             \u{20}     WWWWWWWWW"
        );
        // A controller's callback follows the same rule, and the `def` it names is private, which
        // is exactly why a reader clicks it.
        assert_eq!(
            harness.agreement_map(
                &controller,
                &cursor_after(CALLBACKS, "before_action :authentica")
            ),
            "  before_action :authenticate, only: [:index]\n\
             \u{20}                rrrrrrrrrrrr\n\
             \u{20} def authenticate\n\
             \u{20}     WWWWWWWWWWWW"
        );
    }

    #[test]
    fn a_macro_symbol_is_typed_by_whatever_wrote_the_member() {
        // One card per rung: the member the symbol names, typed as a call of it would be. That the
        // macro makes the symbol a name at all is the rung's evidence, which the card, saying what
        // the answer is and not how it was found, does not repeat.
        let (mut harness, story, _controller) = macros_project();

        let card = |harness: &mut Harness, needle: &str| {
            harness.hover_at(&story, MACROS, needle)["contents"]["value"]
                .as_str()
                .unwrap_or("null")
                .to_owned()
        };

        // A generator's own declaration, so the card is `Story.new.user`'s.
        assert_eq!(
            card(&mut harness, "user\n"),
            "```ruby\nStory#user -> User?\n```"
        );

        // The column, the answer worth having: `validates :title` is a claim about `db/schema.rb`.
        let column = card(&mut harness, "title, presence");
        assert!(column.contains("Story#title"), "{column}");
        assert!(!column.contains("Guessed from name alone"), "{column}");

        // A `def`, where the only derived part is that the symbol names one.
        let method = card(&mut harness, "title_is_short\n");
        assert!(method.contains("Story#title_is_short"), "{method}");
        assert!(!method.contains("Guessed from name alone"), "{method}");
    }

    #[test]
    fn a_symbol_no_ancestor_declares_is_not_answered_at_all() {
        // The decline, which is the whole rung below this one. `:absent` is not a column, a `def`
        // or anything a generator wrote, so the only answer left is a name match against every
        // `def absent`, and a jump from a `validates` line into someone else's method is a wrong
        // answer the reader cannot spot. `skip_before_action :verify` is the same decline in a
        // controller.
        let (mut harness, story, controller) = macros_project();

        assert_eq!(
            harness.agreement_map(&story, &cursor_after(MACROS, "validates :abse")),
            "null"
        );
        assert!(
            harness
                .hover_at(&story, MACROS, "absent, presence")
                .is_null(),
            "a symbol nothing declares must not be typed either"
        );
        assert_eq!(
            harness.agreement_map(
                &controller,
                &cursor_after(CALLBACKS, "skip_before_action :ver")
            ),
            "null"
        );
    }

    #[test]
    fn only_a_macros_own_positional_symbol_is_one_of_its_names() {
        // Three symbols that are *not* the macro's subject, each differently: `dependent: :destroy`
        // configures the macro; `:desc` is inside the block `scope` got; `:index` is inside an
        // array inside a keyword argument. All would resolve if asked (`destroy` and `index` are
        // real methods), so the syntax test is the whole safety.
        let (mut harness, story, controller) = macros_project();

        for needle in ["dependent: :destr", "created_at: :des", "[:draf"] {
            assert_eq!(
                harness.agreement_map(&story, &cursor_after(MACROS, needle)),
                "null",
                "at {needle:?}"
            );
        }
        assert_eq!(
            harness.agreement_map(&controller, &cursor_after(CALLBACKS, "only: [:inde")),
            "null"
        );
    }

    #[test]
    fn a_symbol_sent_from_a_method_body_names_the_method_it_calls() {
        // A macro is a call written into a class body, so `send(:title_is_short)` one level down
        // is not a macro's symbol. It is a method's name all the same: Ruby's `send` calls what
        // its first argument names, so the symbol jumps where the call would (`resolve_named`).
        // A name only running Ruby spells (`%i[a b].each { |name| send(name) }`) is no symbol, and
        // answers nothing.
        let (mut harness, story, _controller) = macros_project();

        assert_eq!(
            harness.agreement_map(&story, &cursor_after(MACROS, "send(:title_is_sh")),
            "  def title_is_short\n\
             \u{20}     WWWWWWWWWWWWWW\n\
             \u{20}   send(:title_is_short)\n\
             \u{20}         rrrrrrrrrrrrrr"
        );
    }

    #[test]
    fn a_symbol_that_only_ruby_declares_is_not_this_projects_answer() {
        // Every class inherits `Object` and `Kernel`, so an ancestor walk always ends somewhere,
        // and `Kernel` alone declares `format`, `print`, `select`, `system`, `test`, `open` and
        // `sub`, all common column names. Without the filter, `validates :format` on a model with
        // no `format` column is a **Resolved** jump into `vendor/rbs`: right about the tier and
        // about nothing else.
        //
        // Nothing real is lost: a member the class actually has is found on the class before the
        // walk reaches a root, as `MACROS`' own `validates :title` is.
        let (mut harness, story, _controller) = macros_project();
        assert!(
            harness.has("Kernel#format()"),
            "the fixture has to reach Ruby's own methods for this to be testing anything"
        );

        let source = "class Widget < ApplicationRecord\n  validates :format\nend\n";
        let widget = harness.write("app/models/widget.rb", source);
        harness.index();

        assert!(
            harness.definition_at(&widget, source, "format").is_null(),
            "a macro's symbol never means one of Ruby's own methods"
        );
        // The class's own member still answers, from the same walk one step earlier.
        assert!(
            !harness
                .definition_at(&story, MACROS, "title, presence")
                .is_null()
        );
    }

    #[test]
    fn turning_the_guess_off_takes_nothing_away_from_a_macro_symbol() {
        // `[types] guess_from_names` silences the one answer allowed to be wrong. A symbol's answer
        // is not that: the member was found in the class's own ancestors, and only the convention
        // (a macro's argument names a member) is derived, and the card states it. So the setting
        // has nothing to switch off here.
        let mut harness = signed(
            &[("core/core.rbs", TYPED_RBS)],
            "\n\
                 [types]\nguess_from_names = false\n",
        );
        harness.write(
            "db/schema.rb",
            "ActiveRecord::Schema[8.0].define(version: 1) do\n  \
             create_table \"stories\", force: :cascade do |t|\n    \
             t.string \"title\"\n  \
             end\n\
             end\n",
        );
        let story = harness.write("app/models/story.rb", MACROS);
        harness.write(
            "app/models/user.rb",
            "class User < ApplicationRecord\nend\n",
        );
        harness.write(
            "app/models/comment.rb",
            "class Comment < ApplicationRecord\nend\n",
        );
        harness.index();
        harness.index_gems();

        let card = harness.hover_at(&story, MACROS, "title, presence")["contents"]["value"]
            .as_str()
            .unwrap_or("null")
            .to_owned();
        assert!(card.contains("Story#title"), "{card}");
        assert!(!card.contains("Guessed from name alone"), "{card}");
    }

    /// A class, an association and a caller: the fixture the four cursor kinds are asked of.
    ///
    /// One `def` whose body names its return, so a call has a type without any signature. The body
    /// rung answers it, which is `Derived` and therefore drawable.
    fn stories() -> Harness {
        let harness = Harness::new();
        harness.write(
            "app/models/author.rb",
            "class Author\n  def name\n    \"a\"\n  end\nend\n",
        );
        harness.write(
            "app/models/story.rb",
            "class Story\n  def author\n    Author.new\n  end\n\n  def self.first\n    Story.new\n  end\nend\n",
        );
        harness
    }

    #[test]
    fn a_calls_type_is_the_class_it_hands_back_and_not_the_def_it_reaches() {
        // `typeDefinition`'s question beside `definition`'s at the same byte: the jump goes to
        // `def author`, this to `class Author`. A call: the one shape where the cursor must be
        // inside the **message**, not anywhere in the node.
        let mut harness = stories();
        let source = "story = Story.first\nstory.author\n";
        let caller = harness.write("app/main.rb", source);
        harness.index();

        assert_eq!(
            harness.type_definition_list(&caller, source, "author"),
            ["author.rb:0:6"]
        );
        // The neighbour, asserted beside it: the two requests answer differently at one cursor, and
        // that difference is the feature.
        assert_eq!(
            linked(&harness.definition_at(&caller, source, "author")),
            ["story.rb:1:6"]
        );
    }

    #[test]
    fn a_local_answers_at_the_name_it_is_bound_to_and_at_every_read_of_it() {
        // A binding, both roads. The binding is `cursor::bindings_in` with its range collapsed to a
        // point, the same call the inlay margin makes, so the jump and the label cannot disagree;
        // the read is the walk beside it.
        let mut harness = stories();
        let source = "story = Story.first\nstory.author\n";
        let caller = harness.write("app/main.rb", source);
        harness.index();

        assert_eq!(
            harness.type_definition_list(&caller, source, "story ="),
            ["story.rb:0:6"]
        );
        assert_eq!(
            harness.type_definition_list(&caller, source, "story.author"),
            ["story.rb:0:6"]
        );
    }

    #[test]
    fn an_instance_variable_is_the_scope_walk_here_too() {
        // An instance variable: `resolve_variable`, nothing new. The walk before the graph is the
        // order every request here keeps; asking the syntax walk first would classify one `@story`
        // in two modules.
        let mut harness = stories();
        let source =
            "class Page\n  def show\n    @story = Story.first\n    @story.author\n  end\nend\n";
        let caller = harness.write("app/page.rb", source);
        harness.index();

        assert_eq!(
            harness.type_definition_list(&caller, source, "@story.author"),
            ["story.rb:0:6"]
        );
    }

    #[test]
    fn a_constant_answers_with_the_class_it_names() {
        // A constant: `Story` as a receiver has the type `Story`'s singleton class, whose
        // declaration site is `class Story`. It goes through the same arm as every other kind;
        // coinciding with `definition` at this cursor is redundant, not wrong.
        let mut harness = stories();
        let source = "Story.first\n";
        let caller = harness.write("app/main.rb", source);
        harness.index();

        assert_eq!(
            harness.type_definition_list(&caller, source, "Story"),
            ["story.rb:0:6"]
        );
    }

    #[test]
    fn a_member_an_operator_write_parks_on_the_operator_answers_on_its_name() {
        // rubydex files `value` (and `value=`) on the `||=`, so the name answered nothing
        // (an upstream defect). `+=` files on the `.`, one byte left of the name, and always
        // answered; it is here as the control.
        let source = "\
class Holder
  def value; end
  def value=(given); end

  def fill
    self.value ||= 1
    self.value &&= 2
    self.value += 3
  end
end

holder = Holder.new
holder.value ||= 4
holder.value += 5
";
        let mut harness = Harness::new();
        let uri = harness.write("app/holder.rb", source);
        harness.index();

        for needle in [
            "value ||= 1",
            "value &&= 2",
            "value += 3",
            "value ||= 4",
            "value += 5",
        ] {
            let card = harness.hover_at(&uri, source, needle);
            assert!(
                card.to_string().contains("Holder#value"),
                "{needle}: {card}"
            );
            // The reader, never the writer rubydex files beside it.
            assert!(!card.to_string().contains("value="), "{needle}: {card}");
            assert_eq!(
                linked(&harness.definition_at(&uri, source, needle)),
                ["holder.rb:1:6"],
                "{needle}"
            );
        }
        // A typed receiver is asked about its own member, as `holder.value` would be: the message of
        // an operator write is a call for the parse too (`cursor::at`).
        for needle in ["value ||= 4", "value += 5"] {
            let card = harness.hover_at(&uri, source, needle);
            assert!(!card.to_string().contains("name alone"), "{needle}: {card}");
        }
        // Placed on the name, where the editor underlines, not on the operator.
        let card = harness.hover_at(&uri, source, "value ||= 1");
        assert_eq!(card["range"]["start"]["character"], 9, "{card}");
        assert_eq!(card["range"]["end"]["character"], 14, "{card}");
        // `+=` too, where rubydex's span is the `.` before the name.
        let card = harness.hover_at(&uri, source, "value += 5");
        assert_eq!(card["range"]["start"]["character"], 7, "{card}");
        assert_eq!(card["range"]["end"]["character"], 12, "{card}");
        // The operator itself still answers, as rubydex filed it.
        let operator = harness.hover_at(&uri, source, "||= 1");
        assert!(operator.to_string().contains("Holder#value"), "{operator}");
    }

    #[test]
    fn a_member_a_constant_path_hangs_off_answers_from_the_parse() {
        // rubydex files the constant and never visits its parent, so no offset of the call
        // answered (an upstream defect). The rungs every call gets answer it now.
        let source = "\
class Setting
  TYPES = [].freeze
end

class Holder
  def config
    Setting
  end

  def typed
    self.config::TYPES
  end

  def nested
    self.config.itself::TYPES
  end
end

->(owner) { owner.config::TYPES }
";
        let mut harness = Harness::new();
        let uri = harness.write("app/holder.rb", source);
        harness.index();

        // A typed receiver: the member itself, resolved.
        let typed = harness.hover_at(&uri, source, "config::TYPES\n  end\n\n  def nested");
        assert!(typed.to_string().contains("Holder#config"), "{typed}");
        assert!(!typed.to_string().contains("name alone"), "{typed}");
        assert_eq!(
            linked(&harness.definition_at(&uri, source, "config::TYPES\n  end\n\n  def nested")),
            ["holder.rb:5:6"]
        );
        // Anywhere inside the parent, not only the call next to the `::`.
        let deeper = harness.hover_at(&uri, source, "config.itself");
        assert!(deeper.to_string().contains("Holder#config"), "{deeper}");
        // An untyped receiver gets the name rung, and says so.
        let guessed = harness.hover_at(&uri, source, "config::TYPES }");
        assert!(guessed.to_string().contains("name alone"), "{guessed}");
        assert_eq!(
            linked(&harness.definition_at(&uri, source, "config::TYPES }")),
            ["holder.rb:5:6"]
        );
        // The constant was never missing, and still answers as rubydex filed it.
        let constant = harness.hover_at(&uri, source, "TYPES }");
        assert!(
            !constant.to_string().contains("Holder#config"),
            "{constant}"
        );
    }

    #[test]
    fn a_read_whose_writes_are_in_other_files_jumps_to_them_and_gets_their_card() {
        // The reading file writes nothing, so the in-file walk has nothing; the writes
        // the type side folds for the read are the parent's and a subclass's, whose object the read
        // can run on. A class outside the hierarchy writing the same name is not one of them.
        let mut harness = Harness::new();
        let parent = harness.write(
            "app/controllers/application_controller.rb",
            "class ApplicationController\n  def set_page\n    @page = params[:page].to_i\n    \
             @page = 1 if @page.zero?\n  end\nend\n",
        );
        let child = harness.write(
            "app/controllers/admin_controller.rb",
            "class AdminController < InboxController\n  def boot\n    @page = 3\n  end\nend\n",
        );
        let other = harness.write(
            "app/models/other.rb",
            "class Other\n  def x\n    @page = 9\n  end\nend\n",
        );
        let source = "class InboxController < ApplicationController\n  def show\n    @page * 2\n  end\nend\n";
        let reader = harness.write("app/controllers/inbox_controller.rb", source);
        harness.watch(&[&parent, &child, &other, &reader]);

        assert_eq!(
            linked(&harness.definition_at(&reader, source, "page * 2")),
            [
                "admin_controller.rb:2:4",
                "application_controller.rb:2:4",
                "application_controller.rb:3:4",
            ]
        );
        // The card names the reading class's own ancestry first, not the subclass that sorts first.
        let card = harness.hover_at(&reader, source, "page * 2");
        assert!(
            card.to_string().contains("ApplicationController#@page"),
            "{card}"
        );
        assert_eq!(card["range"]["start"]["line"], 2, "{card}");
    }

    #[test]
    fn a_read_nothing_types_gets_the_card_its_write_gets() {
        // `definition` at the read jumps to both writes; the card used to be silent, because a read
        // is a span rubydex files nothing under and the writes disagree about the type.
        let source = "\
class Inbox
  def show
    @page * 2
  end

  def set_page
    @page = params[:page].to_i
    @page = 1 if @page.zero?
  end
end
";
        let mut harness = Harness::new();
        let uri = harness.write("app/inbox.rb", source);
        harness.index();

        let read = harness.hover_at(&uri, source, "page * 2");
        let write = harness.hover_at(&uri, source, "page = params");
        assert!(!read.is_null(), "{read}");
        assert_eq!(read["contents"], write["contents"], "{read}\n{write}");
        assert!(read.to_string().contains("Inbox#@page"), "{read}");
        // The span the reader pointed at, not the write the card came from.
        assert_eq!(read["range"]["start"]["line"], 2, "{read}");

        // A file that only reads it is answered from the files of its class that write it
        //, and jumps there, as the card says.
        let only = "class Inbox\n  def index\n    @page\n  end\nend\n";
        let reader = harness.write("app/reader.rb", only);
        harness.index();
        let elsewhere = harness.hover_at(&reader, only, "page\n");
        assert_eq!(elsewhere["contents"], write["contents"], "{elsewhere}");
        assert_eq!(
            linked(&harness.definition_at(&reader, only, "page\n")),
            ["inbox.rb:6:4", "inbox.rb:7:4"]
        );
    }

    #[test]
    fn a_block_parameter_answers_with_what_the_signature_says_it_is_handed() {
        // The other half of the binding case: nothing about `|story|` says what it holds; the
        // signature of the method the block was passed to does. A hand-written `sig/` says it here,
        // the one place a block parameter's type can come from.
        let mut harness = stories();
        harness.write(
            "sig/story.rbs",
            "class Story\n  def self.each: () { (Story) -> void } -> void\nend\n",
        );
        let source = "Story.each do |story|\n  story.author\nend\n";
        let caller = harness.write("app/main.rb", source);
        harness.index();

        assert_eq!(
            harness.type_definition_list(&caller, source, "story|"),
            ["story.rb:0:6"]
        );
    }

    #[test]
    fn a_type_matched_on_a_name_alone_is_never_jumped_to() {
        // The refusal is a test on the **tier**, not on the shape. `story` is assigned nothing, the
        // name rung guesses `Story` from its letters, and a jump has no room for the footnote
        // saying so. `hover` at the same cursor still answers and says it is guessing, which makes
        // this a refusal, not a gap.
        let mut harness = stories();
        let source = "story.author\n";
        let caller = harness.write("app/main.rb", source);
        harness.index();

        let refused = harness.type_definition_at(&caller, source, "author");
        assert!(refused.is_null(), "{refused}");
        let card = harness.hover_at(&caller, source, "author");
        assert!(
            card.to_string().contains("name"),
            "the card still answers and still says it is guessing: {card}"
        );

        // The cursor one step left answers `null` by design: an unassigned local reaches the name
        // rung, the tier a jump refuses. Not a defect; the count is what to watch.
        let local = harness.type_definition_at(&caller, source, "story");
        assert!(local.is_null(), "{local}");
    }

    #[test]
    fn the_answer_is_links_for_a_client_that_asked_for_them_and_locations_for_one_that_did_not() {
        // Its own `linkSupport`, not `definition`'s: the protocol gives each goto one, and clients
        // differ. The third field for the third goto.
        let mut harness = stories();
        let source = "story = Story.first\nstory.author\n";
        let caller = harness.write("app/main.rb", source);
        harness.index();

        let linked_answer = harness.type_definition_at(&caller, source, "author");
        assert!(linked_answer[0]["targetUri"].is_string(), "{linked_answer}");
        assert!(
            linked_answer[0]["originSelectionRange"].is_object(),
            "a link carries the span it was asked at: {linked_answer}"
        );

        harness.takes_no_type_definition_links();
        let flat = harness.type_definition_at(&caller, source, "author");
        assert!(flat[0]["uri"].is_string(), "{flat}");
        assert!(flat[0]["targetUri"].is_null(), "{flat}");
    }

    /// [`stories`] with a hand-written signature, which is what a project that answers
    /// `textDocument/declaration` looks like.
    ///
    /// `Story#author` is declared twice (the `def` and the `.rbs`), the ordinary case where the two
    /// jumps split. `Story.count` is declared **only** by the signature, where they coincide.
    fn stories_with_a_signature() -> Harness {
        let harness = stories();
        harness.write(
            "sig/story.rbs",
            "class Story\n  def author: () -> Author\n  def self.count: () -> Integer\nend\n",
        );
        harness
    }

    #[test]
    fn a_declaration_is_the_signature_where_the_definition_is_the_source() {
        // The whole method at one cursor: `definition` opens the `def` someone wrote, and this
        // opens the `.rbs` saying what it takes and returns. Two requests, two files, one byte.
        let mut harness = stories_with_a_signature();
        let source = "story = Story.first\nstory.author\n";
        let caller = harness.write("app/main.rb", source);
        harness.index();

        assert_eq!(
            harness.declaration_list(&caller, source, "author"),
            ["story.rbs:1:6"]
        );
        assert_eq!(
            linked(&harness.definition_at(&caller, source, "author")),
            ["story.rb:1:6"]
        );
    }

    #[test]
    fn a_def_is_asked_at_its_own_name_too_and_that_is_where_the_audit_asks_it() {
        // Standing on `def author` in the source, where `definition` would answer the line already
        // under the cursor. Here the answer is the signature over it, the one thing at that cursor
        // the reader cannot already see.
        let mut harness = stories_with_a_signature();
        let source = "class Story\n  def author\n    Author.new\n  end\n\n  def self.first\n    Story.new\n  end\nend\n";
        let story = harness.write("app/models/story.rb", source);
        harness.index();

        assert_eq!(
            harness.declaration_list(&story, source, "author"),
            ["story.rbs:1:6"]
        );
    }

    #[test]
    fn a_signature_with_no_source_beside_it_is_what_both_jumps_answer() {
        // Where the two coincide, which is [`places`]' rule seen from the other side: a signature
        // is dropped where source survives and **kept where none does**, so a method only the
        // `.rbs` declares is already `definition`'s answer. Refusing here would reject the clearest
        // declaration in the index for agreeing with `definition`.
        let mut harness = stories_with_a_signature();
        let source = "Story.count\n";
        let caller = harness.write("app/main.rb", source);
        harness.index();

        assert_eq!(
            harness.declaration_list(&caller, source, "count"),
            ["story.rbs:2:11"]
        );
        assert_eq!(
            linked(&harness.definition_at(&caller, source, "count")),
            ["story.rbs:2:11"]
        );
    }

    #[test]
    fn nothing_a_signature_does_not_declare_is_answered_with_the_source_instead() {
        // The refusal that makes this more than a second `definition`. `Author#name` is Ruby with
        // no other declaration, so the narrowed list is empty and the answer `null`, which hands
        // the client its fallback: `definition`, the right answer and not this one's to give.
        let mut harness = stories_with_a_signature();
        let source = "author = Story.first.author\nauthor.name\n";
        let caller = harness.write("app/main.rb", source);
        harness.index();

        let refused = harness.declaration_at(&caller, source, "name");
        assert!(refused.is_null(), "{refused}");
        assert_eq!(
            linked(&harness.definition_at(&caller, source, "name")),
            ["author.rb:1:6"]
        );
    }

    #[test]
    fn a_generated_declaration_is_a_place_and_never_a_declaration() {
        // `belongs_to :user` writes a graph declaration and a *place* on the macro line, an `.rb`,
        // so `definition` answers it and this answers nothing: no written signature says what
        // `user` is. The same holds for a Sorbet `sig` or a YARD `@return`: they give a `def` a
        // type, not a second declaration site.
        let (mut harness, _, _) = macros_project();
        let source = "Story.new.user\n";
        let caller = harness.write("app/main.rb", source);
        harness.index();

        let refused = harness.declaration_at(&caller, source, "user\n");
        assert!(refused.is_null(), "{refused}");
        // The macro's own symbol, where the generator recorded what it declared.
        let defined = linked(&harness.definition_at(&caller, source, "user\n"));
        assert_eq!(defined, ["story.rb:1:14"]);
    }

    #[test]
    fn a_receiver_matched_on_a_name_alone_is_never_handed_a_signature() {
        // The tier gate, on its fourth surface and at its worst case: a vendored `.rbs` is the most
        // official-looking document in the index, so pointing a name match at one would pass a
        // guess off as the standard library's word. `definition` at the same cursor still answers
        // (the name rung is honest when *labelled*), which makes this a refusal, not a gap.
        let mut harness = stories_with_a_signature();
        let source = "story.author\n";
        let caller = harness.write("app/main.rb", source);
        harness.index();

        let refused = harness.declaration_at(&caller, source, "author");
        assert!(refused.is_null(), "{refused}");
        assert_eq!(
            linked(&harness.definition_at(&caller, source, "author")),
            ["story.rb:1:6"]
        );
    }

    #[test]
    fn the_declaration_answers_in_whichever_shape_this_client_negotiated_for_it() {
        // The fourth flag for the fourth goto: the protocol has one `linkSupport` per goto, and no
        // client sends all four alike.
        let mut harness = stories_with_a_signature();
        let source = "story = Story.first\nstory.author\n";
        let caller = harness.write("app/main.rb", source);
        harness.index();

        let linked_answer = harness.declaration_at(&caller, source, "author");
        assert!(linked_answer[0]["targetUri"].is_string(), "{linked_answer}");
        assert!(
            linked_answer[0]["originSelectionRange"].is_object(),
            "a link carries the span it was asked at: {linked_answer}"
        );

        harness.takes_no_declaration_links();
        let flat = harness.declaration_at(&caller, source, "author");
        assert!(flat[0]["uri"].is_string(), "{flat}");
        assert!(flat[0]["targetUri"].is_null(), "{flat}");
    }

    #[test]
    fn a_superclass_spelled_like_its_class_names_the_class_ruby_names() {
        // An upstream defect: rubydex resolves the superclass to the class being opened.
        // Ruby cannot, since the constant does not exist yet, so it names the top-level class.
        let mut harness = Harness::new();
        let base = "\
class ApplicationController
  def authenticate
  end

  def self.configure
  end
end
";
        harness.write("app/controllers/application_controller.rb", base);
        let admin_source = "\
module Admin
  class ApplicationController < ApplicationController
  end

  class UsersController < ApplicationController
    configure

    def index
      authenticate
    end
  end
end
";
        let admin = harness.write("app/controllers/admin/base.rb", admin_source);
        let lonely_source = "module Lonely\n  class Thing < Thing\n  end\nend\n";
        let lonely = harness.write("app/models/lonely.rb", lonely_source);
        harness.index();
        let card = |harness: &mut Harness, uri: &DocUri, source: &str, needle: &str| {
            harness.hover_at(uri, source, needle)["contents"]["value"]
                .as_str()
                .map(str::to_owned)
        };

        // The superclass is the top-level class, on the card and on the jump.
        let superclass = "ApplicationController\n  end";
        assert_eq!(
            card(&mut harness, &admin, admin_source, superclass).as_deref(),
            Some("```ruby\nclass ApplicationController\n```")
        );
        assert_eq!(
            linked(&harness.definition_at(&admin, admin_source, superclass)),
            ["application_controller.rb:0:6"]
        );
        // The same spelling anywhere else in `module Admin` is `Admin::ApplicationController`,
        // which exists by then.
        let sibling = "ApplicationController\n    configure";
        assert_eq!(
            card(&mut harness, &admin, admin_source, sibling).as_deref(),
            Some("```ruby\nclass Admin::ApplicationController\n```")
        );
        assert_eq!(
            linked(&harness.definition_at(&admin, admin_source, sibling)),
            ["base.rb:1:8"]
        );

        // Both sides of the chain reach the top-level class's members.
        assert_eq!(
            linked(&harness.definition_at(&admin, admin_source, "authenticate\n    end")),
            ["application_controller.rb:1:6"]
        );
        assert_eq!(
            linked(&harness.definition_at(&admin, admin_source, "configure\n")),
            ["application_controller.rb:4:11"]
        );

        // Where Ruby would find no class, the superclass names nothing, not the class itself.
        assert!(
            harness
                .hover_at(&lonely, lonely_source, "Thing\n  end")
                .is_null()
        );
        assert!(linked(&harness.definition_at(&lonely, lonely_source, "Thing\n  end")).is_empty());
    }
}
