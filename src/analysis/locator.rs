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
//!
//! Width is the second question, not the first. A span that **begins** at the cursor beats one
//! that merely covers it, however wide either is, and `locate` says why at the line that does
//! it.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use rubydex::{
    model::{
        declaration::{Ancestor, Declaration, Namespace},
        definitions::{Definition, Mixin, Receiver},
        graph::Graph,
        ids::{DeclarationId, DefinitionId, NameId, StringId, UriId},
        name::ParentScope,
        references::{ConstantReference, MethodRef},
        visibility::Visibility,
    },
    offset::Offset,
    query::{self, FindMemberError, MatchMode},
};

use ruby_prism::{
    ArgumentsNode, BlockNode, CallNode, ClassNode, DefNode, ModuleNode, SingletonClassNode, Visit,
};

use crate::workspace::DocUri;

use super::{
    cursor::{self, Context},
    environment,
    position::Rebase,
    render, scopes,
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

/// An instance variable at the cursor, and every assignment that shares its `self`.
///
/// The one ordinary thing to put a cursor on that the graph does not model. rubydex records an
/// instance variable's *assignment* as a declaration and records no reference to one, so
/// [`locate`] finds nothing at a read and finds the declaration itself at a write — and neither
/// is the answer to what `@title` means, which is every place in this `self` it is written.
///
/// **In the buffer's coordinates, not the graph's**, because [`scopes`](super::scopes) read the
/// buffer. Nothing here is rebased and nothing here may be: the answer is a fact about the text
/// the client is holding this keystroke, which is the same reason `documentHighlight` answers
/// from the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    /// The span the cursor landed in, which is what an editor underlines.
    pub start: u32,
    pub end: u32,
    /// Every write, in source order. Empty where the file only ever reads it — an instance
    /// variable a superclass assigns, or a template's, which its controller assigns.
    pub writes: Vec<(u32, u32)>,
}

/// A macro's symbol argument at the cursor, and the span an editor underlines.
///
/// The other ordinary thing to point at that the graph does not hold, and it is a *different*
/// nothing from [`Variable`]'s: rubydex models an instance variable's assignment and gets the
/// question wrong, while it models a symbol literal not at all. So this one is asked **after**
/// [`locate`] rather than before it — there is no span both halves answer for, and where the
/// graph does speak at a symbol it is because `attr_reader :count` filed a definition whose name
/// span is the symbol, which is the same declaration this would have found anyway.
///
/// No declarations of its own: unlike a variable's writes, what a symbol names is a declaration
/// in the graph, so it travels in the [`Resolution`] beside this and is rebased and turned into
/// a place by the machinery every other target already uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// The name, colon excluded: `authenticate`.
    pub name: String,
    /// The span of the name, colon excluded, **in the buffer's coordinates** — it was read out
    /// of the text the client is holding.
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
    /// The class the receiver typed to, when the answer is still the name-based list because
    /// the member is not on it.
    ///
    /// **Set only where [`Self::precise`] is `false` and the receiver nonetheless had a type.**
    /// The list is the honest answer and the tier is still a guess — see the `Err` arm of
    /// [`typed`] for why a wrong *no such method* would be worse — but *why* it is a guess is a
    /// different fact here, and a card that says the receiver's type is unknown when the server
    /// had one contradicts the `completion` list at the same cursor, which is offered from
    /// exactly this class.
    pub missed: Option<Missed>,
}

/// A receiver that typed, and a member that is on no ancestor of it.
///
/// **A name a reader can read, which is not the same as one they can open.** `Foo::<Foo>` is
/// rubydex's spelling for a type Ruby cannot write down and a singleton is kept rather than
/// refused, because the *class it hangs off* is spellable and is what a reader would look at —
/// the flag is what keeps the sentence from calling a class object an instance of itself. An
/// anonymous `Class.new` is kept for the same reason one step further out: `render` already
/// spells it the way the whole crate spells it, and *the receiver is a `Class.new`* is a true
/// sentence where *the receiver's type is unknown* was not. `Namespace::Todo` is the one still
/// refused, and by kind rather than by spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missed {
    /// The class as a reader would write it: `User`, or the `Foo` a class object hangs off.
    pub class: String,
    /// `true` when the receiver is that class's **object** rather than an instance of it.
    pub class_object: bool,
    /// The spelling the class was guessed from, when the type was itself the name rung's guess.
    ///
    /// Carried rather than dropped because the two sentences make different claims — *the
    /// receiver is a `User`* and *the receiver was guessed to be a `User`* — and over six
    /// corpora the second is the one nearly every instance-side position needs: with
    /// `[types] guess_from_names = false` the class disappears from all but 96 of them.
    pub guessed_from: Option<String>,
    /// `true` when the class **does** declare this member and Ruby would refuse the call anyway,
    /// because the receiver is written and is not `self`.
    ///
    /// **A field rather than a sentence, because the sentence would otherwise be false.** The
    /// footnote this feeds reads *which has no such method*, and that is the one thing the
    /// privacy gate must not make the card say: it refuses a member the class has. See
    /// [`Privacy`].
    pub private: bool,
}

impl Resolution {
    fn precise(declarations: Vec<DeclarationId>) -> Self {
        Self {
            declarations,
            precise: true,
            redirected: false,
            derivation: Derivation::default(),
            missed: None,
        }
    }

    /// One declaration ya-lsp typed itself, carrying what it followed to get there.
    ///
    /// `precise` because the tier is the [`Derivation`]'s to carry — the same division
    /// `resolve_typed` makes for a receiver it derived rather than read.
    fn derived(declaration: DeclarationId, derivation: Derivation) -> Self {
        Self {
            declarations: vec![declaration],
            precise: true,
            redirected: false,
            derivation,
            missed: None,
        }
    }

    fn redirected(declaration: DeclarationId) -> Self {
        Self {
            declarations: vec![declaration],
            precise: true,
            redirected: true,
            derivation: Derivation::default(),
            missed: None,
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

    // **A span that begins at the cursor outranks every span that does not**, and that tier is
    // applied before width. `covers` is end-inclusive, so a span that merely *ends* at the
    // cursor is a candidate too — deliberately, and this is what keeps that generosity from
    // costing an answer.
    //
    // `!display_social_login?` records a call to `!` over the bang and a call to the method over
    // the name, adjacent rather than nested. With the cursor on the `d`, width alone picks the
    // bang — one byte against twenty-one — and the user who pointed at a method gets
    // `BasicObject#!`. The method's span starts where the cursor is; the bang's does not.
    //
    // It has to be *starts at* rather than *contains*, because containment is the ordinary
    // nesting that width already sorts. `Rails.env.development?` records a method reference over
    // the whole expression as well as one over each message, so at the `.` after `Rails` the
    // wide call contains the cursor and the `Rails` constant reference only ends there — and the
    // constant is the answer, because it is what types the receiver. Nothing begins at that
    // byte, so no tier opens and width decides, which is the behaviour that was already right.
    //
    // It is also what keeps `a.b += c` answering: rubydex records that call over the `.`, one
    // byte in front of the message, so with the cursor on `b` nothing begins there either.
    if found.iter().any(|located| located.begins_at(offset)) {
        found.retain(|located| located.begins_at(offset));
    }

    let Some(narrowest) = found.iter().map(Located::width).min() else {
        return Vec::new();
    };
    found.retain(|located| located.width() == narrowest);
    found
}

/// The instance variable at `offset`, or `None` when the cursor is on anything else.
///
/// **Asked before [`locate`], and it has to be**: `highlight::find` already asks the scope walk
/// first, for the half of the reason that is visible there — the walk is what can say *no*, and
/// its `None` is what lets a constant or a call fall through to the graph. The other half is
/// this one. `@name = 1` is the single span both halves can answer for, and the graph's answer
/// is the one place the variable is written and none of the places it is read. Two requests
/// disagreeing about the same span is the bug; one rule, asked in one order, is the fix.
#[must_use]
pub fn variable_at(source: &str, offset: u32) -> Option<Variable> {
    let (name, occurrences) = scopes::variable(source, offset)?;
    // A local is the other thing the walk speaks for, and it is not this question: `person =
    // Person.new` is a line the reader can see from where they are standing, and the graph does
    // not pretend otherwise. A class variable never arrives at all — `scopes` models none.
    if !name.starts_with('@') {
        return None;
    }
    // The one the cursor is in, by the test `scopes::variable` found it with — so the miss is
    // unreachable from here, and is written rather than asserted for `types::Scope::at`'s
    // reason: answering "not on a variable" is a better way to be wrong than a panic.
    let at = occurrences
        .iter()
        .find(|occurrence| occurrence.start <= offset && offset <= occurrence.end)?;
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

/// The variable at the cursor and what it is, which is the card's half of the same answer.
///
/// [`resolve_typed`] is this for a call; the two are separate functions because a variable is
/// not a call and has no receiver, and the same for the same reason: hover and go-to-definition
/// read one answer, so a jump and a card cannot disagree about what the cursor is on.
///
/// `scope_at` is the same offset as `offset`, **in the graph's coordinates** — the one part of
/// this that reads the graph is the nesting a guessed constant is resolved in, and the graph is
/// keyed by its own text. Everything else is the buffer's.
///
/// `rebase` is what carries the receiver across that boundary, and it is not optional. The
/// assignment that types `@story` is read out of the buffer, and what it yields is a
/// [`Receiver`](cursor::Receiver) whose offsets are graph *keys* — so without the translation an
/// unindexed keystroke anywhere above the cursor makes `Story` a constant the graph files one
/// byte over, which is a name-guessed card in the ordinary case and the wrong class in the
/// unlucky one. `None` where the receiver is written in text the graph has never been given: see
/// [`resolve_typed`].
///
/// The [`Resolution`] is always `precise`, and the [`Derivation`] on it is what says otherwise:
/// an assignment names the line it was taken from and the name rung names itself, exactly as
/// they do for `@story.title` one request over.
#[must_use]
pub fn resolve_variable(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    source: &str,
    offset: u32,
    scope_at: u32,
    rebase: &Rebase,
) -> Option<(Variable, Resolution)> {
    let variable = variable_at(source, offset)?;
    let receiver =
        cursor::instance_variable(source, (variable.start, variable.end)).rebased(rebase)?;
    let scope = types::Scope::at(sources.graph, uri_id, scope_at);
    let typed = types::method_receiver(sources, uri_id, &receiver, &scope)?;
    Some((
        variable,
        Resolution::derived(typed.declaration, typed.derivation),
    ))
}

/// What a macro's `:symbol` argument names, as a member of the class the macro is written in.
///
/// # One rule, and it covers every macro in the corpus
///
/// A macro's symbol argument is a **member of the class the macro is written in** — its own, an
/// ancestor's, or one a generator wrote for it — and that is true of all twenty-three macros the
/// audit draws from and of every one it does not. `before_action :authenticate` is a `def` in
/// this controller or above it; `validates :title` is the column `db/schema.rb` declared;
/// `belongs_to :user` is the reader `workspace/rails/models.rs` wrote. So there is no macro
/// table here and none is wanted: the answer is an ancestor walk, the same one a call on a typed
/// receiver takes, and a DSL nobody has taught this crate about resolves through it unchanged.
///
/// **The instance side first, then the class object.** `scope :recent` declares a singleton
/// method and every other macro in the list names an instance one, so the order is the frequency
/// — and the two sides are asked rather than chosen between because deciding which one a macro
/// means is exactly the table this avoids.
///
/// # Derived, never resolved
///
/// The member is found exactly, and the claim that the symbol *is* a member is a convention: the
/// code says `:authenticate`, and only `before_action` says that is something to call. So the
/// answer carries [`Derivation::named_by`] and the card prints the macro. A symbol whose name no
/// ancestor declares answers `None` rather than a name-matched guess — `:destroy`, `:draft` and
/// `:desc` are values, not calls, and a jump from one into somebody's `def destroy` is the kind
/// of wrong answer a reader cannot see is wrong.
///
/// `scope_at` is `offset` **in the graph's coordinates**, for [`resolve_variable`]'s reason: the
/// nesting is read from the graph and the symbol is read from the buffer.
#[must_use]
pub fn resolve_symbol(
    graph: &Graph,
    uri_id: UriId,
    source: &str,
    offset: u32,
    scope_at: u32,
) -> Option<(Symbol, Resolution)> {
    let symbol = cursor::macro_symbol(source, offset)?;
    let scope = types::Scope::at(graph, uri_id, scope_at);
    // rubydex keys a method member by its name with `()` on the end, which is what
    // `member_name` hands every other caller. A symbol arrives as the bare word.
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
///
/// # The one fence on the name rung
///
/// Whatever the rungs above it answered, an imprecise answer — the name-based list and nothing
/// else — is filtered through [`loadable_from`] on the way out. That is the last step rather
/// than a condition inside any rung because it is a fact about the *target* and not about how
/// the target was found, and because a filter applied in one of two places is a filter with a
/// hole in it.
/// # `None` is a refusal and not an absence
///
/// The receiver is parsed out of the buffer and looked up in the graph, and between a keystroke
/// and the settle that indexes it those are two different texts. `rebase` translates; where it
/// cannot — the receiver *is* what is being typed — this answers `None`, the request declines,
/// and `Analysis::serve` settles and asks again. That is the same move `completion::complete`
/// makes for the same reason, and the reason it is a refusal rather than a fall-through to the
/// name list: a degraded answer is indistinguishable from a settled one at the call site, so it
/// would never be retried and the deferred path would quietly answer less than the eager one.
#[must_use]
pub fn resolve_typed(
    sources: &types::Sources<'_>,
    uri_id: UriId,
    source: &str,
    located: &Located<'_>,
    at_in_source: u32,
    rebase: &Rebase,
) -> Option<Resolution> {
    let resolution = typed(sources, uri_id, source, located, at_in_source, rebase)?;
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
    source: &str,
    located: &Located<'_>,
    at_in_source: u32,
    rebase: &Rebase,
) -> Option<Resolution> {
    let graph = sources.graph;
    let Target::Call(reference) = located.target else {
        return Some(resolve(graph, located));
    };
    // **The gate travels into `resolve_call` because one precise answer is worth fencing and it
    // is decided in there.** See its root arm: a member found on `Object` is a member found on
    // every receiver, so a `def` the suite alone declares answers for the whole workspace.
    let fence = environment::Fence::at(uri_of(graph, uri_id), sources.layout);
    let resolution = resolve_call(graph, reference, fence, Privacy::Allowed);
    // Built here rather than passed in, because this is the only rung that refuses and the memo
    // inside it is worth exactly one request. Empty until something asks it, so the resolved
    // jump that is not private pays nothing for its existence.
    let modifiers = Modifiers::new(sources.read);
    // Only where rubydex could not name the receiver itself. A derived type is a *worse* answer
    // than a resolved one and must never displace it.
    //
    // **One precise answer is not final, and it is the only one: a private declaration.**
    // Whether Ruby would let it be written here is a question about the syntax — see
    // [`Privacy`] — and the syntax is read below, out of the buffer this rung is the only
    // caller to have. Asking it eagerly would put a parse of the file on every resolved jump
    // and hover; asking it here costs one visibility lookup on the answer already in hand, and
    // the parse only where that lookup says the answer is private, which is rare.
    let vetoable = resolution.precise && holds_private(graph, &modifiers, &resolution);
    if resolution.precise && !vetoable {
        return Some(resolution);
    }
    let Some(member) = member_name(graph, *reference.str()) else {
        return Some(resolution);
    };
    // The same classification completion runs, asked about a call the user has finished writing
    // rather than one they are in the middle of. `cursor::at` needs no special case for that:
    // its test is that the cursor sits between the operator and the end of the message, which a
    // cursor resting *on* a method name does.
    //
    // **Where it cannot be read, the answer above stands.** Both early returns below hand back
    // the resolution as it was, private or not: the gate refuses on a fact it has established
    // and never on one it failed to establish, which is the same direction every other rung
    // here falls in.
    // **`source` is the buffer and `located` is the graph's, and those are two different
    // texts** whenever a keystroke has not been indexed yet. `Scope::at` below is asked in the
    // graph's coordinates because it reads the graph; `cursor::at` is asked in the buffer's
    // because it parses the buffer. Passing `located.start` to both reads the token to the left
    // of the one the user is on.
    let Some(cursor) = cursor::at(source, at_in_source) else {
        return Some(resolution);
    };
    // And the context the buffer just described has to be moved into the graph's coordinates
    // before anything is looked up with it, which is the other half of the same sentence.
    // `cursor::at` read `Story` at a buffer offset; `types::method_receiver` hands that offset
    // to `locate`, which indexes the graph's text. The refusal is the point — see this
    // function's docs.
    let context = cursor.context.rebased(rebase)?;
    // What the syntax permits, read once and applied at all three rungs below.
    let privacy = Privacy::at(&context, &modifiers);
    // The veto on the *resolved* rung, and it is a re-resolve rather than a filter on what came
    // back. What the gate changes is **which rung answers**: refusing the ancestor hit sends the
    // call on to the extend repair and then to the name rung, both of which may hold a public
    // answer the private one was standing in front of. Striking the declaration out of the
    // finished `Resolution` instead would leave a precise answer with nothing in it, which reads
    // to every caller as a resolved *has no such method* rather than as the fall-through it is.
    if vetoable {
        return Some(match privacy {
            Privacy::Allowed => resolution,
            Privacy::Refused(modifiers) => {
                resolve_call(graph, reference, fence, Privacy::Refused(modifiers))
            }
        });
    }
    // The name rung was drawn before the syntax had been read — it is what `resolve_call`
    // returns when nothing above it answered — so the refusal is applied to it here instead.
    // This is the same step `by_name` takes last, over the list `by_name` produced, and the two
    // agree because both are outermost.
    let resolution = Resolution {
        declarations: privacy.keep(graph, resolution.declarations),
        ..resolution
    };
    Some(match &context {
        Context::MethodCall { receiver } => {
            let scope = types::Scope::at(graph, uri_id, located.start);
            let Some(typed) = types::method_receiver(sources, uri_id, receiver, &scope) else {
                return Some(resolution);
            };
            match query::find_member_in_ancestors(
                graph,
                typed.declaration,
                StringId::from(&member),
                false,
            ) {
                // **Gated exactly as the resolved rung above is**, and this is the rung
                // upstream's own looseness lands on: `Vault.new.secret` types the
                // receiver from the constructor and then asks for a member the interpreter
                // refuses. A *Derived* card is still a claim that the code can make this call.
                Ok(found) if privacy.admits(graph, found) => Resolution {
                    declarations: vec![found],
                    precise: true,
                    redirected: false,
                    derivation: typed.derivation,
                    missed: None,
                },
                // The receiver was typed and the method is not on it. The name-based list is
                // still the honest answer: a signature can be incomplete, and this is exactly
                // the case where a wrong "no such method" would be worse than a guess.
                //
                // **What the class was is kept, and until it was the card said the opposite of
                // what the server knew.** `completion` at this same cursor offers this class's
                // members, because it never reaches a member lookup at all — so a footnote
                // reading *the receiver's type is unknown* was a self-contradiction a user
                // could see by pressing one more key. Measured over six corpora, as cards
                // that said the type was unknown while `completion` at the same cursor
                // answered from a class: **207 of 1,506** at an instance variable and **180 of
                // 209** at a class object. The tier does not move — the answer is still a name
                // match.
                //
                // A refused hit falls through with the receiver's class kept, exactly as a
                // missing one does: the card says the type is known and the answer is still a
                // name match, which is true either way. The one thing the footnote may **not**
                // say is *which has no such method*, because the class has it — so which of the
                // two happened is carried, rather than collapsed into a sentence.
                Ok(_) => Resolution {
                    missed: missed(graph, &typed, true),
                    ..resolution
                },
                Err(_) => Resolution {
                    missed: missed(graph, &typed, false),
                    ..resolution
                },
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
            .map(|found| Resolution {
                declarations: vec![found.declaration],
                precise: true,
                redirected: false,
                derivation: Derivation {
                    view: Some(found.how),
                    ..Derivation::default()
                },
                missed: None,
            })
            // The two cannot both answer — a template has no class body to write a block in —
            // so the order between them states which is the better answer rather than settling
            // a contest. A view context is a fact about how Rails loads the file; a closure's
            // `self` is an inference about what somebody's DSL does with a block.
            .or_else(|| in_a_closure(graph, reference, cursor.in_a_closure, &member))
            .unwrap_or(resolution),
        Context::NamespaceAccess { .. } => resolution,
    })
}

/// What the receiver turned out to be, for a card that has to say why it is still guessing.
///
/// `None` for a type there is nothing true to say about: the [`Namespace::Todo`] rubydex
/// invents for a constant no file defines, which has no members by construction, so *has no such
/// method* would be vacuously true of it and of every other name.
///
/// **An anonymous class is not one of those, and saying it was cost this card a false sentence.**
/// rubydex keys a `Class.new` nothing binds to a constant by document and offset;
/// [`render::spelled`] replaces that key with the call that built it, which is what the
/// candidate list two lines above this footnote is already rendered with. Read raw instead, the
/// name failed [`render::is_nameable`] and the card fell back to *the receiver's type is
/// unknown* — a different claim from the truth, which is that the type is **known and
/// unnameable**. It is also the claim `completion` contradicts by listing that very class's
/// members, which is what the audit's check 6 reads.
///
/// This is no longer the test [`hints`](super::hints) applies, and the two have different
/// questions: a margin is drawn unasked and `Class.new` in one is noise, where a footnote is
/// read by somebody who has already asked why the answer is a guess.
fn missed(graph: &Graph, typed: &types::Typed, private: bool) -> Option<Missed> {
    missed_on(
        graph,
        typed.declaration,
        typed.derivation.guess.clone(),
        private,
    )
}

/// The same footnote for a receiver **rubydex** named, where there is no `Typed` to read it off.
///
/// The resolved rung has a `DeclarationId` and nothing else: it never derived the type, because
/// the graph handed it over. Without this the privacy gate's fall-through arrived at the name
/// rung with no `Missed` at all and the card said *the receiver's type is unknown* — of a
/// receiver the server had just resolved. `audit` check 6 is what caught it, at 31 cursors over
/// six corpora, which is the check written for exactly that sentence.
fn missed_on(
    graph: &Graph,
    id: DeclarationId,
    guessed_from: Option<String>,
    private: bool,
) -> Option<Missed> {
    let declaration = graph.declarations().get(&id)?;
    // A `Todo` is the namespace rubydex invents for a constant the workspace references and no
    // file defines. It has no members by construction, so *every* lookup on one misses, and
    // naming it would put a class on the card that nothing declares.
    if matches!(declaration, Declaration::Namespace(Namespace::Todo(_))) {
        return None;
    }
    // **Spelled before it is read apart**, and the order is load-bearing. rubydex spells the
    // singleton of an anonymous class `<key><anonymous>::<<key><anonymous>>`, which
    // `class_object_of` cannot recognise — its `prefix.ends_with(singleton)` test fails on the
    // raw key — while the spelled `Class.new::<Class.new>` it recognises exactly. So a bare
    // `self.` written in one of these bodies says *the class object `Class.new`* rather than
    // nothing, and that cursor is the one this change exists for.
    let name = render::qualified_name(graph, declaration.name());
    let (class, class_object) = match render::class_object_of(&name) {
        Some(class) => (class, true),
        None => (name.as_str(), false),
    };
    // Still the gate, and still the one the symbol picker uses. What it refuses now is only a
    // name nothing could spell: `spelled` deliberately leaves alone an `<anonymous>` suffix with
    // no key in front of it, because a display name is the one thing that must not invent.
    render::is_nameable(class).then(|| Missed {
        class: class.to_owned(),
        class_object,
        guessed_from,
        private,
    })
}

/// The instance side of a class, for a bare name written inside a **block** in its body.
///
/// # Two live scopes, and only one of them was ever searched
///
/// rubydex has no notion of a block: its nesting stack holds lexical scopes, `Class.new` owners
/// and methods, so a bare call inside `[1].each { … }` in a class body is recorded with exactly
/// the receiver a bare call written as a statement of that body gets — the **singleton** class.
/// That is Ruby's own answer and it is right for a block nobody re-binds. It is wrong for every
/// block a DSL takes: `rule(:colon) { str(':') }` runs against a parser *instance*,
/// `scope :recent, -> { where(...) }` against a relation, `validates :x, if: -> { active? }`
/// against a record. The class object has no such method, the search above finds nothing, and
/// the answer falls to the name rung.
///
/// **The name rung then picks wrongly rather than merely vaguely**, which is what makes this a
/// defect and not a gap: [`reachable_on_a_class_object`] drops every candidate a `class` owns,
/// because a class object provably cannot reach one — a correct rule about the *class object*,
/// applied to a cursor whose `self` is not the class object. A method inherited from a
/// superclass is thrown away and a same-named module method kept in its place.
///
/// # What this asks
///
/// Two halves, and both have to answer. The syntactic one — is there a block between this
/// cursor and the body it is written in — arrives on the [`Cursor`](cursor::Cursor) that was
/// read to classify the call, so it is a field rather than a question: it used to be a second
/// parse of the file, ordered last so that most cursors never paid for it, and it is now a
/// second walk over the tree the first parse already produced. The graph's half is here: does
/// the attached class have this member on its instance side. Together they are the whole of the
/// evidence — the name is **absent** from the class object and **present** on an instance, and
/// a file that means the class object here would not run.
///
/// # A class, never a module
///
/// The card is about to say *the block is run against an instance of this*, and a module has no
/// instances. The blocks a module body holds are `included do` and `class_methods do`, whose
/// `self` is the **including class** — not the module and not anything reachable from it. The
/// name rung keeps the module's own instance method anyway, so declining here costs the reader
/// nothing and keeps the tier's claim true.
///
/// # Derived, never resolved
///
/// Nothing in the file says the block is re-bound. `Tier::Derived` is exactly what this is —
/// a convention followed, correct if the convention is — and the card names the class the
/// member was found on so a reader who thinks the DSL does something else can go and look. It
/// fires only where the answer was already a name-matched list, so the rung it replaces is the
/// one rung allowed to be wrong.
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
    // A receiver with an attached class is the whole of "rubydex called this a class object":
    // the same test [`resolve_call`] makes before narrowing the name list, asked of the same
    // declaration. Anything else — a call on an instance, a call with no receiver rubydex could
    // name — has no second scope to search.
    let attached = attached_class(graph, owner)?;
    // One lookup for both of the questions left: whether it is a class — see above, a module has
    // no instances for the card's sentence to be about — and what to call it on that card.
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

/// The uri a document id names, for the two places that ask [`environment::fenced_from`] about
/// the **cursor** rather than about a target.
fn uri_of(graph: &Graph, uri_id: UriId) -> Option<&str> {
    graph
        .documents()
        .get(&uri_id)
        .map(rubydex::model::document::Document::uri)
}

/// The name-matched list, with the places only a test run loads taken out of it.
///
/// The rung this filters is a guess made from letters, and where its wrong answers land is the
/// argument for fencing it: a `def` that exists only under RSpec is a `def` the application
/// never loads, so a cursor in a model that lands on one has been sent somewhere the running
/// program has never been. Nothing above this rung is touched — a *resolved* answer inside a
/// spec is the code saying so, and the answer to that is to check the code.
///
/// The rule, the tag and the cursor gate are all [`environment`]'s, which is what keeps this
/// and `completion`'s list from fencing the same name differently. **A declaration is kept if
/// *any* of its definitions is loadable, and one with no definitions at all is kept** —
/// `Tally::loadable` is where both of those are decided and why.
///
/// [`references`](super::references) is not filtered and must not be, for the reason it does not
/// follow a derived receiver either — a work list of places to edit that quietly omitted the
/// specs is a rename that breaks the suite. `environment`'s own docs hold that table.
fn loadable_from(
    graph: &Graph,
    fence: environment::Fence<'_>,
    resolution: Resolution,
) -> Resolution {
    if !fence.is_on() {
        return resolution;
    }
    Resolution {
        declarations: resolution
            .declarations
            .iter()
            .filter(|id| fence.loadable(graph, **id))
            .copied()
            .collect(),
        ..resolution
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
        // **Never fenced.** `resolve` is what `references`, `rename` and the two hierarchies
        // reach the graph through, and those answer *where is this used*, where a use under
        // `spec/` is a use. `environment`'s table holds the whole of that argument.
        // **`Privacy::Allowed`, and for the same reason the fence is off.** These callers have
        // no buffer to parse, so the one question the gate answers cannot be asked here — and
        // they answer *where is this used* rather than *what does this call reach*, where a use
        // of a private method is a use.
        Target::Call(reference) => resolve_call(
            graph,
            reference,
            environment::Fence::off(),
            Privacy::Allowed,
        ),
        Target::Definition(definition) => {
            Resolution::precise(declaration_of(graph, definition).into_iter().collect())
        }
    }
}

/// The declaration a definition makes — rubydex's own answer, except for the `def` whose receiver
/// it has no arm for and the body whose name is an alias.
///
/// `def Foo.bar` and `def Foo::bar` are a singleton method with the class written out instead of
/// spelled `self`. rubydex attributes `def self.bar` by taking the enclosing namespace's
/// singleton class and asking it for `bar`, and it attributes a plain `def bar` by asking the
/// enclosing namespace itself — but a *named* receiver falls into the second of those, so `bar`
/// is looked for as an **instance** member of whatever namespace the `def` is lexically inside.
///
/// The lookup usually misses, and then a real `def` line answers nothing at all: 85 of the 126
/// such lines over the six corpora are this one spelling, and every corpus has between 13 and 16
/// of them — rexml writes its whole XPath surface this way. Where the namespace does happen to
/// declare an instance method of that name the lookup *succeeds*, and the cursor on `def
/// Foo.bar` reports `Foo#bar`, which is a different method.
///
/// So the named receiver is answered here outright rather than after rubydex has been asked:
/// falling back would mean falling back onto the answer that is wrong.
///
/// **And a body opened under an alias is retried, because a name can stand for a namespace
/// without being one.** Ruby ships `Gem::URI = Bundler::URI`, and the vendored copy beside it
/// writes `module Gem::URI` — which reopens Bundler's module, exactly as Ruby would. Every
/// member the body declares is attributed by the *name* of the body holding it, and that name
/// has a declaration: the alias, which has no members and no singleton class. So the walk stops
/// and a real `def` line answers nothing. Everything else about the same file is already right
/// — the member is declared (`Bundler::URI#self.split`), a call on either spelling resolves to
/// it, and a module nested in the same body answers `Bundler::URI::Schemes` — so what is broken
/// is the reverse map alone. The retry is therefore a **fallback and never an override**:
/// rubydex is asked first, the named receiver's own lookup is tried first, and only silence is
/// answered. `def Ripper.parse` needs it too, because Ruby 4 ships
/// `Ripper = Prism::Translation::Ripper`.
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

/// Which namespace a definition adds a method to, which method, and whether it is the
/// singleton's — for the five definitions that add one by being *written inside* a body.
///
/// This is rubydex's own pairing, read a second time so [`aliased_name`] can retry it. Only the
/// method-declaring kinds are here, and the boundary is deliberate: a class, a module and a
/// constant are attributed by their **own** name rather than by the namespace holding them, so
/// they already resolve under an alias — measured, `module Gem::URI::Schemes` answers
/// `Bundler::URI::Schemes` and a constant in the same body answers too. A variable and a
/// visibility marker are left where upstream puts them.
fn nested_member(graph: &Graph, definition: &Definition) -> Option<(NameId, StringId, bool)> {
    let lexical = |nesting: &Option<DefinitionId>, member: &StringId| {
        Some((enclosing_name(graph, *nesting)?, *member, false))
    };
    let on_self = |owner: &DefinitionId, member: &StringId| {
        Some((*graph.definitions().get(owner)?.name_id()?, *member, true))
    };
    match definition {
        // A named receiver never arrives here: `declaration_of` answers it outright above.
        Definition::Method(it) => match it.receiver() {
            Some(Receiver::SelfReceiver(owner)) => on_self(owner, it.str_id()),
            Some(Receiver::ConstantReceiver(_)) => None,
            None => lexical(it.lexical_nesting_id(), it.str_id()),
        },
        Definition::MethodAlias(it) => match it.receiver() {
            Some(Receiver::SelfReceiver(owner)) => on_self(owner, it.new_name_str_id()),
            // `Foo.alias_method :a, :b` where `Foo` is an alias never reaches here: rubydex
            // panics *while indexing* that file — "Tried to add member to a declaration that
            // isn't a namespace" — and the per-file seam skips it, so there is no definition
            // left to ask about. Where `Foo` is a real class it resolves and this is not
            // reached either.
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
/// A walk rather than one step, because upstream's is: `find_enclosing_namespace_name_id` climbs
/// past every definition with no name of its own. For the five kinds retried above it never has
/// to climb — measured, their nesting is always the body itself — but an instance variable's is
/// the `def` around it, and diverging here would be a silent difference rather than a smaller
/// function. The two branches that costs are the only ones no test in this file takes.
fn enclosing_name(graph: &Graph, mut nesting: Option<DefinitionId>) -> Option<NameId> {
    while let Some(definition) = nesting.and_then(|id| graph.definitions().get(&id)) {
        if let Some(name) = definition.name_id() {
            return Some(*name);
        }
        nesting = *definition.lexical_nesting_id();
    }
    None
}

/// The name an alias finally stands for, and **nothing at all unless a hop was taken**.
///
/// `Thing = 1` is a constant too, and `def Thing.bar` on it must keep answering nothing rather
/// than something near it — so the target is read off a `ConstantAlias` *definition*, which is
/// the only kind that records one. The chain is walked because Ruby permits an alias of an
/// alias, and it is capped because Ruby also permits `A = B` beside `B = A`.
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

/// The class `def Foo.bar` names and the member it declares on it, and nothing for any other
/// definition — a plain `def`, a `def self.`, a constant, an `attr_reader`.
fn named_receiver(definition: &Definition) -> Option<(NameId, StringId)> {
    let Definition::Method(method) = definition else {
        return None;
    };
    match method.receiver() {
        Some(Receiver::ConstantReceiver(receiver)) => Some((*receiver, *method.str_id())),
        Some(Receiver::SelfReceiver(_)) | None => None,
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
///
/// **`privacy` is the caller's answer and not this function's**, for the same reason the fence is.
/// A signature card drawn for a method Ruby refuses would disagree with the jump at the identical
/// cursor, which is the disagreement the paragraph above exists to prevent — so `signature_help`
/// passes what `cursor::Call::allows_private` read off the call node. The outgoing call hierarchy
/// has no cursor to read and passes [`Privacy::Allowed`], which is what it answered before.
#[must_use]
pub fn precise_call(
    graph: &Graph,
    uri_id: UriId,
    offset: u32,
    layout: environment::Layout<'_>,
    privacy: Privacy<'_>,
) -> Option<DeclarationId> {
    locate(graph, uri_id, offset)
        .into_iter()
        .find_map(|located| match located.target {
            // **Fenced like the jump, and not like `resolve`.** The two callers here draw a
            // signature card and a call-hierarchy row for a call written in *this* document, so
            // they are answering *what does this call reach* — the jump's question, at the jump's
            // cursor. Letting them keep a rooted answer `definition` has already refused would
            // put a signature nobody can call under the parameter the user is typing, and the
            // two answers would disagree about one cursor. What they do not do is fall through
            // to the name rung: an exact callee is the whole of their contract, so the fenced
            // case is `None` and the card is simply not drawn.
            Target::Call(reference) => {
                let resolution = resolve_call(
                    graph,
                    reference,
                    environment::Fence::at(uri_of(graph, uri_id), layout),
                    // **Passed in rather than decided here, because this rung is handed an
                    // offset and not a text.** Each caller knows a different amount: the
                    // signature card has the `CallNode`'s own receiver, completion's keyword
                    // arguments are by construction at an implicit one, and the outgoing call
                    // hierarchy walks a body's call sites with no cursor at all and can only
                    // say `Allowed`. What this function may not do is guess on their behalf.
                    privacy,
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
/// The order matters twice over: goto-definition jumps to the first entry, and hover reads the
/// first entry's comments. `Declaration::definitions` is filled as documents are indexed in
/// parallel, so without sorting both would change from run to run.
///
/// **Alphabetical by path is stable and says nothing**, which is the whole of the defect this
/// second sort exists for. Ruby reopens a namespace freely, so a wide one collects a definition
/// per file that ever touched it: `Sidekiq` is written in **69** files on mastodon and `Rails` in
/// **145** on lobsters, and sorting those by path put an rspec helper first and the gem's own
/// `sidekiq.rb` fourth, `railties/lib/rails.rb` twenty-third. Both jump and card then answer
/// from a file that merely sorts early.
///
/// The rule, and there is no other clue available: **the file named after the constant wins,
/// and where no file is named after it the path decides.** See [`named_after`] for both halves
/// and for how weak the second one is. It applies to a **namespace only** — a class or module is
/// what a file is conventionally named for and what the 134 wide-constant positions measured,
/// while a method's file is named after its class, so ranking methods this way would reorder
/// answers on noise.
///
/// **What it cannot do is make a wide namespace's list useful.** A constant reopened in 539
/// files has no definition site; every entry is a `module` keyword wrapping something else, and
/// ruby-lsp answers the same cursor with 463 of the same places in plain path order. The list is
/// the right shape and the order is the only thing there is to get right.
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
    // **Stable, and second**, so that the sort above is still what decides between two files
    // ranked alike — the answer may not move between runs — and so that two definitions in one
    // file stay adjacent, which is what lets [`sites`] deduplicate by looking at its neighbour.
    //
    // Skipped below two, which is nearly every declaration there is: a sort key that allocates
    // is not worth building to order a list that has no order.
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
        .map_or("", rubydex::model::document::Document::uri)
}

/// How close a document's file name is to the name of the thing declared in it — **smaller is
/// better**, and it is a sort key rather than a score anybody reads.
///
/// Both sides are squashed to letters and digits, lowercased, so that `active_record.rb`,
/// `ActiveRecord.rb` and `active-record.rb` are one spelling of `ActiveRecord`. Then the key
/// splits into **two tiers that ask different questions**, because most wide namespaces have no
/// file named after them at all and a question with no answer has to be replaced rather than
/// scored.
///
/// **Is the file named after the constant?** The name has to be *in* the stem — `sidekiq.rb`
/// and `sidekiq_adapter.rb` are both named after `Sidekiq`, and so is `ruby-progressbar.rb` for
/// `ProgressBar`, which is why this is containment and not a prefix — **and it has to be at
/// least half of it**. Without that second half a long file name swallows a short constant:
/// discourse writes `Jobs` in 372 files, and `app/jobs/onceoff/remove_old_auto_close_jobs.rb`
/// held first place because its stem contains `jobs`, which outranks every file that merely
/// sits in `app/jobs/`. Within the tier, **how much longer** the stem is decides, so an exact
/// match scores zero and wins — where **131 of the 134** wide-constant positions measured in
/// 2026-09-12 land.
///
/// **And where nothing is named after it, the file name says nothing at all.** That is the
/// second tier and the defect it exists for: solidus writes `module Spree` in 539 files and not
/// one of them is `spree.rb`, so every candidate tied at *unnamed* and the tie fell to how close
/// the stem's **length** was to the constant's — `setup` is five letters and so is `spree`, and
/// a jump on `Spree` opened `api/lib/spree/api/testing_support/setup.rb`. Comparing the lengths
/// of two unrelated words is noise. What is left that means anything is the **path**: a
/// directory the constant names, and then the file nearest the top of that tree. It answers
/// `plugins/discourse-ai/plugin.rb` for `DiscourseAi` and `app/jobs/base.rb` for `Jobs`.
///
/// **The second tier is a weak rule and is stated as one.** Where a namespace is reopened in
/// hundreds of files there is no definition of it to find — every one of those 539 is a `module`
/// keyword wrapping something else — so this picks a plausible door and not the right one. Of
/// the 91 lists it reorders across the six corpora, about ten land somewhere a reader would have
/// chosen and the rest move from one arbitrary reopening to another.
///
/// The name is taken unqualified: a file is called `worker.rb`, not `sidekiq_worker.rb`, for
/// `Sidekiq::Worker`. Squashing also takes the angle brackets off a singleton's name, so
/// `Person::<Person>` asks about `person` and needs no case of its own, and a name that squashes
/// to nothing is *unnamed* everywhere, which leaves the path to decide as it does for `Spree`.
///
/// Read off the URI rather than a decoded path. Only the file's own name and the directories
/// above it are looked at, and neither is a name percent-encoding reaches in practice; a
/// directory that did need escaping squashes the escape's own characters in and fails to match,
/// which costs a tie-break and never an answer.
///
/// **A document that is not a file sorts behind every one that is**, before either tier is
/// asked. rubydex declares Ruby's object model itself under `rubydex:built-in` and ya-lsp's own
/// generators write a URI of the same shape — deliberately not `file:`, so that the editor can
/// never be handed one — and both would otherwise win a chain on a name they have no file for.
/// `class BasicObject` disappearing from a supertype chain is what this costs when it is left
/// out: `rubydex:built-in` squashes to a fourteen-character word that beat the `.rbs` really
/// declaring it, and `preferred_definition` then picked a place no request may point at.
fn named_after(wanted: &str, uri: &str) -> (bool, bool, usize, bool, usize) {
    let stem = squashed(stem_of(uri));
    // Sorting puts `false` and the smaller number first, so each question is asked the way the
    // key wants to read. Three of the five fields are meaningful in one tier only, and the other
    // tier writes the neutral value rather than there being two key shapes to compare.
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

/// A URI with its scheme taken off, so that the second tier reads directories and nothing else.
///
/// `file:` squashes to the four letters `file` — the colon is not alphanumeric and falls out with
/// the separators — so a scheme left on makes **every** document sit in a directory named after
/// `File`, and lobsters' two places for `class File` swapped on it. Found by the corpora and
/// pinned by a test, which is the only reason the rest of this key can be trusted.
fn path_of(uri: &str) -> &str {
    uri.split_once("://").map_or(uri, |(_, path)| path)
}

/// Does a directory above the file carry the constant's own name?
///
/// The last segment is the file and [`named_after`]'s first tier has already asked about it;
/// everything before it is the tree the file sits in, and `plugins/discourse-ai/` is what says
/// `DiscourseAi` lives there. Compared without building a string per segment, because this runs
/// once per definition of a namespace that may have hundreds of them.
///
/// Takes a **path**, not a URI: see [`path_of`] for what a scheme left on this does.
fn directory_named(wanted: &str, path: &str) -> bool {
    let Some((directories, _)) = path.rsplit_once('/') else {
        return false;
    };
    directories
        .split('/')
        .any(|segment| squashes_to(segment, wanted))
}

/// Does one path segment squash to exactly this name, without allocating to find out?
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

/// A declaration's own name, without the namespaces above it.
///
/// A file is called `worker.rb`, not `sidekiq_worker.rb`, for `Sidekiq::Worker`.
fn unqualified(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Letters and digits, lowercased, and nothing else.
fn squashed(text: &str) -> String {
    text.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

/// Which single definition of a declaration a list should point at.
///
/// The user's own code wins when a name is defined in both: opening a Rails app and searching
/// for `ApplicationRecord` should land in `app/models`, not in whichever gem reopens it.
/// Otherwise it is the first in [`definitions_of`]'s stable order, so the answer never moves
/// between runs.
///
/// **And, among the project's own, the copy the application loads.** A name the project writes
/// in both environments is usually written once in the application and many times under
/// `spec/` — `Object#create_category` is one `def` in a rake task and forty in spec files — and
/// which of the forty-one sorts first is an ordering nobody chose. A migration is the third
/// tree this reads, for the same reason and at a much smaller size: the two sorts above
/// already prefer a file named after the constant, and then the one nearest the top of its
/// tree, so a migration reaches the front only where the application's own copy is both
/// unnamed and deep. It is here for consistency rather than for a measured population — no
/// corpus row moved — and the test says exactly how narrow the case is. This is a tie-break and not
/// a fence: a declaration written *only* in a test tree still gets its own row here, because a
/// row that points somewhere is worth more than a row that points nowhere, and the surfaces
/// that care whether it should be listed at all have already decided that
/// ([`environment`](super::environment) holds that table).
///
/// The tag is read here rather than through `environment::Trees` because this runs once per
/// *row* — the symbol picker computes sites only for the survivors — so a string split per
/// definition of one declaration is cheaper than a set built per request.
///
/// Shared by the symbol picker and by the type hierarchy, because "which of the two hundred
/// places `ActiveRecord::Base` is reopened does this row mean" is one question, and two answers
/// to it would put a symbol in a different file depending on which list it was found in.
#[must_use]
pub fn preferred_definition<'g>(
    graph: &'g Graph,
    declaration_id: DeclarationId,
    own: &HashSet<UriId>,
    names: environment::Names<'_>,
) -> Option<&'g Definition> {
    let definitions = definitions_of(graph, declaration_id);
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
        .find(|definition| own.contains(definition.uri_id()) && loadable(definition))
        .or_else(|| {
            definitions
                .iter()
                .find(|definition| own.contains(definition.uri_id()))
        })
        .or(definitions.first())
        .copied()
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

/// Where a reader is *sent*, which is narrower than [`sites`] in four ways.
///
/// **A signature is a declaration, not a definition.** `stdlib/cgi-escape/0/escape.rbs` says
/// what `CGI.escape` takes and what it hands back; it is not where anybody wrote the method,
/// and jumping there lands the reader in a stub with no body. So an `.rbs` is dropped wherever
/// real source survives beside it — and kept where none does, because a signature is a better
/// answer than silence. Measured over the five corpora: **661 positions offer a signature
/// beside the source that defines the same method, and 216 answer with a signature and nothing
/// else.**
///
/// **A copy the project would never load is not a second place.** A bundle that pins `cgi` puts
/// the gem's `lib/cgi/escape.rb` on the load path ahead of the copy inside Ruby, and `require`
/// reads exactly one of the two. Both are indexed, so both are definitions of
/// `CGI::Escape#escape`, and the card says *Defined in 3 places* about one method in one
/// library. Which copy wins is not a new question — it is the order of `load`, which is
/// `Workspace::load_paths`' order, which is Bundler's. **350 positions carry at least one such
/// copy**, `uri/generic.rb`, `net/http/header.rb` and `prism/node.rb` the commonest.
///
/// **A document the editor cannot open is not a place at all.** rubydex declares `Module` and
/// `Kernel` itself, under `rubydex:built-in`, and [`DocUri::from_uri_str`] refuses that URI for
/// every request alike — so the jump has always dropped it and only the count was carrying it.
/// It is one position in the five corpora — a reference to `Module` in a lobsters model,
/// reading *Defined in 18 places* over a jump that offers 17 — and it is fixed here rather than
/// in the counter because the list is what was wrong.
///
/// **And a copy only the suite loads is not a place a reader in application code asked for.**
/// Ruby reopens freely, so a namespace collects a definition per file that ever touched it and
/// a monorepo's specs are most of those files: solidus offers **539** places for `Spree` and
/// **76** of them are under `spec/`. This is [`environment`](super::environment)'s rule reaching
/// a *place list* for the first time — the same walk the name rung, the completion list and the
/// picker's rank already make — and it is a **drop** rather than a rank because a jump sends the
/// reader somewhere and a peek list is read from the top. Same two safety clauses as the rule
/// above it: kept where **no** loadable place survives beside it, since a declaration only a
/// spec writes is still better answered than not, and turned off entirely where the cursor is
/// itself in a test tree or a `testing_support` tree, because a developer working in a spec is
/// exactly who the spec's copy is for. Measured over 4,501 constant cursors outside the test
/// trees: **414 positions carry one, 25,055 places dropped, and 0 positions emptied.**
///
/// **A third clause, and it is what `layout` is for: only *this project's* test trees are test
/// trees.** `environment`'s tag is four directory names read off a path and it was written
/// against the project's own — a gem that ships `lib/rack/test/` is publishing a library, not a
/// suite, `railties` puts its `rails/commands/test/` there, and Ruby's own minitest signatures
/// sit under `minitest/test/`. Without the clause this fence took 81 lists off lobsters, a
/// corpus with **no** project test tree in any place list at all. The clause was this surface's
/// alone for an afternoon, which was itself the defect: the name rung answered `null` for a
/// method rack-test really does declare. It is
/// [`environment::Fence::unloadable`](super::environment::Fence::unloadable) now, and
/// every surface that fences reads that one function.
///
/// `cursor` is the document the question was asked from, and `None` means *no cursor* rather
/// than *nowhere* — [`environment::fenced_from`](super::environment::fenced_from) reads it as
/// unfenced, which is the conservative direction. `completionItem/resolve` is the caller with
/// none: the protocol hands back an item and not a position.
///
/// `references` deliberately asks [`sites`] instead, and the difference is not an oversight:
/// every mention is every mention, and a rename that skipped the project's own `sig/` would
/// leave a signature naming a method that no longer exists. The same sentence is why this fence
/// stops here and `sites` never learns it.
#[must_use]
pub fn places(
    graph: &Graph,
    synthesized: &Synthesized,
    layout: environment::Layout<'_>,
    declaration_id: DeclarationId,
    cursor: Option<&str>,
) -> Vec<Site> {
    let mut places = sites(graph, synthesized, declaration_id);
    places.retain(|place| DocUri::from_uri_str(&place.uri).is_some());
    let shadowed = shadowed(&places, layout.load);
    places.retain(|place| !shadowed.contains(&place.uri));
    if places.iter().any(|place| !is_signature(&place.uri)) {
        places.retain(|place| !is_signature(&place.uri));
    }
    let fence = environment::Fence::at(cursor, layout);
    if fence.is_on() && places.iter().any(|place| !fence.unloadable(&place.uri)) {
        places.retain(|place| !fence.unloadable(&place.uri));
    }
    places
}

/// Every place a whole resolution names, each of them once.
///
/// **Deduplicated across declarations and not only inside one**, which [`places`] cannot do
/// because it is asked about one declaration at a time. That distinction had no cost while
/// every generated declaration was placeless: a name rung that answered `find_each` offered
/// `ActiveRecordRelation#find_each`, `Story::Relation#find_each` and one more per relation
/// class in the project, and all of them dropped out for having nowhere to point. Now that they
/// point at the `def` in the bundle they are one place claimed several times, which is
/// `Defined in N places` overstating N — the defect the footnote was already fixed for once.
///
/// First occurrence wins, so the ranking [`definitions_of`] and [`places`] just applied is
/// exactly the order a reader sees.
#[must_use]
pub fn all_places(
    graph: &Graph,
    synthesized: &Synthesized,
    layout: environment::Layout<'_>,
    declarations: impl IntoIterator<Item = DeclarationId>,
    cursor: Option<&str>,
) -> Vec<Site> {
    let mut seen: HashSet<(String, u32, u32)> = HashSet::new();
    declarations
        .into_iter()
        .flat_map(|id| places(graph, synthesized, layout, id, cursor))
        .filter(|site| seen.insert((site.uri.clone(), site.full.0, site.full.1)))
        .collect()
}

/// Which of a list's places are a copy of a file that another of them shadows.
///
/// Keyed by what `require` would be asked for — the path under a load path, which is the same
/// string for Ruby's own `uri/generic.rb` and for the `uri` gem's — and won by the load path
/// that comes first, because that is the one `require` searches first.
///
/// A place under no load path has no such key and is never shadowed. That is most of them: a
/// project's own files, a gem's `app/` and a gem's `sig/` are all indexed and none of them is
/// on a load path, and none of them is a copy of anything.
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

/// What `require` would be asked for to reach this document, and which load path answers it.
///
/// The **first** prefix that matches, because that is the one `require` searches first: Ruby's
/// platform directory sits inside its library directory, so more than one entry can match one
/// document and the order is the whole answer.
fn under<'a>(uri: &'a str, load: &[String]) -> Option<(usize, &'a str)> {
    load.iter()
        .enumerate()
        .find_map(|(rank, prefix)| uri.strip_prefix(prefix.as_str()).map(|rest| (rank, rest)))
}

/// Whether a document is RBS rather than Ruby.
///
/// By the extension, which is the same thing `Workspace::indexes` decides a file is signatures
/// by — rubydex records a declaration from an `.rbs` exactly as it records one from a `.rb`,
/// so the file name is the only evidence there is.
fn is_signature(uri: &str) -> bool {
    uri.ends_with(".rbs")
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

/// Whether a **private** declaration may answer the call under the cursor.
///
/// Ruby's rule is about how the receiver was *written*: a private method is callable with no
/// receiver at all, or with one spelled `self`, and `other.secret` raises `NoMethodError` even
/// from inside the class that declares `secret`. [`cursor::Context::allows_private`] reads that
/// off the syntax and is the only thing that can, so the answer travels into [`resolve_call`] as
/// a parameter the way the environment fence does.
///
/// **It cannot be recovered from the graph, which is why it is a parameter and not a lookup.**
/// rubydex's `MethodRef` carries `Option<NameId>` for the receiver and nothing else, and it
/// fills that name *both* for `Foo.bar` and for a bare call written in `Foo`'s own body — the
/// two cursors this question has opposite answers at. Nor from upstream's own check, which is a
/// different rule and a looser one: it passes a private method whenever the caller's `self` is the
/// same class as the receiver, where Ruby's exemption is for the receiver being *written* `self`.
#[derive(Clone, Copy)]
pub enum Privacy<'a> {
    /// The syntax permits one — an implicit receiver, or one spelled `self`. Also what every
    /// caller with no text to read passes, because refusing on a question it cannot ask would
    /// be deleting answers on a guess.
    Allowed,
    /// A receiver is written and it is not `self`, so Ruby would raise here — **and the second
    /// opinion the refusal needs**, because the record it would refuse on is wrong for one
    /// ordinary shape. See [`Modifiers`]. It rides in this arm rather than beside the enum
    /// because this is the only arm that ever asks: `Allowed` admits without a lookup.
    Refused(&'a Modifiers<'a>),
}

impl<'a> Privacy<'a> {
    /// The same answer from a caller that has already read the syntax itself.
    ///
    /// `signature_help` holds a [`cursor::Call`](super::cursor::Call) rather than a `Context` —
    /// the two classify a cursor differently on purpose, see `Call`'s own docs — so it arrives
    /// with the bool and not with the enum.
    pub(super) fn written(allows_private: bool, modifiers: &'a Modifiers<'a>) -> Self {
        if allows_private {
            Privacy::Allowed
        } else {
            Privacy::Refused(modifiers)
        }
    }

    /// What the syntax at the cursor permits.
    ///
    /// One function rather than the `if` written at each rung, because there are three of them —
    /// the resolved member, the derived one and the name list — and a gate spelled three times
    /// is a gate that will one day be spelled differently in one of them.
    fn at(context: &Context, modifiers: &'a Modifiers<'a>) -> Self {
        if context.allows_private() {
            Privacy::Allowed
        } else {
            Privacy::Refused(modifiers)
        }
    }

    /// Whether this declaration may be the answer.
    ///
    /// Non-methods pass untouched: only a method has a visibility, and a constant reached
    /// through a namespace is not what this gate is about.
    fn admits(self, graph: &Graph, id: DeclarationId) -> bool {
        match self {
            Privacy::Allowed => true,
            Privacy::Refused(modifiers) => !is_private(graph, modifiers, id),
        }
    }

    /// The same refusal over a list of candidates, which is what the name rung is.
    ///
    /// **It has to be applied to the list as well as to the precise rungs above it, or the
    /// refusal has a side door.** A private declaration the ancestor walk just declined is
    /// matched by its own name like any other, so gating only the rung that found it hands the
    /// same `def` back one tier down — a *Guessed* card naming the method Ruby raises on. Where
    /// that leaves nothing, nothing is the answer: `RSpec.describe` has no public `describe`
    /// anywhere, and rspec-core defines it dynamically, so the correct card is no card.
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

/// Whether this declaration is private, by rubydex's record and by the source that record was
/// read from.
///
/// The same test [`completion::reachable`](super::completion) applies, and deliberately not more
/// than it: Ruby's five always-private names are `completion`'s list because they are a fact
/// about what may be *offered*, and applying them here would refuse `Foo.new` — which resolves
/// to `Foo#initialize` and is the one redirect navigation exists to make. `holds_private` skips
/// a redirect for that reason.
///
/// **The second half is a repair on the record and not on the rule.** The record is wrong for
/// one shape and only one, and [`Modifiers`] is the whole of it; the ordering here is what keeps
/// it cheap, because a declaration rubydex calls public is answered by the first line and never
/// reads a byte.
fn is_private(graph: &Graph, modifiers: &Modifiers<'_>, id: DeclarationId) -> bool {
    matches!(
        graph.visibility(&id),
        // rubydex treats `module_function`'s instance copy as private, and so does Ruby.
        Some(Visibility::Private | Visibility::ModuleFunction)
    ) && modifiers.confirm(graph, id)
}

/// Whether a precise resolution rests on a private declaration, and is therefore worth reading
/// the syntax to check.
///
/// **The redirect is exempt and that exemption is the whole risk in this change.** `Foo.new`
/// resolves to `Foo#initialize`, which Ruby privatises by name at the point of definition — so a
/// gate that did not skip it would refuse every constructor in every workspace, at a receiver
/// that is written and is never `self`.
fn holds_private(graph: &Graph, modifiers: &Modifiers<'_>, resolution: &Resolution) -> bool {
    !resolution.redirected
        && resolution
            .declarations
            .iter()
            .any(|id| is_private(graph, modifiers, *id))
}

/// rubydex's visibility record, and the one question it answers wrongly.
///
/// A bare `private` is a **statement**, and rubydex applies it to the body it is written in
/// until that body ends. A block is not a body to it: `class_methods do … private … end` sets
/// the *module's* default visibility, so every `def` written below the block — public methods,
/// in an ordinary Rails concern — is recorded private. `HasCustomFields#upsert_custom_fields`
/// is the measured one: discourse calls it on explicit receivers in shipped code, and the gate
/// above refused all of them.
///
/// **Ruby agrees with rubydex for one kind of block and disagrees for the other, and nothing in
/// the syntax says which is which.** A bare `private` sets the visibility on the *cref*, and a
/// plain iterator block shares the cref it was written in — so `[1].each { private }` really
/// does privatise the `def` below it. `module_eval` and `class_eval` give the block a cref of
/// its own, so `class_methods do`, `concerning`, `included do`, `Class.new do` and every RSpec
/// example group do not. Telling the two apart means knowing what the method holding the block
/// does with it, which is a fact about a gem and not about this file.
///
/// So the rule here is the one that does not refuse: **a bare modifier governs only the body it
/// is written in, and a block body is a body.** It is Ruby's rule for the eval forms, it is not
/// Ruby's rule for a plain iterator block, and the census says the second shape does not occur:
/// over 24,054 Ruby files in the six corpora, 49 `def`s in 9 files are recorded private by a
/// modifier that escaped a block, and **every one of those blocks is an eval form** —
/// `class_methods do` at 20 of them, an RSpec group or a `Conversion::Step` DSL at the rest.
/// Where the two readings disagree the refusal is the assertion, and an assertion is not a
/// thing to make from the reading that cannot be checked.
///
/// **What it does not repair is the other direction.** A `public` written inside a block escapes
/// it just as far, so a genuinely private `def` below one is recorded public — and nothing here
/// widens a refusal, only narrows one. That is a missing refusal rather than a false one, and it
/// is left alone deliberately: this type exists to stop the gate asserting what the source does
/// not say, not to assert more than rubydex did.
pub struct Modifiers<'a> {
    /// The declaring document's own text — the buffer where one is open, which is why this is
    /// the closure `types::Sources` already carries rather than a read of the file.
    read: &'a dyn Fn(&str) -> Option<(String, Rebase)>,
    /// One parse per declaring document, for the life of one request. The gate refuses rarely,
    /// but the name rung hands it a *list*, and several private candidates in one file would
    /// otherwise be several parses of it.
    escapes: RefCell<HashMap<UriId, HashSet<u32>>>,
}

impl<'a> Modifiers<'a> {
    #[must_use]
    pub fn new(read: &'a dyn Fn(&str) -> Option<(String, Rebase)>) -> Self {
        Self {
            read,
            escapes: RefCell::new(HashMap::new()),
        }
    }

    /// Whether rubydex's `private` for this declaration survives a reread of the source.
    ///
    /// **One `def` that is really private is enough**, because a declaration is every `def` of
    /// one name on one owner and Ruby's own answer is whichever ran last. Reading it the other
    /// way — public if any definition escaped — would let one leaked `def` in one file unlock a
    /// method another file writes under a plain `private`.
    ///
    /// Public because `completion` asks it too. One wrong record read by two surfaces has to be
    /// repaired for both or they disagree about one cursor: a jump that lands and a list that
    /// will not offer the name it landed on.
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
    /// Public for `symbols`, which renders a *definition* rather than resolving a declaration:
    /// an outline row is one `def` and says so, where every other reader here is asking about a
    /// name on an owner.
    #[must_use]
    pub fn escaped(&self, graph: &Graph, definition: &Definition) -> bool {
        let mut escapes = self.escapes.borrow_mut();
        let found = escapes
            .entry(*definition.uri_id())
            .or_insert_with(|| self.reread(graph, definition.uri_id()));
        found.contains(&definition.offset().start())
    }

    /// Every such `def` in one document, in the graph's coordinates.
    ///
    /// **Nothing readable is nothing escaped**, which is the direction every other refusal in
    /// this module falls in: a document with no text to read keeps the record rubydex wrote.
    fn reread(&self, graph: &Graph, uri_id: &UriId) -> HashSet<u32> {
        let Some(document) = graph.documents().get(uri_id) else {
            return HashSet::new();
        };
        // **A signature has no blocks to escape from.** RBS spells visibility with its own
        // `private` keyword inside a declaration that a block cannot open, and rubydex indexes
        // it through a different indexer entirely — so there is nothing here to repair, and
        // parsing Ruby's core signatures as Ruby on the way to finding that out would put the
        // largest files in the workspace on the one path that has to stay cheap.
        if document.uri().ends_with(".rbs") {
            return HashSet::new();
        }
        let Some((text, rebase)) = (self.read)(document.uri()) else {
            return HashSet::new();
        };
        let parsed = ruby_prism::parse(text.as_bytes());
        let mut walk = Escapes::default();
        walk.visit(&parsed.node());
        walk.found
            .iter()
            .filter(|(_, name)| !walk.named.contains(name))
            // The text read is the buffer's and the offsets compared against are the graph's,
            // which are two different strings the moment somebody types. A `def` in the part of
            // the file the edit moved has no graph offset at all, and a `def` with no graph
            // offset is one the gate is not being asked about.
            .filter_map(|(at, _)| rebase.to_graph(*at))
            .collect()
    }
}

/// The walk behind [`Modifiers`]: every `def` the two readings of a bare modifier disagree about.
///
/// **There is no stack, because the visitor's own recursion is one.** A construct that opens a body
/// saves this frame, walks with a fresh one and puts the saved one back; a block walks with *this*
/// frame and restores only half of it on the way out, which is the whole of the disagreement.
#[derive(Default)]
struct Escapes {
    /// The body being walked. `.0` is rubydex's reading — a block writes through to the body
    /// around it — and `.1` is the reading where what a block set dies with the block.
    frame: (bool, bool),
    /// `(offset of the `def`, its name)`, in the text that was parsed.
    found: Vec<(u32, String)>,
    /// Every name a visibility call *named* — `private :foo`, `private def foo`. rubydex reads
    /// both of those correctly and no block can have leaked them, so a `def` whose name is here
    /// is private for a reason this walk has no business overturning.
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

    /// The one line the whole type is about: the block hands back `.0` and gives `.1` up.
    fn visit_block_node(&mut self, node: &BlockNode<'pr>) {
        let outer = self.frame;
        ruby_prism::visit_block_node(self, node);
        self.frame = (self.frame.0, outer.1);
    }

    /// A `private` written in a method body is a call at run time and not a modifier on this
    /// file's text, so the body gets a frame of its own and never writes into the class'.
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
            // **Arguments and not `bare`, because a block is not an argument.** `private do … end`
            // parses, and Ruby reads it as the modifier — it is the argument list that decides
            // whether a visibility call names something or sets a default.
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

fn resolve_call(
    graph: &Graph,
    reference: &MethodRef,
    fence: environment::Fence<'_>,
    privacy: Privacy<'_>,
) -> Resolution {
    let Some(member) = member_name(graph, *reference.str()) else {
        return Resolution::precise(Vec::new());
    };
    let member_id = StringId::from(&member);

    // **What the privacy gate refused, and on which receiver.** The fall-through below is the
    // name rung, which knows nothing about the receiver — so without this the card would say
    // *the receiver's type is unknown* of a type the graph had just handed over. That is the
    // exact false sentence this whole gate exists to remove, reintroduced one rung down, and it
    // is what `audit` check 6 raised at 31 cursors the moment the gate landed.
    let mut refused_on: Option<DeclarationId> = None;

    // Whether `self` here is a **class object** — `Foo.bar`, or a bare call written as a
    // statement of a class or module body. rubydex answers it by giving both the singleton
    // class as the receiver, and a bare call inside a `def` the class itself, so this is one
    // question with one answer rather than a syntactic test. The name rung's fallback filter turns
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
            // and `Object`, so a module extended onto it was always meant to be reached first.
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
            // The root answer is kept wherever the extend repair has nothing, so a genuine
            // top-level `def` called from a class body below it resolves unchanged.
            Ok(found) if owned_by_a_root(graph, found) => {
                let answer = extended_member(graph, owner, member_id).unwrap_or(found);
                // **The one precise answer the test-tree fence applies to, and it has to be
                // applied here rather than in `loadable_from`.** A hit on a root is a hit on
                // every receiver in the workspace, so a name only the suite declares — which,
                // via `Object`, is most of what a spec's top-level `def` produces — answers
                // confidently for application code that could never call it. Measured over the
                // six corpora at 800 bare calls each: 31 cursors answered this way and every one
                // was wrong, a migration's `execute` landing in a plugin's spec and a service
                // object's `model` in a migrations-tooling spec.
                //
                // **And the fall-through is the name rung, not silence.** The list below holds
                // the gem's real `ActiveRecord::Migration#execute`; `resolve_typed` fences it in
                // turn, so the spec's copy does not come back through the side door, and what
                // the reader gets is a *Guessed* card naming the right method instead of a
                // *Resolved* one naming the wrong one.
                if fence.loadable_on_a_root(graph, answer) {
                    if privacy.admits(graph, answer) {
                        return Resolution::precise(vec![answer]);
                    }
                    // Refused for privacy and not by the fence, which are different reasons and
                    // owe the card different footnotes: the fence's fall-through is a *better*
                    // answer elsewhere, this one is the same class keeping the member to itself.
                    refused_on = Some(owner);
                }
                return refusing(
                    graph,
                    by_name(
                        graph,
                        &member,
                        attached_class(graph, owner).is_some(),
                        privacy,
                    ),
                    refused_on,
                );
            }
            // **A private hit is refused rather than walked past**, and the walk is not resumed
            // above it. Ruby's own lookup stops at the first match and raises; a search that
            // carried on to the next ancestor would answer with a method the interpreter never
            // reaches, which is a second wrong answer rather than a repair of the first.
            Ok(found) if privacy.admits(graph, found) => {
                return Resolution::precise(vec![found]);
            }
            Ok(_) => refused_on = Some(owner),
            Err(FindMemberError::MemberNotFound) => {}
            Err(error) => {
                tracing::debug!("receiver {owner} is not searchable: {error:?}");
            }
        }
        if let Some(found) = extended_member(graph, owner, member_id) {
            if privacy.admits(graph, found) {
                return Resolution::precise(vec![found]);
            }
            refused_on = Some(owner);
        }
        on_a_class_object = attached_class(graph, owner).is_some();
    }

    // No receiver, or a receiver that does not have the method — or one that has it and may not
    // call it.
    refusing(
        graph,
        by_name(graph, &member, on_a_class_object, privacy),
        refused_on,
    )
}

/// The name rung, told which receiver kept the member private so its footnote can say so.
///
/// **`None` leaves the resolution exactly as it was**, which is every fall-through that was not
/// a refusal: a fence, a miss on the ancestor chain, a call with no receiver written at all.
/// Only the gate fills it, and only where it actually refused something.
fn refusing(
    graph: &Graph,
    resolution: Resolution,
    refused_on: Option<DeclarationId>,
) -> Resolution {
    let Some(owner) = refused_on else {
        return resolution;
    };
    Resolution {
        missed: missed_on(graph, owner, None, true),
        ..resolution
    }
}

/// Every declaration whose name ends in this method, which is the whole of the name rung.
///
/// `Person#shout()` and `Person::<Person>#shout()` both contain `#shout()`, and nothing else
/// does. It is a function of its own because two callers reach it: the tail of [`resolve_call`],
/// where no receiver was named, and its root arm, where one was and the answer it produced is a
/// `def` the application never loads.
fn by_name(
    graph: &Graph,
    member: &str,
    on_a_class_object: bool,
    privacy: Privacy<'_>,
) -> Resolution {
    let query = format!("#{member}");
    let candidates = query::declaration_search(graph, &[&query], &MatchMode::Exact);
    let candidates = if on_a_class_object {
        reachable_on_a_class_object(graph, candidates)
    } else {
        candidates
    };
    Resolution {
        // **Last, and after the class-object filter rather than before it.** That filter falls
        // back to the whole list when nothing survives it, so the two orders are not the same
        // list — and `typed` applies this same refusal to a list this function already returned,
        // which it may only do if the refusal is the outermost step here too.
        declarations: privacy.keep(graph, candidates),
        precise: false,
        redirected: false,
        derivation: Derivation::default(),
        // The one caller that can fill this fills it after the fact — see the `Err` arm of
        // `typed`. Here there is no receiver to report: this *is* the rung below one.
        missed: None,
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

/// A member of a module the receiver's class object really `extend`s, where rubydex missed the
/// edge.
///
/// **This is a repair and not a convention.** Every `extend` rubydex linearizes is found by the
/// ordinary ancestor search, which runs first; what reaches here is the one shape it does not —
/// see [`extends_written_on`] for the four probes that isolated it. The Rails half that used to
/// live beside this is gone: a concern's class methods are **declared** now, by
/// `workspace/rails/concerns.rs`, onto every class that includes the concern, so an ordinary
/// ancestor walk finds them and nothing in this module knows the word.
///
/// It is asked **after** the ordinary ancestor search and only when that found nothing, so a
/// method a class really declares can never be displaced by one it extends.
fn extended_member(
    graph: &Graph,
    singleton: DeclarationId,
    member: StringId,
) -> Option<DeclarationId> {
    extended_modules(graph, singleton)
        .into_iter()
        .find_map(|found| declared_by(graph, found.module, member))
}

/// What the module itself declares, and deliberately **not** what its ancestors do.
///
/// `extend M` really does install the instance methods of `M`'s ancestors, so an ancestor walk
/// is the right reading of Ruby and the wrong reading of the graph. rubydex records an
/// `include` written **inside a `def`**
/// as a mixin of the enclosing namespace, and that is exactly how the modules that reach this
/// are written:
///
/// ```ruby
/// module ClassMethods
///   def has_secure_password(...)
///     include ActiveModel::Validations   # the *record's*, when the macro is called
///   end
/// end
/// ```
///
/// Over Rails' five core gems and the six corpora, 125 such modules hold **2** mixins written in
/// the module body and **19** written inside a `def`, and every one of the 19 means the class the
/// macro was called on. The applications write none of either: their own 20 blocks hold no
/// module-body mixin, and the 4 in-`def` ones are in a vendored gem the default include excludes.
/// Following them made `Category.valid?` — which raises in Ruby — answer with
/// `ActiveModel::Validations#valid?`, and put six of that module's *instance* methods into the
/// completion list for a class object.
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
    /// The module — where the members themselves are declared.
    pub module: DeclarationId,
    /// How many classes the receiver's chain passes through before reaching the one whose body
    /// wrote the `extend`, which is where the module sits in Ruby's *singleton* chain.
    ///
    /// It is not the module's own step in the instance ancestor list, and that distinction is the
    /// whole of the ranking argument. The two chains have different lengths: a Rails model's
    /// instance ancestors are forty rungs of concerns and its singleton chain is five classes, so
    /// a module at instance step 12 would score its members *below* `Object`'s own. Counting only
    /// the classes gives the position of the singleton the `extend` really landed on.
    pub step: usize,
}

/// Every module a class object `extend`s and rubydex did not linearize, nearest first.
///
/// The walk both halves of the repair share: [`extended_member`] takes the first module that
/// answers one member and `completion` collects every member of all of them, so the one shape
/// this is about — [`extends_written_on`]'s table — is stated once. An empty answer is the
/// ordinary case: a receiver that is not a class object, or a chain whose `extend`s the graph
/// already holds.
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
    // Ancestor order, which is the linearization of the `include`s — and the order the
    // `extend`s ran in, because each of them happens at the moment its `include` does.
    for ancestor in namespace.ancestors() {
        let Ancestor::Complete(id) = ancestor else {
            continue;
        };
        // **Only where the receiver's singleton chain really passes through this declaration's
        // singleton**: the attached declaration itself, and the classes above it. An `extend`
        // written in an *included module* lands on that module's own singleton and never on the
        // includer's, so walking it would install methods Ruby does not.
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
/// [`extended_member`], which resolution asks **after** the ordinary ancestor search came
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
/// The other half of the class-object answer, and the half a user meets first: `scope` in a
/// model body offered 37 candidates headed by a *routing* method and `belongs_to` offered 7
/// headed by a migration method, because the name-based list is every declaration in the graph
/// whose name ends this way and nothing narrowed it.
///
/// **An instance method of a class is never it.** What a class object answers is its own
/// singleton chain — the singleton classes of its ancestors, plus whatever is `extend`ed onto
/// it, plus the instance methods of `Class`, `Module`, `Object` and `Kernel` — and rubydex puts
/// every one of those in the singleton's own ancestors, which the search above already walked
/// and did not find the member in. So a surviving candidate owned by a `class` is provably
/// unreachable, while one owned by a `module` is exactly the case this list must keep: a
/// module's *instance* method reached through an `extend` is exactly that, and it is the right
/// answer for every site the walk above could not resolve.
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
///
/// Shared with [`completion`](super::completion) for the same reason [`extended_modules`]
/// is: the closure rung and the list beside it have to mean the same class by "the class this
/// block's `self` is an instance of", and one of them stating it is how that stays true.
pub(super) fn attached_class(graph: &Graph, receiver: DeclarationId) -> Option<DeclarationId> {
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

/// The five owners that mean the ancestor walk ran out of the project and into Ruby.
///
/// [`ROOTS`] plus the two that only a class object reaches. It is a longer list than `ROOTS`
/// because it is asked about both sides of the object model, and the same list for both because
/// a macro's symbol is looked up on both.
const OBJECT_MODEL: [&str; 5] = ["Object", "Kernel", "BasicObject", "Module", "Class"];

/// Whether the member found is one of Ruby's own rather than one of this project's.
///
/// **The one filter on [`resolve_symbol`]**, and it is not a refinement. Every class in a Ruby
/// project inherits `Object` and `Kernel`, so an ancestor walk *always* terminates somewhere —
/// and `Kernel` alone declares `format`, `p`, `print`, `select`, `system`, `test`, `open`, `sub`
/// and `warn`, every one of which is also an ordinary column name. `validates :format` on a model
/// with no `format` column would otherwise jump into `vendor/rbs`: a Resolved answer, in a file
/// the reader did not write, about a method nobody was asking about.
///
/// The reverse can never be lost by it. A member the class really has is found on the class
/// itself, before the walk reaches a root — so this only ever fires where the honest answer was
/// already nothing.
pub(super) fn ruby_s_own(graph: &Graph, found: DeclarationId) -> bool {
    OBJECT_MODEL
        .iter()
        .any(|owner| owned_by(graph, found, owner))
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
    //
    // This admits a candidate; it does not rank one. A span that merely reaches the cursor
    // loses to any span that begins at it — `locate` tiers them before it compares widths — so
    // being generous here costs nothing where a better answer exists and is the only answer
    // where none does.
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

    /// A workspace with the bundle turned off, so that what is on the load path is exactly the
    /// two directories the default configuration names and nothing a machine happens to have
    /// installed.
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
        // Ruby reopens a namespace freely, so a wide one collects a definition per file that
        // ever touched it — 69 files write `module Sidekiq` on mastodon and 145 write
        // `module Rails` on lobsters. Alphabetical by path is stable and says nothing, and it
        // put an rspec helper above the gem's own `sidekiq.rb`. Both halves of the order are
        // asserted here because both are read: `definition` jumps to the first entry and the
        // card takes its prose from it.
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
        // Most wide namespaces land here: solidus writes `module Spree` in 539 files and not one
        // of them is `spree.rb`, so every candidate ties at *unnamed* and whatever decides the
        // tie is the whole answer. It used to be how close the stem's **length** was to the
        // constant's, which compares two unrelated words — `setup` is five letters and so is
        // `spree`, and the jump opened `testing_support/setup.rb`. What is left that means
        // anything is the path: a directory the constant names, and then the file nearest the
        // top of that tree.
        //
        // Both files below score the old key identically — `sixxxx` and `config` are each six
        // letters against `Widget`'s six — so the path order it fell through to put `aaa` first.
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
        // `file:` squashes to the four letters `file` — the colon falls out with the separators
        // — so a scheme left on the path puts **every** document in a directory named after
        // `File`, the second tier's directory test answers `true` everywhere, and what is left
        // to decide is depth. It swapped lobsters' two places for `class File` when it shipped
        // for ten minutes. The corpora found it; this keeps it found.
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
        // Containment alone lets a long file name swallow a short constant. discourse writes
        // `Jobs` in 372 files, and `app/jobs/onceoff/remove_old_auto_close_jobs.rb` held first
        // place because its stem *contains* `jobs` — which outranked every file that merely sits
        // in `app/jobs/`, `base.rb` among them. The name now has to be at least half of the
        // stem, which sends this one back to the second tier where its path is judged.
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
        // The other side of the same rule, and the reason it is a proportion rather than a
        // prefix: `ruby-progressbar.rb` **is** the file `ProgressBar` is declared in, and a
        // prefix test would have thrown it back to its path alongside every other file in the
        // gem. The name is eleven of the stem's fifteen characters, so it stays.
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
        // Ruby reopens freely and a monorepo's specs are most of the files that ever touch a
        // namespace: solidus offers 539 places for `Spree` and 76 of them are under `spec/`.
        // The reader is in application code, which loads none of them. **The card has to agree**
        // — a count taken without the fence is the second arithmetic item 13 closed, arriving
        // through a different door.
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
        // The same sentence as the test below, reaching the rung that did not have it. `places`
        // was handed the load paths and the name rung was not, so the two surfaces disagreed
        // about one file: a method whose only definition is rack-test's
        // `lib/rack/test/utils.rb` answered **null** here, and the same file one directory up
        // answered. 128 gem files across the six corpora carry such a segment and appear in
        // real `definition` answers. Both halves travel as one `environment::Fence` now, so a
        // surface cannot be written with the cursor gate and without the load paths.
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
        // pundit ships `class ApplicationPolicy` in the tree `rails generate` copies out of,
        // and every project that installed it wrote the same class into `app/policies/`. Both
        // are real declarations and only one of them runs. The load-path clause cannot settle
        // it — the template is under the gem's own `lib/`, so `require` could name it and
        // nothing ever does — which is why the tag reads what the tree is *for*.
        //
        // Measured over 5,859 drawn cursors: 67 positions on chatwoot and forem answered this
        // constant with two places, and the second one was always this file.
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
        // **The cursor the 90 were actually counted at.** `vendor/rbs/stdlib/minitest/0/kernel.rbs`
        // declares `private def describe` on `Kernel`, which `Object` includes and every class
        // object therefore inherits — so the ancestor walk reached it from `RSpec`, and the first
        // line of nearly every spec file in all six corpora got a **Resolved** card naming
        // minitest's signature. 88 of the 90.
        //
        // The signature is written out here rather than taken from the binary because the
        // vendored copy is extracted at run time and no unit test indexes it; the declaration is
        // minitest's own, copied, and the ancestry around it is `TYPED_RBS`'s.
        //
        // **No card is the correct answer and not a degraded one.** rspec-core defines
        // `RSpec.describe` dynamically — there is no `def describe` anywhere in the gem — so
        // there is no better place to name, and the fall-through has nothing public to find.
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(
            signatures.join("core/core.rbs"),
            "module Kernel\n  \
             private def describe: (untyped desc) { (?) -> untyped } -> untyped\n\
             end\n\n\
             class Object\n  include Kernel\nend\n",
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
        // **The defect this gate exists for, in the shape the head-to-head found it in.** A
        // top-level `private def` is filed on `Object` by rubydex, which is an ancestor of every
        // class object in the workspace, so the ancestor walk finds it for any constant written
        // as a receiver — and returns it as **precise**, which is the tier that claims the code
        // names the type. Ruby raises `NoMethodError: private method called` at that call.
        //
        // The real one is `RSpec.describe` at the first line of nearly every spec file: the walk
        // reaches minitest's `private Kernel#describe` out of the vendored signatures, and the
        // card printed the word *private* itself. 88 of the 90 `absent-resolved` rows over six
        // corpora were that one cursor.
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
        // The other half of clause 1, and the one that decides whether this gate is a fix or a
        // regression. Ruby permits a private call with **no receiver written**, and with one
        // spelled `self` — through `.` and `::` alike, since 2.7. All three are the same cursor
        // to rubydex, whose `MethodRef` records `Some(name)` for the receiver in every one of
        // them, which is why the question is asked of the syntax instead.
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
        // Upstream's looseness, asked of the jump rather than of the list. rubydex's
        // own visibility check passes a private method whenever the caller's `self` is the same
        // **class as** the receiver; Ruby's exemption is for the receiver being *written* `self`,
        // and `Vault.new.secret` raises from inside `Vault`. `completion::reachable` was the only
        // surface applying the stricter rule until now.
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

    /// The concern discourse writes, trimmed to the three things that make the shape: a block
    /// holding a bare `private`, a `def` inside it below that `private`, and a `def` in the
    /// module body below the whole block.
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

    #[test]
    fn a_public_method_below_a_block_holding_private_answers_at_a_written_receiver() {
        // **The defect the gate introduced, in the shape the head-to-head found it in.** A bare
        // `private` is a statement, and rubydex applies it to the body it is written in until
        // that body ends — a block is not a body to it. So `class_methods do … private … end`
        // sets the *module's* default visibility, and every `def` below the block is recorded
        // private. `HasCustomFields#upsert_custom_fields` is discourse's own, called on explicit
        // receivers in shipped code, and the gate refused all of them.
        //
        // `class_methods` is `module_eval` and this file knows no Rails word: what makes the
        // repair right is that a block has a body, not which gem opened this one. See
        // `Modifiers`.
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
            !markdown.contains("Matched on the method name"),
            "and resolved rather than guessed, because the refusal never happened: {markdown}"
        );
        // **The card prints the word too, off the same wrong record.** A jump that lands on a
        // card reading `private HasCustomFields#upsert_custom_fields` has moved the false
        // sentence rather than removed it.
        assert!(
            !markdown.contains("private"),
            "and the card does not call a public method private: {markdown}"
        );
    }

    #[test]
    fn a_private_inside_a_block_still_applies_inside_that_block() {
        // Clause 6, first direction. The repair narrows the modifier to the body it is written
        // in — it does not throw it away, and a `def` *under* that `private` and *inside* the
        // same block is private in every reading of Ruby there is.
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
        // Clause 6, second direction, and the one that says the repair is a narrowing rather
        // than a deletion. Nothing about the ordinary shape changes: a bare `private` written in
        // a class body governs every `def` after it, block or no block.
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
        // The one way the repair could have over-reached. It overturns rubydex's record only
        // where a bare modifier is the whole reason for it, so the walk has to know the *named*
        // forms exist — `private def name`, `private :name` — even though it never interprets
        // one: a `def` either of those picks out is private for a reason no block can have
        // leaked, and this file holds both a leaking block and such a `def`.
        //
        // The inline form is the one that needs the walk's own guard. `private :name` writes a
        // second definition into the graph at the call rather than at the `def`, and a
        // declaration is confirmed private by any definition that did not escape — so that
        // spelling is already refused by the line above the guard.
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
        // The string spelling of the same thing, which the walk has to recognise for the same
        // reason the symbol one does — a name is a name however it is quoted.
        let targets = harness.definition_at(&uri, source, "quiet_value\n");
        assert!(
            targets.as_array().is_none_or(Vec::is_empty),
            "`private \"quiet_value\"` names this one: {targets}"
        );
    }

    #[test]
    fn a_constructor_is_not_refused_by_the_privacy_gate() {
        // **The exemption, and the one way this change could have broken every workspace at
        // once.** `Foo.new` resolves to `Foo#initialize`, which Ruby privatises by name at the
        // point of definition — so a gate that read visibility off the redirect would refuse a
        // constructor at a receiver that is written and is never `self`. `holds_private` skips a
        // redirected resolution for exactly this.
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
    fn a_private_method_on_a_derived_receiver_is_refused_and_the_footnote_says_why() {
        // **Upstream's looseness, at the second of the three rungs.** `Vault.new`
        // is not a constant, so rubydex records no receiver and the resolved rung never runs;
        // `types::method_receiver` types it from the constructor and the member lookup then
        // finds a `def` Ruby refuses. That card was *Derived*, which is a weaker claim than
        // *Resolved* and still a claim that the call can be made.
        //
        // `Other#secret_value` is here so the name rung has something public left to answer
        // with, which is what makes the footnote observable at all: with only the private `def`
        // in the workspace the list is empty and there is no card to read.
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
            markdown.contains("which keeps it private"),
            "and the footnote says what happened rather than `has no such method`, which would \
             be false of a class that has it: {markdown}"
        );
        assert!(
            !markdown.contains("has no such method"),
            "the two sentences are different facts and only one of them is true here: {markdown}"
        );
    }

    #[test]
    fn a_refused_resolved_member_does_not_say_the_receiver_has_no_type() {
        // **The defect the audit found in this gate the moment it landed**, at 31 cursors over
        // six corpora. The resolved rung refuses a private hit and falls through to the name
        // rung, which knows nothing about the receiver — so the card said *the receiver's type
        // is unknown* of a type rubydex had just handed over, which is the same false sentence
        // the gate exists to remove, reintroduced one tier down. `audit` check 6 is written for
        // exactly that sentence: a card claiming no type, beside a completion list built from a
        // class.
        //
        // `Other#describe_thing` is here so the name rung has something public left to answer
        // with, which is what makes the footnote observable — with only the private `def` in the
        // workspace the list is empty and there is no card at all.
        let mut harness = Harness::new();
        harness.write(
            "app/lib/support.rb",
            "private def describe_thing(name)\nend\n",
        );
        harness.write(
            "app/lib/other.rb",
            "class Other\n  def describe_thing(name)\n  end\nend\n",
        );
        harness.write("app/models/widgets.rb", "module Widgets\nend\n");
        let source = "class Store\n  def go\n    Widgets.describe_thing(\"x\")\n  end\nend\n";
        let caller = harness.write("app/models/store.rb", source);
        harness.index();

        let markdown =
            harness.hover_at(&caller, source, "describe_thing(\"x\")")["contents"]["value"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
        assert!(
            markdown.contains("the class object `Widgets`"),
            "the receiver was resolved and the footnote has to say so: {markdown}"
        );
        assert!(
            markdown.contains("which keeps it private"),
            "and say what it did with the member: {markdown}"
        );
        assert!(
            !markdown.contains("the receiver's type is unknown"),
            "which is the sentence `audit` check 6 counts, and it is false here: {markdown}"
        );
    }

    #[test]
    fn a_private_method_is_still_found_by_references_and_by_rename() {
        // `resolve` passes `Privacy::Allowed`, and this is the assertion that says why that is a
        // decision rather than an omission. Those callers answer *where is this used*, where a
        // use of a private method is a use — and they are handed no text, so the one question
        // the gate answers cannot be asked of them at all.
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

    #[test]
    fn a_top_level_def_in_a_generator_template_does_not_answer_the_root_rung() {
        // The tag's worst case, and why the root rung reads it even though it refuses the
        // layout. fabrication ships
        // `lib/rails/generators/fabrication/cucumber_steps/templates/fabrication_steps.rb`,
        // whose top-level `def with_ivars` rubydex files on `Object` — a member of every
        // receiver in three of the six corpora. `resolve_call`'s root arm returns that as
        // **precise**, so the card says *Resolved* and names a file nobody loads.
        //
        // The same shape reached 12 discourse cursors through active_model_serializers' own
        // `.id` template, where the fall-through does better than the fence needs it to: eight
        // of them now answer the model's real column out of `db/structure.sql`.
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
        // The tag is four directory names read off a path, and it was written against *the
        // project's* trees. A gem that ships `lib/rack/test/` is publishing a library —
        // `railties` puts `rails/commands/test/` there too — and dropping those would be
        // deleting the answer. What `require` can name is what the application loads, whichever
        // word the directory uses, and `load` is the list that knows. Without this the fence
        // took 81 lists off lobsters, a corpus with no project test tree in any place list.
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
        // The safety clause, and the same one the signature rule above has: dropped wherever a
        // loadable place survives beside it, kept where none does. A declaration written only
        // under `spec/` is still better answered than not — over 4,501 constant cursors outside
        // the test trees, 414 carried a spec place and **0** were emptied by this.
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
        // The gate is the cursor's, exactly as it is for the name rung and for `resolve_call`'s
        // root arm: a developer working in a spec is who the spec's copy is the answer for, and
        // a fence that fired there would take away the place they are most likely to want.
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
        // The rule is a namespace's, because a file is conventionally named after the class in
        // it. A method's file is named after its class too, so ranking a method's places this
        // way would reorder answers on nothing — `spin.rb` is not a better place to look for
        // `Shop.spin` than the file that happens to sort first.
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
        // The one rung the library exemption deliberately does **not** reach, and the corpora
        // are the argument. `rbs` ships `lib/rbs/test/setup.rb` — a script `require` really can
        // name, holding a real top-level `def match` — so the exemption made it loadable, the
        // root arm answered it as **precise**, and `match` in discourse's `config/routes.rb`
        // went from a 106-candidate *Guessed* list holding the right answer to a one-place
        // *Resolved* card pointing at an RBS test harness. Five of 6,836 drawn call cursors
        // did that. A hit on a root is a hit on every receiver in the workspace, so this rung
        // takes the crudest reading of the path there is and the list rungs take the better
        // one.
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
        // The half that matters is the tier. A candidate list says *matched on the method name
        // alone*, and the reader can see the gem's script for what it is; a one-place
        // **Resolved** card pointing at it cannot be seen through at all.
        let markdown = harness.hover_at(&post, source, "calibrate(")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(markdown.contains("Tuner#calibrate"), "{markdown}");
        assert!(
            markdown.contains("Matched on the method name"),
            "{markdown}"
        );
    }

    #[test]
    fn a_root_answer_only_the_suite_declares_falls_through_to_the_name_rung() {
        // `Object` is an ancestor of every receiver, so a top-level `def` in a spec — which is
        // where rubydex files a `def` written inside an `RSpec.describe` block as well —
        // answers for the whole workspace, and `resolve_call`'s root arm returns it as
        // **precise**. That is the one answer `loadable_from` never sees, because
        // `resolve_typed` fences only an imprecise one. Measured over six corpora at 800 bare
        // calls each: 31 of 4,251 answered cursors landed this way and every one was wrong.
        //
        // The fall-through is the point of the test. Silence would also beat the wrong jump,
        // but the name rung holds the method the reader actually meant.
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
        // And the card says it is a guess, which is the honest half of the trade: a *Resolved*
        // card naming the wrong method is worse than a *Guessed* one naming the right one.
        let markdown = harness.hover_at(&post, source, "cook(")["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(markdown.contains("Cooker#cook"), "{markdown}");
        assert!(!markdown.contains("Object#cook"), "{markdown}");
    }

    #[test]
    fn a_top_level_def_in_a_migration_is_not_a_member_of_every_receiver() {
        // The root rung, one tree on from the spec. rubydex has no notion of a script, so a
        // `def` written at the top level of a migration — the helper people put above
        // `def change` — lands on `Object` and answers for every receiver in the workspace,
        // *precisely*. The root rung reads the directory name and nothing else, deliberately:
        // a fence loosened on the one rung where being wrong is worst is loosened backwards.
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
    fn a_cursor_in_a_testing_support_tree_keeps_the_answer_the_suite_declares() {
        // The cursor gate is wider than the target tag on purpose. solidus publishes 119 Ruby
        // files under a `testing_support` segment that is in no test tree — shared examples and
        // factories under `core/lib/spree/testing_support/`, shipped for other people's suites —
        // and a developer reading one of those is exactly who a spec's `def` is the answer for.
        // Without this the fence fires at nine of the corpora's cursors whose own rule says it
        // should not.
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
        // An `.rbs` says what a method takes and hands back; it is not where anybody wrote it,
        // and a reader sent there lands in a stub with no body. So it stands aside wherever
        // source survives beside it — and stands where none does, because a signature is a
        // better answer than silence. Both halves are needed: over the five corpora 661
        // positions have both and 216 have only the signature.
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
        // Two files at the same require-relative path under two load paths are one file as far
        // as `require "thing"` is concerned: `lib/` is searched first, so `app/`'s copy is one
        // this project can never load. The shape a real bundle has it in is a pinned `cgi`
        // against the copy inside Ruby — 350 positions over the five corpora — and that needs a
        // Ruby installation to write down. This is the same rule against the same list, on the
        // two load paths the default configuration already names.
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
        // The other half of the same rule, and the reason it is keyed on the load path rather
        // than on a file name: a project's `db/` and `script/` are indexed and neither is a
        // load path, so nothing about two files called `thing.rb` there makes either of them a
        // copy of the other. Both are real places and the card counts both.
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

        let markdown = harness.hover_at(&caller, source, "Thing")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Defined in 2 places"), "{markdown}");
    }

    #[test]
    fn a_document_the_editor_cannot_open_is_not_a_place() {
        // rubydex declares Ruby's own object model itself, under `rubydex:built-in`, and
        // `DocUri::from_uri_str` refuses that URI for every request alike — so a jump has
        // always dropped it and only the count was carrying it. One position in the five
        // corpora read `Defined in 18 places` over a jump offering 17, and it is the last of
        // the two numbers' disagreements that is this server's to fix.
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
        // rubydex keys a declaration's *members* by the parenthesised `shout()` but records a
        // call as the bare `shout`, so every method lookup goes through here. An empty graph
        // holds neither spelling, which is the case that has to answer `None` rather than
        // fabricate the parenthesised form out of an interned string that is not there.
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
        // Not the whole method body: the name, which is where the cursor should land.
        assert_eq!(targets[0]["targetSelectionRange"]["start"]["line"], 9);
        assert_eq!(targets.as_array().unwrap().len(), 1, "{targets}");
    }

    #[test]
    fn a_cursor_on_a_name_is_not_the_operator_one_byte_in_front_of_it() {
        // `!shout` records two calls over adjoining bytes: `!` over the bang and `shout` over
        // the name. `covers` is end-inclusive, so with the cursor on the `s` both spans reach
        // it, and width alone picks the bang — one byte against five. The answer used to be
        // every `#!` in the graph, at a cursor the user put on a method they can see.
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
        // The other half of the same rule, and the reason `covers` is end-inclusive at all: a
        // span that only *ends* at the cursor stays a candidate, it merely loses to one that
        // *begins* there. Nothing begins at this cursor, so `build` wins — and that is also
        // what keeps `a.b += c` answering, where rubydex records the call over the `.` and
        // there is no span over the message at all.
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
        // Regression: rubydex records a *second*, invented constant reference over the same
        // bytes as `Person` in `Person.build`, pointing at the singleton class, so that the
        // call can be resolved. Following it would jump to the wrong thing — or, for a call
        // with an implicit receiver, to a `class << self` block on the other side of the file.
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
        // to read. Both goto-definition and hover redirect to the constructor, and hover is the
        // half that matters most: it is where the parameter list comes from.
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
            !markdown.contains("receiver's type is unknown"),
            "a redirect is still exact: {markdown}"
        );

        // A class that writes its own `new` is reached by that method, and `initialize` is one
        // `super` further on. Redirecting here would skip the code the call actually runs.
        let markdown = harness.hover_at(&caller, source, "new\n")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Registry.new(*args)"), "{markdown}");
    }

    #[test]
    fn a_class_with_no_constructor_keeps_the_honest_answer() {
        // Every object inherits `BasicObject#initialize`, so with rbs indexed there is always
        // *an* `initialize` to redirect to — and for a class that defines none it is as useless
        // as `Class#new` and less true. The guard is what keeps the redirect meaning something.
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

        // And the guard has not simply turned the redirect off for everyone.
        let markdown = harness.hover_at(&caller, source, "new(1)")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Money#initialize(cents)"), "{markdown}");
    }

    #[test]
    fn a_call_on_a_local_is_answered_as_a_guess() {
        // Without type inference `thing.shout` can only be matched by name. The answer is
        // still worth giving — it is right most of the time — but it must not be dressed up
        // as certainty.
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
        assert!(
            markdown.contains("receiver's type is unknown"),
            "{markdown}"
        );
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
        assert!(markdown.contains("Alpha#ping"), "{markdown}");
        assert!(markdown.contains("Beta#ping"), "{markdown}");
    }

    /// A class whose body writes blocks, which is the shape every Ruby DSL has — Parslet's
    /// `rule(:colon) { str(':') }`, Rails' `scope :recent, -> { … }`, a concern's `included do`.
    ///
    /// `Base` owns `solo` as an ordinary instance method and an unrelated module owns the same
    /// name, which is the pair the name rung gets backwards: it drops the class's and keeps the
    /// module's, because a class object provably cannot reach the first.
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
        // rubydex has no notion of a block, so a bare call inside one in a class body is
        // recorded with the receiver a statement of that body gets — the singleton class. That
        // is right for a block nobody re-binds and wrong for every block a DSL takes, and the
        // class object has no `only_instance`, so the answer used to fall to the name rung.
        let (mut harness, caller) = closures();

        let card = closure_card(&mut harness, &caller, "only_instance }");
        assert!(card.contains("Helpers#only_instance"), "{card}");
        assert!(card.contains("Found on an instance of `Thing`"), "{card}");
        // The rung it replaces is the one rung allowed to be wrong, and the card has to stop
        // saying so.
        assert!(!card.contains("Matched on the method name"), "{card}");

        // And the jump is the same answer: the two requests go through one function so that
        // they cannot disagree, and this is the case where a card could have been improved
        // without the navigation following it.
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
        // The singleton side is searched first and this rung only ever runs after it found
        // nothing, so a name on both keeps the lexical answer and keeps it *resolved*. There is
        // nothing to derive: the file as written would run.
        let (mut harness, caller) = closures();

        let card = closure_card(&mut harness, &caller, "on_both }");
        assert!(card.contains("ClassHelpers#on_both"), "{card}");
        assert!(!card.contains("Found on an instance"), "{card}");

        // The control beside it: a name only the class object has, which never needed this rung.
        let card = closure_card(&mut harness, &caller, "only_class }");
        assert!(card.contains("ClassHelpers#only_class"), "{card}");
        assert!(!card.contains("Found on an instance"), "{card}");
    }

    #[test]
    fn a_closure_reaches_a_method_its_superclass_owns() {
        // The half that was a *wrong* answer rather than a vague one.
        // `reachable_on_a_class_object` drops every candidate a `class` owns — correct about a
        // class object, and applied here to a cursor whose `self` is not one — so the name rung
        // threw away `Base#solo` and answered an unrelated module's method of the same name.
        let (mut harness, caller) = closures();

        let card = closure_card(&mut harness, &caller, "solo }");
        assert!(card.contains("Base#solo"), "{card}");
        assert!(!card.contains("Unrelated#solo"), "{card}");
        assert!(card.contains("Found on an instance of `Thing`"), "{card}");
    }

    #[test]
    fn a_block_in_a_module_body_keeps_the_guess_it_had() {
        // A module has no instances, so the sentence this rung would print is not true of one.
        // The blocks a module body holds are `included do` and `class_methods do`, and what
        // Rails runs those against is the **including class** — not the module, and not
        // anything reachable from it. The name rung keeps the same declaration; what it does
        // not do is call it derived.
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
        assert!(card.contains("Matched on the method name"), "{card}");
        assert!(!card.contains("Found on an instance"), "{card}");
    }

    #[test]
    fn a_statement_in_a_class_body_is_not_a_closure_and_a_block_in_a_def_is_not_either() {
        // Both of these have a `self` nothing can re-bind: a statement of the body runs with
        // the class object, and a block written inside a `def` closes over the method's `self`.
        // A name only an instance has is unreachable from both, and the honest answer is the
        // guess it always was.
        let (mut harness, caller) = closures();

        for needle in ["only_instance\n", "only_instance if true"] {
            let card = closure_card(&mut harness, &caller, needle);
            assert!(card.contains("Helpers#only_instance"), "{needle}: {card}");
            assert!(
                card.contains("Matched on the method name"),
                "{needle}: {card}"
            );
            assert!(!card.contains("Found on an instance"), "{needle}: {card}");
        }
    }

    #[test]
    fn a_call_on_a_constant_that_was_never_declared_still_answers() {
        // `Nowhere` is a receiver rubydex can name and cannot resolve — the normal state of a
        // file mid-refactor, and of every constant a gem defines when the gem is not indexed.
        // The precise path has to stand down rather than resolve against a missing owner.
        let mut harness = Harness::new();
        let person = harness.write(
            "app/person.rb",
            "class Person\n  def frobnicate\n  end\nend\n",
        );
        let source = "Nowhere.frobnicate\n";
        let main = harness.write("app/main.rb", source);
        harness.index();

        // Name-based, so the one declaration spelled this way is still the answer — and *which*
        // declaration is the assertion. "not null" would pass just as happily on a jump to the
        // wrong file, which is the failure this degradation actually risks.
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
        // `def Functions::floor` rather than `def self.floor` — how rexml writes its whole XPath
        // surface, and the shape behind 85 of the 126 `def` lines that answered nothing across
        // the six corpora. rubydex attributes a named receiver through the arm meant for a plain
        // `def`, which looks the name up as an instance member and misses.
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
        // The same miss, where the namespace happens to declare an instance method spelled the
        // same. Then the lookup does not miss — it lands on another method entirely, and the
        // card says so with no hedge. This is why the named receiver is answered outright
        // rather than after rubydex has been asked and has said something wrong.
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
        // Ruby ships `Gem::URI = Bundler::URI`, and the vendored copy beside it writes
        // `module Gem::URI` — 7 of the 16 `def` lines still answering nothing over the six
        // corpora, and all 19 `def self.` in that one file. Every member a body declares is
        // attributed by the *name* of that body, and this name has a declaration: the alias,
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
        // `self.alias_method` is the second receiver a `MethodAlias` can carry, and upstream
        // pairs it with the *instance* member, which is what `Module#alias_method` does.
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

        // Every member the body declares by being written inside it — the kinds whose owner is
        // the *name* of the body rather than a name of their own.
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

        // A `def` inside a `def` is what makes the walk a *walk*: the nesting it records is the
        // outer method, which names nothing, so the name comes from two steps out.
        assert!(card(&mut harness, &unit, common, "inner").contains("Bundler::URI#inner"));

        // And the two that were never wrong, which are the evidence that the reverse map alone
        // is: a nested namespace and a constant carry a name of their own and resolve already.
        assert!(
            card(&mut harness, &unit, common, "Schemes").contains("module Bundler::URI::Schemes")
        );
        assert!(card(&mut harness, &unit, common, "ELSEWHERE").contains("Bundler::URI::ELSEWHERE"));
    }

    #[test]
    fn a_named_receiver_that_is_an_alias_declares_on_the_class_the_alias_names() {
        // Ruby 4 ships `Ripper = Prism::Translation::Ripper` in `prism/translation/ripper/
        // shim.rb`, and `ripper/core.rb` writes `def Ripper.parse` — the last 2 of the 16, and
        // the reason the named receiver needs the retry as well: its own lookup asks the name
        // for a namespace and the name has an alias. It is tried first and this only runs
        // where it found nothing, which is why `Thing = 1` below still answers nothing.
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
        // Ruby lets `Fore = Aft` stand beside `Aft = Fore`, and neither ever becomes a
        // namespace. The walk is capped so that the cursor on a `def self.` under one of them
        // costs a fixed number of lookups and then gives up.
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
        // `def Thing.bar` on a constant that holds a number. There is no singleton class to ask,
        // and the enclosing namespace's `bar` — if one existed — is not what was written. The
        // only honest answer is none, and the one thing that must not happen is a jump.
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
        // `Missing::Thing` makes the resolver record a `Missing` it never saw defined. It is a
        // placeholder with no definitions behind it, so listing it in the picker would offer a
        // destination that does not exist.
        let mut harness = Harness::new();
        let uri = harness.write("app/main.rb", "Missing::Thing.new\n");
        harness.index();

        // Nor somewhere to hover. The cursor is on a real reference, so there is something to
        // resolve — but what it resolves to is a name the resolver wrote down and nothing else,
        // and a card saying `Missing` over a constant spelled `Missing` tells a reader only
        // that the server has no idea either. Silence says the same thing and takes no space.
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
        // `index.include` is `**/*.rb`, so a Ruby-looking buffer under another extension has
        // text on disk and no document in the graph. Every request has to survive that: the
        // text is readable, so nothing short of the graph lookup can tell them apart.
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
        // The other half of `a_half_typed_def_does_not_take_the_outline_down_with_it`. The
        // outline drops a nameless `def` before it ever builds a range; goto-definition does
        // not, because looking a name up per definition would cost every hover in the file.
        // So `locator::spans` is the one place the containment rule is enforced, and this is
        // the request that reaches it.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();

        // Asked of `spans` directly: containment is a property of every pair it hands out, and
        // the requests that carry one filter half-typed definitions out before they get there.
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
                    // The shape VS Code threw on: Prism recovered `def` into a node spanning
                    // the three keyword bytes, with its name span in the whitespace after them.
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
        // rubydex records a method call under its bare name — except an `alias`, which it
        // records with the parentheses already on. Both have to arrive at the same member key
        // or the lookup silently misses and `yell` navigates nowhere.
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

        // And from inside the `alias` statement itself, which is the reference rubydex records
        // with the parentheses already on.
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
        // `LocationLink::targetSelectionRange` carries the identical containment rule, from the
        // identical pair of spans. VS Code does not validate this one, so the symptom is quieter
        // — goto-definition parks the cursor on a newline outside the construct it claims — but
        // it is the same defect and it is fixed in the same place.
        let mut harness = Harness::new();
        let uri = harness.write("lib/a.rb", "");
        harness.index();
        let source = "class A\n def\n";
        harness.open(&uri, source);

        // The whitespace after the keyword, which is where the recovered name span sits.
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
    fn a_hover_on_an_untyped_receiver_caps_the_list_of_candidates() {
        // With no inference there is nothing to narrow `thing.call` down to. Listing all of
        // them would be a page; the count is what tells the user the answer is a guess.
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
        assert!(
            markdown.contains("**12 possible definitions**"),
            "{markdown}"
        );
        assert!(markdown.contains("…and 2 more"), "{markdown}");
    }

    /// A project whose only `def shout` is in its spec tree, and two cursors on `x.shout` —
    /// one in the application, one in a spec.
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
        // The one fence on the name rung. `x` types to nothing, so `shout` reaches the
        // name-matched list, and the only `def shout` in this project is one RSpec loads and
        // the application never does. A jump from a model into a spec is not an answer a
        // developer can act on, and the honest thing to hand back is nothing.
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

        // …and the same guess from inside the spec tree keeps it: a developer editing a spec is
        // exactly who the `def` in the next spec file over is the answer for.
        let definition = harness.definition_at(&from_spec, source, "shout");
        assert!(
            definition[0]["targetUri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("spec/support/loud.rb")),
            "{definition}"
        );
    }

    /// A project whose only `def rollout` is inside a migration, and two cursors on `x.rollout`.
    ///
    /// The shape the corpora ship: a class declared at the top of a migration so a data script
    /// can run against the schema of the day it was written. `db/migrate` is on no autoload
    /// path, so nothing outside that one file can reach it.
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
        // The same fence as the spec tree, one tree further on. `x` types to nothing, so
        // `rollout` reaches the name-matched list and the only `def rollout` in this project is
        // one the migration task loads, by path, in a process of its own. Measured on mastodon
        // before this clause: 19 of 1,074 drawn cursors carried a place inside a migration and
        // two of them had one **first**, which is where the jump lands.
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

        // …and a reader inside a migration keeps it, for the reason a reader inside a spec
        // does: the copy at the top of one of these files is exactly what they are reading.
        let definition = harness.definition_at(&from_migration, source, "rollout");
        assert!(
            definition[0]["targetUri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("20180528141303_backfill_stories.rb")),
            "{definition}"
        );
    }

    #[test]
    fn a_definition_whose_file_is_gone_answers_nothing_rather_than_a_dead_link() {
        // The graph still holds the declaration and still knows which file it was written in.
        // Turning that into a `LocationLink` needs the text, to place two ranges in it, and a
        // file deleted since indexing has none — the same situation
        // `diagnostics_are_skipped_for_a_file_that_has_gone_from_disk` covers from the other
        // end. Every site failing leaves an empty list, which must be answered as `null`: a
        // client handed `[]` opens an empty peek window instead of saying nothing was found.
        let mut harness = Harness::new();
        let person = harness.write("app/person.rb", "class Person\nend\n");
        let source = "Person.new\n";
        let main = harness.write("app/main.rb", source);
        harness.index();
        assert!(
            !harness.definition_at(&main, source, "Person").is_null(),
            "the jump works while the file is there"
        );

        std::fs::remove_file(person.to_path().expect("a path")).unwrap();

        assert!(
            harness.definition_at(&main, source, "Person").is_null(),
            "and answers nothing once it is not"
        );
    }

    /// Every spelling of an instance variable [`scopes`] handles, and a second `self` holding
    /// one of the same name.
    ///
    /// Prism reduces the six to six nodes and no fewer — a write, an `||=`, an `&&=`, an
    /// operator write, a destructuring target and a read — and each of them is a shape
    /// `definition` has to find the others from. The `def self.reset` at the bottom is the one
    /// that must *not* be found: `@total` there hangs off the class object.
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
        // declaration and files no reference to one, so `definition` at a *read* had nothing to
        // look up and answered nothing at all — at a cursor `documentHighlight` was lighting
        // the whole file for.
        //
        // Pinned as one picture per spelling, and pinned *together*, for `GALLERY`'s reason.
        // `w`/`r` is what the highlight lit and an uppercase cell is a definition target
        // landing on it, so the property under test — every jump goes to a span the highlight
        // already lit — is the absence of a lowercase `d` anywhere on the page.
        let mut harness = Harness::new();
        let uri = harness.write("lib/counter.rb", IVARS);
        harness.index();

        // Every cursor is on the *read* in `report` — the shape that used to answer nothing —
        // and each needle is the shortest run of the line that names one position.
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
        // `@total` in `def self.reset` and `@total` in `def initialize` are two variables, and
        // the whole of what keeps them apart is that this answer comes from the same
        // `SelfContext` algebra `documentHighlight` and `rename` already read. The `-1` is in
        // the fixture so that this picture cannot be confused with the other `@total = 0`.
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
        // The scope walk speaks for two kinds of variable and this answer is only about one of
        // them. `person = Person.new` is a line the reader can see from where they are
        // standing, so nothing here jumps to it and nothing here types it — which leaves a
        // local as the one cursor in this file where `documentHighlight` answers and
        // `definition` does not. Pinned because it is a gap rather than a decision that needs
        // no revisiting: the lowercase `w` is what it looks like, and it is the same shape the
        // audit counts.
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
    /// `belongs_to`, `has_many`, `scope`, `delegate` and `enum` each *declare* the name they are
    /// given, and a generator in `workspace/rails/` has already written it down. `validates`
    /// names a column `db/schema.rb` declared. `validate` and `before_save` name a `def` further
    /// down the file and declare nothing. `validates :absent` names something nobody declares
    /// anywhere, which is the decline.
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

    /// A controller holding the callback shape that is a controller's and not a model's, and the
    /// one symbol in this application that names nothing at all.
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

    /// The application `MACROS` is written in: a schema, the two other models it names, and a
    /// controller holding the callback shape that is a controller's and not a model's.
    fn macros_project() -> (Harness, DocUri, DocUri) {
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(signatures.join("core/core.rbs"), TYPED_RBS).unwrap();
        // Enough of Ruby's own object model for the ancestor walk to run off the end of the
        // project into it, which is the case `ruby_s_own` exists for and which no fixture
        // without a core signature can reach.
        std::fs::write(
            signatures.join("core/kernel.rbs"),
            "module Kernel\n  def format: (String) -> String\nend\n",
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
        // The two answers a macro's symbol can have, drawn at the cursor. A macro that
        // *declares* a name — `belongs_to`, `scope`, `enum` — answers with the macro line
        // itself, because that is where `workspace/rails/` recorded the declaration it wrote;
        // one that only *names* an existing method answers with the `def`.
        //
        // Every definition target is an uppercase mark, which is lane 2's containment check
        // drawn: `definition` lands nowhere `documentHighlight` did not light. The two requests
        // read one resolution, so that is a property rather than a coincidence.
        let (mut harness, story, controller) = macros_project();

        // `belongs_to :user` declares `Story#user`, so the macro *is* the declaration's place
        // and the jump is to the line the cursor is already on. The card is what carries the
        // answer here; the agreement is what keeps the two requests from drifting apart.
        assert_eq!(
            harness.agreement_map(&story, &cursor_after(MACROS, "belongs_to :us")),
            "  belongs_to :user\n\
             \u{20}             WWWW"
        );
        // `scope :recent` is the same, one side of the object over: it declares a singleton
        // method, which is why the lookup asks the class object after the instance.
        assert_eq!(
            harness.agreement_map(&story, &cursor_after(MACROS, "scope :rec")),
            "  scope :recent, -> { order(created_at: :desc) }\n\
             \u{20}        WWWWWW"
        );
        // `before_save :normalise` declares nothing. The `def` is the target, the call in
        // `title_is_short`'s body is lit beside it, and the symbol itself is lit as the read it
        // is — none of which the graph has any record of.
        assert_eq!(
            harness.agreement_map(&story, &cursor_after(MACROS, "before_save :norm")),
            "  before_save :normalise\n\
             \u{20}              rrrrrrrrr\n\
             \u{20}   normalise\n\
             \u{20}   rrrrrrrrr\n\
             \u{20} def normalise\n\
             \u{20}     WWWWWWWWW"
        );
        // A controller's callback is the same rule, and the `def` it names is private — which
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
        // One card per rung, and the last line of each is the same: which macro is the evidence
        // that this symbol is a name at all. Nothing in `:user` says it is something to call.
        let (mut harness, story, _controller) = macros_project();

        let card = |harness: &mut Harness, needle: &str| {
            harness.hover_at(&story, MACROS, needle)["contents"]["value"]
                .as_str()
                .unwrap_or("null")
                .to_owned()
        };

        // A generator's own declaration, so the card is the one `Story.new.user` already got,
        // with one line more.
        let association = card(&mut harness, "user\n");
        assert!(association.contains("Story#user"), "{association}");
        assert!(association.contains("which is a `User`"), "{association}");
        assert!(
            association.ends_with(
                "*Named by `belongs_to` — the symbol is a method name because the macro says \
                 so, not because the code spells it as a call.*"
            ),
            "{association}"
        );

        // The column, which is the answer worth having: `validates :title` is a claim about
        // `db/schema.rb`, and the card names the file that made it true.
        let column = card(&mut harness, "title, presence");
        assert!(column.contains("Story#title"), "{column}");
        assert!(column.contains("db/schema.rb"), "{column}");
        assert!(column.contains("Named by `validates`"), "{column}");

        // A `def`, where the only thing derived is that the symbol names one.
        let method = card(&mut harness, "title_is_short\n");
        assert_eq!(
            method,
            "```ruby\nStory#title_is_short\n```\n\n*Named by `validate` — the symbol is a \
             method name because the macro says so, not because the code spells it as a call.*"
        );
    }

    #[test]
    fn a_symbol_no_ancestor_declares_is_not_answered_at_all() {
        // The decline, and it is the whole of the rung below this one. `:absent` is not a
        // column, not a `def` and not anything a generator wrote, so the only answer left would
        // be a name match against every `def absent` in the workspace — and a jump out of a
        // `validates` line into somebody else's method is a wrong answer the reader cannot see
        // is wrong. `skip_before_action :verify` is the same decline in a controller.
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
        // Three symbols that are *not* the macro's subject, and each is a different way of not
        // being one. `dependent: :destroy` configures the macro; `:desc` is inside the block
        // `scope` was handed; `:index` is inside an array inside a keyword argument. All three
        // would resolve to something if they were asked — `destroy` and `index` are real
        // methods — which is what makes the syntactic test the whole of the safety here.
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
    fn a_symbol_inside_a_method_body_is_not_a_macro_argument() {
        // The other half of the syntactic test: a macro is a call written into a class body, so
        // the same `send(:title_is_short)` one level down is a call and not a declaration. It
        // resolves to a real method and is still declined, because `send` is not evidence of
        // anything — `%i[a b].each { |name| send(name) }` is the shape it would have to be
        // right about.
        let (mut harness, story, _controller) = macros_project();

        assert_eq!(
            harness.agreement_map(&story, &cursor_after(MACROS, "send(:title_is_sh")),
            "null"
        );
    }

    #[test]
    fn a_symbol_that_only_ruby_declares_is_not_this_projects_answer() {
        // Every class inherits `Object` and `Kernel`, so an ancestor walk always terminates
        // somewhere — and `Kernel` alone declares `format`, `print`, `select`, `system`, `test`,
        // `open` and `sub`, every one of which is also an ordinary column name. Without the
        // filter, `validates :format` on a model with no `format` column is a **Resolved** jump
        // into `vendor/rbs`, which is the worst shape of wrong answer this server has: right
        // about the tier and about nothing else.
        //
        // Nothing real is lost to it. A member the class actually has is found on the class
        // before the walk reaches a root, which `MACROS`' own `validates :title` is.
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
        // And the class's own member still answers, from the same walk one step earlier.
        assert!(
            !harness
                .definition_at(&story, MACROS, "title, presence")
                .is_null()
        );
    }

    #[test]
    fn turning_the_guess_off_takes_nothing_away_from_a_macro_symbol() {
        // `[types] guess_from_names` exists to silence the one answer this server gives that is
        // allowed to be wrong. A symbol's answer is not that answer: the member was found in the
        // class's own ancestors, and the only thing derived is the convention that a macro's
        // argument names one — which the card states rather than hides. So the setting has
        // nothing here to switch off, and the plan's rung below this one was never built.
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

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
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
        assert!(card.contains("db/schema.rb"), "{card}");
        assert!(card.contains("Named by `validates`"), "{card}");
        assert!(!card.contains("Matched on the method name"), "{card}");
    }
}
