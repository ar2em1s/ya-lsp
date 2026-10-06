//! How ya-lsp spells Ruby constructs for humans.
//!
//! rubydex's names are built for lookup, not reading: a singleton method is
//! `Person::<Person>#build()`, and every method carries empty parentheses whether or not it takes
//! arguments. Editors show these strings directly, so they are translated back into what a Ruby
//! developer would write.
//!
//! Everything here is pure formatting, shared by `hover` and `documentSymbol` so the two never
//! disagree about what a construct is called.

use super::{cursor::ParameterSlot, synthesized::GENERATED_SCHEME, types};
use rubydex::model::{
    comment::Comment,
    declaration::{Ancestor, Declaration, Namespace},
    definitions::{Parameter, Signatures},
    graph::Graph,
    ids::DeclarationId,
};
use std::{borrow::Cow, sync::OnceLock};

/// A **type** as a reader sees it, which is not the same as a class name.
///
/// Three folds happen before a return reaches a margin or a card, and this is the one place they
/// are spelled, together with the four names Ruby readers write in lower case:
///
/// - **`nil` is a mark, not a member.** A method returning a `String` on one path and `nil` on
///   another is a `String?`. The mark is all optionality does here (completion, goto and the next
///   chain step read the class without it, per [`types::Typed`]), so drawing it is what keeps it
///   from being silent.
/// - **`true` and `false` together are `bool`.** Ruby has no such class, and `bool` is RBS's name
///   for the union: the word already written across every signature in the bundle, not one this
///   server invented.
/// - **Each of the three values with a Ruby literal is spelled as that literal**: `nil`, `true` and
///   `false`, wherever one stands alone. A method whose every exit is `nil` does answer `NilClass`,
///   and one that can only return `true` does answer `TrueClass`, but no Ruby reader calls them
///   that. `nil` is also the one case the mark cannot cover: there is nothing for it to sit on.
/// - **Lower case is all that separates these four from a class**, which a union needs most:
///   `true | String` shows at a glance which half came from a literal and which from a name, while
///   `TrueClass | String` reads as two classes a project declared.
///
/// `?` goes on the **head** of a union, not around the whole: `String? | Integer` and
/// `(String | Integer)?` are the same type, and the first needs no brackets in a margin with no
/// room for them.
///
/// **A union holding a class that every other member inherits from is spelled as that class**
///. stdlib's `URI.parse` returns ten classes, `URI::Generic` and nine of its
/// subclasses, and every value it answers is a `URI::Generic`. The label says that; the type
/// itself keeps every member, so a call reaches the members only a subclass has.
///
/// `None` where any class is not a class (a name resolving to something else, or one of the query
/// interface's two sentinels): the gate a margin has always applied, and why a raw rubydex key
/// never reaches a reader.
#[must_use]
pub fn typed(graph: &Graph, typed: &types::Typed) -> Option<String> {
    label(graph, typed, false)
}

/// A method's return as a label, marked `!` where the method's own body writes `raise` or `fail`
/// (`cursor::raises_in`).
///
/// **The mark sits where `?` does** (decided 2026-09-25), after it: `String!`, `String?!`,
/// `Article?! | Article:class`. It is the method's, not the value's, so only a return is spelled
/// with it: a variable holding what the method returned is spelled by [`typed`].
#[must_use]
pub fn returned(graph: &Graph, typed: &types::Typed, raises: bool) -> Option<String> {
    label(graph, typed, raises)
}

fn label(graph: &Graph, typed: &types::Typed, raises: bool) -> Option<String> {
    let shared = common_superclass(graph, typed.classes());
    let classes = shared.as_slice();
    let classes = if classes.is_empty() {
        typed.classes()
    } else {
        classes
    };
    let mut spelled = Vec::with_capacity(classes.len());
    for id in classes {
        let declaration = graph.declarations().get(id)?;
        let name = match declaration {
            Declaration::Namespace(Namespace::Class(_) | Namespace::Module(_)) => {
                qualified_name(graph, declaration.name())
            }
            Declaration::Namespace(Namespace::SingletonClass(_)) => {
                class_object(graph, declaration)?
            }
            _ => return None,
        };
        // A class declared inside `class << self` is filed under a singleton segment
        // (`Orchestrator::<Orchestrator>::Params`), a name neither Ruby nor RBS can write. A label
        // must be one a reader can type.
        if name.contains('<') {
            return None;
        }
        spelled.push(name);
    }
    // Wherever the name stands, not only at the head: `TrueClass` is the carrier the boolean fold
    // leaves, and a union keeps both halves as classes wherever it put them (`bool | String` is one
    // exit that answered a predicate beside one that answered a name). The pair is spelled `bool`
    // where `TrueClass` stands, and `FalseClass` then says nothing more. `NilClass` can only stand
    // alone, because the fold lifts it out of every union, and it goes through the same loop so
    // there is one place, not two.
    let pair = typed.boolean
        || (spelled.iter().any(|name| name == "TrueClass")
            && spelled.iter().any(|name| name == "FalseClass"));
    if pair {
        spelled.retain(|name| name != "FalseClass");
    }
    for name in &mut spelled {
        let word = match name.as_str() {
            "TrueClass" if pair => "bool",
            "TrueClass" => "true",
            "FalseClass" => "false",
            "NilClass" => "nil",
            _ => continue,
        };
        *name = word.to_owned();
    }
    let (head, rest) = spelled.split_first()?;
    let nil = if typed.nilable { "?" } else { "" };
    let raise = if raises { "!" } else { "" };
    // What a union's head holds is that member's, not the shared class's. A bound `Method` holds
    // the method it runs, which is what a reader wants of it: `Method[Widget#shout]`.
    let held = if shared.is_some() {
        String::new()
    } else if let Some(method) = typed
        .bound_method()
        .and_then(|method| graph.declarations().get(&method))
    {
        format!("[{}]", qualified_name(graph, method.name()))
    } else {
        held_by(graph, typed)
    };
    let mut label = format!("{head}{held}{nil}{raise}");
    for other in rest {
        label.push_str(" | ");
        label.push_str(other);
    }
    Some(label)
}

/// The member of a union every other member inherits from, where there is one.
///
/// Only a class, only a union, and only ancestry rubydex completed: a partial chain may hide the
/// link, and then the union is spelled whole. Never `Object` or `BasicObject`: every class inherits
/// them, so `String | Object` spelled `Object` would say nothing about the `String`.
fn common_superclass(graph: &Graph, classes: &[DeclarationId]) -> Option<DeclarationId> {
    if classes.len() < 2 {
        return None;
    }
    let inherits = |class: &DeclarationId, from: &DeclarationId| {
        graph
            .declarations()
            .get(class)
            .and_then(Declaration::as_namespace)
            .is_some_and(|namespace| {
                namespace
                    .ancestors()
                    .iter()
                    .any(|ancestor| matches!(ancestor, Ancestor::Complete(id) if id == from))
            })
    };
    classes.iter().copied().find(|candidate| {
        matches!(
            graph.declarations().get(candidate),
            Some(declaration @ Declaration::Namespace(Namespace::Class(_)))
                if !matches!(declaration.name(), "Object" | "BasicObject")
        ) && classes
            .iter()
            .all(|other| other == candidate || inherits(other, candidate))
    })
}

/// A **class object**'s type, spelled `Foo:class`: what `Foo` and `foo.class` are.
///
/// - **Why this spelling** (decided 2026-09-24): rubydex's name is `Foo::<Foo>` and RBS's is
///   `singleton(Foo)`. `Foo:class` reads as "the class `Foo` itself", beside `Foo` for one of its
///   instances.
/// - **A named class's own singleton only.** A module object, a singleton's singleton, and a class
///   nothing named answer `None`, as every name this module cannot spell does.
///
/// Spelled first, then taken apart (`navigation.md`): an anonymous class spells as `Class.new`, which
/// declares nothing, so it answers `None` here instead of a label for a class nobody named.
fn class_object(graph: &Graph, declaration: &Declaration) -> Option<String> {
    let spelled = spelled(graph, declaration.name());
    let attached = class_object_of(&spelled)?;
    let class = types::declared(graph, attached)?;
    matches!(
        graph.declarations().get(&class),
        Some(Declaration::Namespace(Namespace::Class(_)))
    )
    .then(|| format!("{attached}:class"))
}

/// What the head was written holding: `[String]` of an `Array[String]`, and `""` for the vast
/// majority of types, which are generic over nothing.
///
/// **Each position is spelled by [`typed`] itself**, one level down, which keeps one rule: the gate
/// that refuses to draw a head this server cannot name refuses the same at a position, and the same
/// four lower-case words come out. A refused position (resolved to nothing, or to a non-class) is
/// **`untyped`**, RBS's own word, never a blank that would make `Hash[untyped, String]` read as
/// `Hash[String]` and silently shift every later position.
///
/// **One level, which is all the table holds.** The `Typed` built here carries no arguments of its
/// own, so `Array[Array[String]]` draws `Array[Array]`; what the inner one holds is a question
/// `types::held_by_return` does not carry.
///
/// **A head whose every position is unnamed draws nothing**, because `Array[untyped]` tells a
/// reader strictly less than `Array` and costs them the width.
///
/// There is no union case to refuse: [`types::Typed`] only holds arguments beside **one** class,
/// because `holding` is applied where a single declaration resolved.
fn held_by(graph: &Graph, typed: &types::Typed) -> String {
    let arguments = typed.arguments();
    if arguments.iter().all(Option::is_none) {
        return String::new();
    }
    let spelled: Vec<String> = arguments
        .iter()
        .map(|held| {
            held.and_then(|id| {
                self::typed(graph, &types::Typed::of(id, types::Derivation::default()))
            })
            .unwrap_or_else(|| "untyped".to_owned())
        })
        .collect();
    format!("[{}]", spelled.join(", "))
}

/// Turn a rubydex declaration name into Ruby.
///
/// `Person::<Person>#build()` is rubydex for `Person.build`, because it models singleton methods as
/// members of a synthetic singleton class. Instance methods keep the `#` spelling, as Ruby
/// documentation does.
///
/// The graph is here for the other name rubydex invents: an anonymous `Class.new`, keyed by number,
/// which must be looked up before it can be spelled. See [`spelled`].
///
/// **A namespace a body of knowledge invented is spelled as it says** ([`shown_as`]):
/// `ActiveRecordRelation#where` is `ActiveRecord::Relation#where`, and a route helper, whose module
/// Rails never names, is `story_path` alone.
#[must_use]
pub fn qualified_name(graph: &Graph, name: &str) -> String {
    let name = &*spelled(graph, name);
    let Some((owner, method)) = name.rsplit_once('#') else {
        return shown_as(graph, name)
            .filter(|shown| !shown.is_empty())
            .map_or_else(|| name.to_owned(), str::to_owned);
    };
    let method = method.strip_suffix("()").unwrap_or(method);

    let (owner, separator) = match singleton_parts(owner) {
        // The path, not the last segment: an instance method of `Foo::Bar` is `Foo::Bar#baz`, so
        // its singleton method must be `Foo::Bar.baz`, not `Bar.baz`. Only a top-level class has no
        // path, and there `singleton` is the whole name.
        Some((prefix, singleton)) => (if prefix.is_empty() { singleton } else { prefix }, '.'),
        None => (owner, '#'),
    };
    match shown_as(graph, owner) {
        Some("") => method.to_owned(),
        Some(shown) => format!("{shown}{separator}{method}"),
        None => format!("{owner}{separator}{method}"),
    }
}

/// What a reader sees in place of `namespace`, where a body of knowledge invented it
/// ([`Knowledge::shown`](crate::knowledge::Knowledge::shown)): the class Ruby really builds, or `""`
/// where the object has no name and a member stands alone.
///
/// **Only where every definition of the name is generated.** A project that declares the name
/// itself meant something by it, and the generator then writes nothing there (the collision rule
/// in `workspace/rails/`), so its class is spelled as written.
fn shown_as(graph: &Graph, namespace: &str) -> Option<&'static str> {
    let (_, shown) = invented()
        .iter()
        .find(|(invented, _)| *invented == namespace)?;
    let definitions = graph
        .declarations()
        .get(&DeclarationId::from(namespace))?
        .definitions();
    let generated = definitions.iter().all(|id| {
        graph
            .definitions()
            .get(id)
            .and_then(|definition| graph.documents().get(definition.uri_id()))
            .is_some_and(|document| document.uri().starts_with(GENERATED_SCHEME))
    });
    (generated && !definitions.is_empty()).then_some(*shown)
}

/// Every name the build's bodies of knowledge invent, with what a reader sees for it.
///
/// A constant of the build, not of one server: the registry is the same list everywhere
/// (`Analysis::registered`), and each module's table is `'static`. Read once, on the first name
/// spelled.
fn invented() -> &'static [(&'static str, &'static str)] {
    static INVENTED: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    INVENTED.get_or_init(|| super::Analysis::registered().shown())
}

/// Split a declaration name into the pair a symbol list shows: the label, and the container printed
/// beside it.
///
/// The label matches what the outline calls the same construct, so a symbol reads the same whether
/// found in one file or across the project. The container is the *full* path: `self.baz` alone is
/// ambiguous, and the picker has a column for exactly this.
#[must_use]
pub fn split_qualified(graph: &Graph, name: &str) -> (String, Option<String>) {
    let name = &*spelled(graph, name);
    if let Some((owner, method)) = name.rsplit_once('#') {
        let method = simple_name(method);
        return match singleton_parts(owner) {
            // A singleton method: `class << self` is an implementation detail, so it is spelled as
            // written, and the container is the class it hangs off.
            Some((prefix, singleton)) => (
                format!("self.{method}"),
                Some(if prefix.is_empty() { singleton } else { prefix }.to_owned()),
            ),
            None => (method.to_owned(), non_empty(owner)),
        };
    }
    match name.rsplit_once("::") {
        Some((owner, simple)) => (simple.to_owned(), non_empty(owner)),
        None => (name.to_owned(), None),
    }
}

/// `Foo::Bar::<Bar>` -> `("Foo::Bar", "Bar")`, and a top-level `<Foo>` -> `("", "Foo")`.
///
/// `None` when the name is not a singleton class, which covers every name rubydex spells without
/// angle brackets (its only use for them).
fn singleton_parts(owner: &str) -> Option<(&str, &str)> {
    let rest = owner.strip_suffix('>')?;
    let (prefix, singleton) = rest.rsplit_once('<')?;
    let prefix = prefix.strip_suffix("::").unwrap_or(prefix);
    // `Foo::<Bar>` is not `Bar`'s singleton class written the long way round; refusing it keeps an
    // unexpected shape from being rendered as something it is not.
    (prefix.is_empty() || prefix.ends_with(singleton)).then_some((prefix, singleton))
}

/// The class a singleton class hangs off: `Foo::Bar::<Bar>` -> `Foo::Bar`, `<Foo>` -> `Foo`.
///
/// `None` for every other name, i.e. every name rubydex spells without angle brackets. A third
/// caller of [`singleton_parts`], not a second copy of the rule: naming the receiver of `Foo.bar`
/// and printing the container beside `self.bar` ask the same question of the same string.
#[must_use]
pub fn class_object_of(name: &str) -> Option<&str> {
    let (prefix, singleton) = singleton_parts(name)?;
    Some(if prefix.is_empty() { singleton } else { prefix })
}

fn non_empty(name: &str) -> Option<String> {
    (!name.is_empty()).then(|| name.to_owned())
}

/// The simple name of a definition, without rubydex's method parentheses.
#[must_use]
pub fn simple_name(raw: &str) -> &str {
    raw.strip_suffix("()").unwrap_or(raw)
}

/// rubydex's suffix for a class or module it had nothing to call.
const ANONYMOUS: &str = "<anonymous>";

/// Whether rubydex named this by number because nothing named it in Ruby.
///
/// `Class.new` and `Module.new` are expressions, so what they build has no name until bound to a
/// constant. Where nothing binds it, rubydex keys it by document and offset:
/// `15613248007104500482:144<anonymous>`.
#[must_use]
pub fn is_anonymous(name: &str) -> bool {
    // **The first byte before the whole marker.** `str::contains(&str)` builds a two-way searcher
    // on every call, and this is asked of every candidate a completion renders, so it showed up as
    // a real share of the analysis thread's CPU in `StrSearcher::new`. A name holding the marker
    // holds its `<`, and the byte search is a `memchr`, so the guard is exact, not approximate.
    name.contains('<') && name.contains(ANONYMOUS)
}

/// A name with every number rubydex invented replaced by the call that built it.
///
/// **There is nothing better to print.** rubydex already names `Foo = Class.new do … end` as `Foo`
/// (in a method body, a block, a `class << self`, under any superclass path), so what stays
/// anonymous is what Ruby left anonymous. `Foo = Class.new { … }.new` is not an exception: there
/// the constant is an *instance* of the class, and lending its name to the class would print
/// something untrue.
///
/// **Every occurrence, not the first.** A singleton method of one is
/// `<id>:<offset><anonymous>::<<id>:<offset><anonymous>>#call`, and replacing both halves with the
/// same string lets [`singleton_parts`] recognise the shape and spell the whole thing
/// `Class.new.call`.
///
/// Borrowed unless there is something to replace: this runs once per completion item, and a list
/// holds up to 512.
fn spelled<'n>(graph: &Graph, name: &'n str) -> Cow<'n, str> {
    if !is_anonymous(name) {
        return Cow::Borrowed(name);
    }
    let mut spelled = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(marker) = rest.find(ANONYMOUS) {
        let end = marker + ANONYMOUS.len();
        match keyed_at(&rest[..marker]) {
            Some(start) => {
                spelled.push_str(&rest[..start]);
                spelled.push_str(constructor(graph, &rest[start..end]));
            }
            // A suffix with no key in front of it is not a name rubydex wrote. Left alone, not
            // guessed at: a display name must never invent.
            None => spelled.push_str(&rest[..end]),
        }
        rest = &rest[end..];
    }
    spelled.push_str(rest);
    Cow::Owned(spelled)
}

/// Where the `15613248007104500482:144` before an [`ANONYMOUS`] suffix begins: a document id, a
/// colon, and an offset, each at least one digit.
///
/// The prefix is what a name is *keyed* by, so it is also what the graph is asked for. Digits and
/// one colon, never more: walking back over a `::` would swallow the namespace before it and spell
/// `Foo::<id>:<offset><anonymous>` as though `Foo` were not there.
fn keyed_at(head: &str) -> Option<usize> {
    let offset = head.trim_end_matches(|character: char| character.is_ascii_digit());
    let document = offset.strip_suffix(':')?;
    let start = document.trim_end_matches(|character: char| character.is_ascii_digit());
    (offset.len() < head.len() && start.len() < document.len()).then_some(start.len())
}

/// Which of the two calls built it.
///
/// rubydex spells both the same way, so only the keyed declaration says which, and modules are a
/// large share of anonymous namespaces that own methods, so a guess would often be wrong. Nothing
/// guarantees the owner survived the walk that reached its member; a key the graph has lost is
/// still spelled, not printed raw: `Class.new` names the construct either way, and a module is a
/// narrower claim about something no longer there.
fn constructor(graph: &Graph, keyed: &str) -> &'static str {
    match graph.declarations().get(&DeclarationId::from(keyed)) {
        Some(Declaration::Namespace(Namespace::Module(_))) => "Module.new",
        _ => "Class.new",
    }
}

/// Whether a declaration has a name a person could have written.
///
/// Angle brackets are rubydex's only punctuation for invented names, and it invents two kinds:
/// `Foo::<Foo>` for a singleton class, and `<uri>:<offset><anonymous>` for an unbound `Class.new`.
/// Neither can be typed, so neither belongs in a list of things to type; without this, a large
/// workspace answered `::` with a page of `10042574982090812855:14001<anonymous>`.
#[must_use]
pub fn is_nameable(name: &str) -> bool {
    segment_is_nameable(last_segment(name))
}

/// [`is_nameable`], asked of a [`last_segment`] the caller already holds.
///
/// The rule stays one line in one place; this spelling saves a second walk of the name.
/// `completion::ranked_declaration` needs both the check and the label for every candidate in the
/// graph, so taking them separately would walk each method name twice, across the workspace and its
/// whole bundle.
#[must_use]
pub fn segment_is_nameable(segment: &str) -> bool {
    !segment.contains('<')
}

/// The last segment of a declaration name: the part a person types.
///
/// `Foo::Bar#baz()` -> `baz`, `Foo::Bar` -> `Bar`, `Parent#@var` -> `@var`. Shared by the symbol
/// picker, which matches against it, and completion, which shows it: a name searched one way and
/// inserted another is a bug waiting to happen.
#[must_use]
pub fn last_segment(name: &str) -> &str {
    // **Two pattern searches, not one reverse walk: measured, not preferred.** This is the crate's
    // hottest string function (`ranked_declaration` asks it of every method declaration in the
    // graph), and `memrchr`'s prologue looked like overhead for a name whose `#` is five bytes from
    // the end. A hand-written byte-by-byte walk answering both rules in one pass was **slower** on
    // the audit's prefix sweep, so the vectorised search stays, even at this length.
    let tail = match name.rsplit_once('#') {
        Some((_, member)) => member,
        None => name.rsplit_once("::").map_or(name, |(_, simple)| simple),
    };
    simple_name(tail)
}

/// A method's parameter list, rendered as Ruby: `(volume = 10, *rest, sep:, **opts, &blk)`.
///
/// Empty for a method taking nothing, so `Person#shout` reads the way it is called.
///
/// **A default is printed as the `def` writes it** where `written` holds it
/// ([`types::written_defaults`]) and it fits on the line ([`LONGEST_DEFAULT`], one line). Anywhere
/// else `= ...` stands in: rubydex records that a parameter is optional, not what it defaults to,
/// and RBS writes no default at all.
#[must_use]
pub fn parameter_list(
    graph: &Graph,
    signatures: &Signatures,
    written: &[(ParameterSlot, String)],
) -> String {
    typed_parameter_list(graph, signatures, written, &[])
}

/// [`parameter_list`], with the type written before each parameter one is known for:
/// `(String | untyped name, Integer count: 1)`, as RBS puts a type before a positional's name. A
/// card's line, where the parameter's own type is the one thing about it not in the `def`.
#[must_use]
pub fn typed_parameter_list(
    graph: &Graph,
    signatures: &Signatures,
    written: &[(ParameterSlot, String)],
    typed: &[(ParameterSlot, String)],
) -> String {
    // Ruby has exactly one signature per method; overloads only come from RBS.
    signatures
        .as_slice()
        .first()
        .map_or_else(String::new, |signature| {
            labelled(graph, "", signature, written, typed).label
        })
}

/// The longest default a signature line prints, in characters. A longer one is `...`: the line is
/// the method's shape, and a default that needs a reader's attention is in the `def`.
const LONGEST_DEFAULT: usize = 32;

/// A method's signature as Ruby, and where inside it each parameter was written.
///
/// Produced together on purpose. `signatureHelp` highlights a parameter by giving the client
/// offsets into this very string. LSP's other option (the parameter as a substring to search for)
/// mis-highlights as soon as a label holds the same token twice, which `def each(key, value = key)`
/// already does. So the function that writes the label reports where it wrote each piece, and
/// nothing downstream counts characters.
///
/// **The offsets are UTF-16 code units**, which is how clients index the label: the protocol ties
/// `Position` to the negotiated encoding but says nothing about these, and every client that
/// renders them holds the label as a UTF-16 string. Ruby names can be non-ASCII
/// (`def приветствие(имя)` is legal), so the two counts really differ.
#[must_use]
pub fn signature_label(graph: &Graph, name: &str, signature: &[Parameter]) -> Signature {
    labelled(graph, name, signature, &[], &[])
}

/// [`signature_label`], with the defaults a `def` writes ([`parameter_list`]).
fn labelled(
    graph: &Graph,
    name: &str,
    signature: &[Parameter],
    written: &[(ParameterSlot, String)],
    typed: &[(ParameterSlot, String)],
) -> Signature {
    let mut label = name.to_owned();
    let mut at = utf16_len(name);
    if signature.is_empty() {
        return Signature {
            label,
            parameters: Vec::new(),
        };
    }

    let mut parameters = Vec::with_capacity(signature.len());
    label.push('(');
    at += 1;
    // [`ParameterSlot::Positional`] counts required then optional positionals, as the `def`'s
    // defaults were keyed.
    let mut positional = 0;
    for (index, parameter) in signature.iter().enumerate() {
        if index > 0 {
            label.push_str(", ");
            at += 2;
        }
        let name = graph
            .strings()
            .get(parameter.inner().str())
            .map_or_else(String::new, |string| string.as_str().to_owned());
        let slot = match parameter {
            Parameter::RequiredPositional(_) | Parameter::OptionalPositional(_) => {
                positional += 1;
                Some(ParameterSlot::Positional(positional - 1))
            }
            Parameter::OptionalKeyword(_) => Some(ParameterSlot::Keyword(name.clone())),
            _ => None,
        };
        let held = match parameter {
            Parameter::RequiredKeyword(_) => Some(ParameterSlot::Keyword(name.clone())),
            _ => slot.clone(),
        }
        .and_then(|held| typed.iter().find(|(slot, _)| *slot == held))
        .map(|(_, spelled)| spelled.as_str());
        let default = slot
            .and_then(|slot| written.iter().find(|(held, _)| *held == slot))
            .map(|(_, text)| text.as_str())
            .filter(|text| !text.contains('\n') && text.chars().count() <= LONGEST_DEFAULT);
        let written = match held {
            Some(spelled) => format!("{spelled} {}", spell(parameter, name, default)),
            None => spell(parameter, name, default),
        };
        let width = utf16_len(&written);
        parameters.push((at, at + width));
        label.push_str(&written);
        at += width;
    }
    label.push(')');

    Signature { label, parameters }
}

/// One rendered signature: the whole line, and one span per parameter inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub label: String,
    /// Start and end of each parameter in `label`, in UTF-16 code units, in signature order.
    pub parameters: Vec<(u32, u32)>,
}

/// One parameter, as Ruby writes it, with its default where the `def`'s text gave one.
fn spell(parameter: &Parameter, name: String, default: Option<&str>) -> String {
    let default = default.unwrap_or("...");
    match parameter {
        Parameter::RequiredPositional(_) | Parameter::Post(_) => name,
        Parameter::OptionalPositional(_) => format!("{name} = {default}"),
        Parameter::RestPositional(_) => sigil("*", &name),
        Parameter::RequiredKeyword(_) => format!("{name}:"),
        Parameter::OptionalKeyword(_) => format!("{name}: {default}"),
        Parameter::RestKeyword(_) => sigil("**", &name),
        Parameter::Block(_) => sigil("&", &name),
        Parameter::Forward(_) => "...".to_owned(),
    }
}

/// How long a string is to a client holding it as UTF-16, saturating instead of wrapping: a label
/// long enough to overflow a `u32` is not one anybody is reading.
fn utf16_len(text: &str) -> u32 {
    u32::try_from(text.encode_utf16().count()).unwrap_or(u32::MAX)
}

/// A rest, keyword-rest or block parameter, written once.
///
/// Ruby 3.x allows all three to be anonymous (`def f(*, **, &)`), and rubydex records those under
/// the sigil itself instead of an empty name, so prepending unconditionally would spell `**` as
/// `****`. A parameter whose recorded name already *is* its sigil is written as it stands.
fn sigil(sigil: &str, name: &str) -> String {
    if name == sigil {
        sigil.to_owned()
    } else {
        format!("{sigil}{name}")
    }
}

/// The documentation comment above a definition, as markdown.
///
/// `None` when there is nothing left after the magic comments are dropped.
#[must_use]
pub fn documentation(comments: &[Comment]) -> Option<String> {
    let mut lines: Vec<&str> = comments
        .iter()
        .map(|comment| strip_marker(comment.string()))
        .collect();

    // rubydex attaches the comment block above a definition, allowing one blank line in between, so
    // `# frozen_string_literal: true` at the top of a file becomes the first class's documentation.
    // Directives only ever lead, so dropping them from the front leaves prose (and YARD tags)
    // untouched.
    let leading = lines.iter().take_while(|line| is_directive(line)).count();
    lines.drain(..leading);
    // RDoc's note of the file a comment was extracted from, as one HTML comment on its own line:
    // `<!-- rdoc-file=string.rb -->` above most of Ruby's own class docs. A reader never sees it in
    // RDoc's output, and escaped here it would be the card's first line.
    lines.retain(|line| !is_rdoc_file(line));

    let call_seq = take_rdoc_header(&mut lines);

    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }

    if lines.is_empty() && call_seq.is_empty() {
        return None;
    }
    let body = to_markdown(&lines.join("\n"));
    if call_seq.is_empty() {
        return Some(body);
    }
    Some(format!("```ruby\n{}\n```\n\n{body}", call_seq.join("\n")))
}

/// `<!-- rdoc-file=hash.c -->`, the whole line: where RDoc read a comment from, which the
/// multi-line header [`take_rdoc_header`] reads carries inside it instead.
fn is_rdoc_file(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("<!-- rdoc-file=") && line.ends_with("-->")
}

/// RDoc's markup, as markdown a client will actually render.
///
/// Ruby's own signatures carry the documentation RDoc extracted from the C source, and it is partly
/// HTML: many `<code>` spans in the vendored copy, plus `<em>`, `<strong>`, `<tt>`, `<b>` and
/// `<i>`. A `MarkupContent` is markdown, and every client strips HTML from it, so
/// `<code><=></code>` reaches the user as a bare `<=>` without its markup, and a tag that is not
/// markup at all (`<vowel>`, `<rhs>`, `<main>` and `<html>` all appear in the prose) silently
/// disappears along with its angle brackets.
///
/// RDoc's links go nowhere either: `[Case Mapping](rdoc-ref:case_mapping.rdoc)` points into a
/// documentation tree the editor has never seen, and `core/` is full of them, each a dead word the
/// user can click.
///
/// Code is left exactly as written (a fenced block, an indented block, a backtick span). What is
/// inside is Ruby, and `Hash<Symbol, untyped>` in an example must not grow a backslash.
fn to_markdown(text: &str) -> String {
    let mut chunks: Vec<String> = Vec::new();
    let mut prose: Vec<String> = Vec::new();
    let mut fenced = false;
    // Whether the indented lines below belong to a list item or to an example. In RDoc the same two
    // spaces mean both, and what opened above decides; see [`list_item`]. It survives a blank line,
    // because a labelled list item with two paragraphs is ordinary, and the second still belongs to
    // the item.
    let mut listing = false;

    for line in text.lines() {
        let fence = line.trim_start().starts_with("```");
        if fence {
            fenced = !fenced;
        }
        if fenced || fence {
            flush(&mut chunks, &mut prose);
            chunks.push(line.to_owned());
            continue;
        }
        if let Some(heading) = heading(line) {
            flush(&mut chunks, &mut prose);
            chunks.push(heading);
            listing = false;
            continue;
        }
        if let Some(label) = list_item(line) {
            flush(&mut chunks, &mut prose);
            chunks.push(format!("- **{}**", converted(label)));
            listing = true;
            continue;
        }
        if line.trim().is_empty() {
            prose.push(line.to_owned());
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent >= 2 && !line.starts_with('\t') {
            if listing {
                // The item's own text, keeping its indentation: markdown reads an indented line
                // under a `-` as its continuation, which is what RDoc means too.
                prose.push(line.to_owned());
            } else {
                // A verbatim block, which is RDoc's **two** spaces and markdown's four. Every
                // example in Rails' own comments is written this way, and reading one as prose
                // would render examples as plain text.
                flush(&mut chunks, &mut prose);
                let pad = " ".repeat(4_usize.saturating_sub(indent));
                chunks.push(format!("{pad}{line}"));
            }
            continue;
        }
        if line.starts_with('\t') {
            flush(&mut chunks, &mut prose);
            chunks.push(line.to_owned());
            continue;
        }
        // Back at the margin with something on the line: whatever list was open is closed.
        listing = false;
        prose.push(line.to_owned());
    }
    flush(&mut chunks, &mut prose);
    chunks.join("\n")
}

/// Convert whatever prose has accumulated and put it in `chunks`.
fn flush(chunks: &mut Vec<String>, prose: &mut Vec<String>) {
    if !prose.is_empty() {
        chunks.push(converted(&prose.join("\n")));
        prose.clear();
    }
}

/// `== Options` -> `## Options`, and nothing for a line that is not a heading.
///
/// RDoc's heading is a run of `=` at the margin followed by a space: the one spelling markdown does
/// not share (markdown's underline form never appears in these comments). Six levels, where
/// markdown stops.
fn heading(line: &str) -> Option<String> {
    let level = line.len() - line.trim_start_matches('=').len();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = line[level..].strip_prefix(' ')?;
    Some(format!("{} {rest}", "#".repeat(level)))
}

/// The label of an RDoc labelled list item (`[+:autosave+]` or `autosave::`) at the margin.
///
/// The one construct that must be recognised before the indentation is read, because it makes the
/// two spaces below it mean *description*, not *example*. Rails writes many of these in
/// `has_many`'s comment alone, and read as verbatim, every option's description would become a code
/// block.
fn list_item(line: &str) -> Option<&str> {
    if line.starts_with(' ') || line.starts_with('\t') {
        return None;
    }
    if let Some(inner) = line
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return (!inner.is_empty() && !inner.contains(']')).then_some(inner);
    }
    let label = line.strip_suffix("::")?;
    (!label.is_empty() && !label.contains(' ')).then_some(label)
}

/// One run of prose, converted. Takes whole lines, because RDoc wraps its links across them.
fn converted(prose: &str) -> String {
    let mut out = String::with_capacity(prose.len());
    let mut at = 0;

    while at < prose.len() {
        let rest = &prose[at..];
        if rest.starts_with('`') {
            // Already code, and its contents are not markup. Copied out whole.
            let span = code_span(rest);
            out.push_str(span);
            at += span.len();
        } else if let Some((label, taken)) = rdoc_link(rest) {
            out.push_str(&converted(label));
            at += taken;
        } else if let Some((rendered, taken)) = braced_link(rest) {
            out.push_str(&rendered);
            at += taken;
        } else if let Some((code, taken)) = plus_code(rest, out.chars().last()) {
            out.push_str(&code);
            at += taken;
        } else if let Some(taken) = suppressed(rest) {
            out.push_str(&rest[1..taken]);
            at += taken;
        } else if let Some((rendered, taken)) = inline_tag(rest) {
            out.push_str(&rendered);
            at += taken;
        } else {
            let ch = rest
                .chars()
                .next()
                .expect("a non-empty remainder has a char");
            // Anything still angled here is not a tag markdown knows, and a renderer would eat it
            // and everything up to the next `>`. In these comments `Array<Integer>` is prose far
            // more often than markup.
            if ch == '<' {
                out.push('\\');
            }
            out.push(ch);
            at += ch.len_utf8();
        }
    }
    out
}

/// A backtick span, from its opening run to the matching closing run of the same length.
///
/// An opener with no closer is a stray backtick, and is one character of text.
fn code_span(rest: &str) -> &str {
    let fence = rest.len() - rest.trim_start_matches('`').len();
    match rest[fence..].find(&"`".repeat(fence)) {
        Some(end) => &rest[..fence + end + fence],
        None => &rest[..fence],
    }
}

/// `[label](rdoc-ref:…)` — the label, and how much of `rest` it accounted for.
///
/// Only RDoc's own scheme. An `https:` link in a comment is a link the editor can follow, and
/// is left exactly as it was written.
fn rdoc_link(rest: &str) -> Option<(&str, usize)> {
    let separator = rest.strip_prefix('[')?.find("](")? + 1;
    let target = &rest[separator + 2..];
    let end = target.find(')')?;
    target
        .starts_with("rdoc-ref:")
        .then(|| (&rest[1..separator], separator + 2 + end + 1))
}

/// `{text}[url]`: RDoc's own link, the spelling a `.rb` file uses.
///
/// [`rdoc_link`] handles `[text](url)`, which the *vendored signatures* carry because RDoc
/// generated them; a gem's own source is written in RDoc itself and needs this one. An `rdoc-ref:`
/// target points into a documentation tree the editor has never seen, so it is treated like the
/// other: the words stay and the dead link goes. A real URL is kept, because an editor can follow
/// it.
fn braced_link(rest: &str) -> Option<(String, usize)> {
    let end = rest.strip_prefix('{')?.find("}[")?;
    let text = &rest[1..=end];
    let target = &rest[end + 3..];
    let close = target.find(']')?;
    let url = &target[..close];
    let taken = end + 3 + close + 1;
    if url.starts_with("rdoc-ref:") || url.contains(' ') {
        return Some((converted(text), taken));
    }
    Some((format!("[{}]({url})", converted(text)), taken))
}

/// `+word+` as a code span: RDoc's own emphasis for code.
///
/// It means the same as `<tt>`, and both must render the same: otherwise `<tt>:autosave</tt>` comes
/// out as code while `+:autosave+` shows as literal pluses in the same card. RDoc's rule: the `+`
/// opens at a non-word boundary and closes before one, with no whitespace inside, which keeps
/// `1 + 2` and `a+b` as prose.
fn plus_code(rest: &str, previous: Option<char>) -> Option<(String, usize)> {
    if previous.is_some_and(|ch| ch.is_alphanumeric() || ch == '_') {
        return None;
    }
    let inner = rest.strip_prefix('+')?;
    let end = inner.find('+')?;
    let word = &inner[..end];
    if word.is_empty() || word.chars().any(char::is_whitespace) {
        return None;
    }
    // A word character straight after the closing `+` means it never closed a span.
    if inner[end + 1..]
        .chars()
        .next()
        .is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
    {
        return None;
    }
    Some((fenced_code(word), end + 2))
}

/// `\Word`: RDoc's escape, asking for the word with no link. The backslash is not text.
///
/// Only before a letter, because `\n` in a sentence about escapes is the thing itself, and markdown
/// would eat the backslash anyway.
fn suppressed(rest: &str) -> Option<usize> {
    let word = rest.strip_prefix('\\')?;
    let first = word.chars().next()?;
    first.is_alphabetic().then(|| 1 + first.len_utf8())
}

/// An inline HTML tag as the markdown that means the same thing, and how much it accounted for.
fn inline_tag(rest: &str) -> Option<(String, usize)> {
    let close = rest.strip_prefix('<')?.find('>')?;
    let name = &rest[1..=close];
    let marker = match name {
        "code" | "tt" => "`",
        "em" | "i" => "*",
        "strong" | "b" => "**",
        _ => return None,
    };
    let opened = name.len() + 2;
    let closing = format!("</{name}>");
    let end = rest[opened..].find(&closing)?;
    let inner = &rest[opened..opened + end];
    let rendered = if marker == "`" {
        fenced_code(inner)
    } else {
        format!("{marker}{}{marker}", converted(inner))
    };
    Some((rendered, opened + end + closing.len()))
}

/// `inner` as a backtick span, whatever backticks it holds.
///
/// ``<code>$`</code>`` is in Ruby's own signatures (the global holding what preceded a match), and
/// a one-backtick fence around it would end the span mid-name. CommonMark's answer is a longer
/// fence, plus a space at each end when the content starts or ends with one.
fn fenced_code(inner: &str) -> String {
    let longest = inner
        .split(|ch: char| ch != '`')
        .fold(0, |longest: usize, run| longest.max(run.len()));
    let fence = "`".repeat(longest + 1);
    let pad = if inner.starts_with('`') || inner.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{inner}{pad}{fence}")
}

/// Take RDoc's header off the front of an RBS comment, keeping the call-seq lines.
///
/// Ruby's core signatures carry the documentation RDoc extracted from the C source, and it arrives
/// wrapped:
///
/// ```text
/// <!--
///   rdoc-file=string.c
///   - upcase(mapping = :ascii) -> new_string
/// -->
/// Returns a new string containing the upcased characters in `self`:
/// ```
///
/// Rendered as markdown, that whole block disappears (HTML comments are invisible), taking the
/// call-seq with it. For a method implemented in C, the call-seq is the only place the block forms
/// are written down (`each {|element| ... } -> self`), and it says more than the RBS signature, so
/// it is lifted out as code and the rest of the wrapper is dropped.
fn take_rdoc_header<'a>(lines: &mut Vec<&'a str>) -> Vec<&'a str> {
    if lines.first().is_none_or(|line| line.trim() != "<!--") {
        return Vec::new();
    }
    let Some(end) = lines.iter().position(|line| line.trim() == "-->") else {
        // An opener with no closer is not RDoc's header; leave the comment exactly as it was.
        return Vec::new();
    };

    let call_seq: Vec<&str> = lines[1..end]
        .iter()
        .filter_map(|line| line.trim().strip_prefix("- "))
        .collect();
    lines.drain(..=end);
    call_seq
}

/// `# Say hello.` -> `Say hello.`, keeping any indentation the author used for code samples.
fn strip_marker(comment: &str) -> &str {
    let body = comment.trim_start().strip_prefix('#').unwrap_or(comment);
    body.strip_prefix(' ').unwrap_or(body)
}

/// RDoc's own visibility directives: not prose, and the whole comment when they are the whole
/// comment.
///
/// `:nodoc:` above a `def` means "there is no documentation here", and printing the word is worse
/// than the empty card it asks for, because a reader takes a card with anything in it as an answer.
const RDOC_DIRECTIVES: [&str; 6] = [
    ":nodoc:",
    ":doc:",
    ":startdoc:",
    ":stopdoc:",
    ":enddoc:",
    ":yields:",
];

/// A magic comment, a linter pragma, an RDoc directive, or a shebang: never documentation.
///
/// A deliberately narrow test: an all-lowercase word followed immediately by a colon.
/// `TODO: rewrite` and `Note: this is fine` are prose and survive.
fn is_directive(line: &str) -> bool {
    let line = line.trim_start();
    if line.starts_with('!') || line.starts_with("-*-") {
        return true;
    }
    // `:nodoc: all` is the spelling with an argument; both are the directive and neither is
    // documentation.
    if RDOC_DIRECTIVES
        .iter()
        .any(|directive| line == *directive || line.starts_with(&format!("{directive} ")))
    {
        return true;
    }
    let Some((word, _)) = line.split_once(':') else {
        return false;
    };
    !word.is_empty()
        && word
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch == '_' || ch.is_ascii_digit())
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use rubydex::model::declaration::{ClassDeclaration, MethodDeclaration, ModuleDeclaration};
    use rubydex::offset::Offset;

    /// An empty graph, for spellings that never reach one.
    ///
    /// Every name below was written in Ruby, and [`spelled`] leaves those alone without asking, so
    /// this graph exists only to satisfy the signature the anonymous `Class.new` case needs. Tests
    /// that do reach the graph build a real one.
    fn no_graph() -> Graph {
        Graph::new()
    }

    #[test]
    fn the_last_segment_is_the_rightmost_hash_or_else_the_rightmost_pair_of_colons() {
        // The three shapes the doc comment names, and the `()` rubydex puts on a method.
        assert_eq!(last_segment("Foo::Bar#baz()"), "baz");
        assert_eq!(last_segment("Foo::Bar"), "Bar");
        assert_eq!(last_segment("Parent#@var"), "@var");

        // A bare name has neither separator, and a top-level method has only the `#`.
        assert_eq!(last_segment("Bar"), "Bar");
        assert_eq!(last_segment("#call()"), "call");
        assert_eq!(last_segment(""), "");

        // **The `#` wins wherever both are present, however many `::` sit to its right.**
        assert_eq!(last_segment("A#b::c"), "b::c");
        assert_eq!(last_segment("Foo::Bar::<Bar>#baz"), "baz");

        // The rightmost pair, and a single colon is not one. rubydex keys an anonymous namespace
        // `<id>:<offset><anonymous>`, which is exactly a name with one colon and no pair.
        assert_eq!(last_segment("::Foo"), "Foo");
        assert_eq!(last_segment("a:::b"), "b");
        assert_eq!(
            last_segment("15613248007104500482:144<anonymous>"),
            "15613248007104500482:144<anonymous>"
        );
        assert_eq!(last_segment(":"), ":");
    }

    fn comments(lines: &[&str]) -> Vec<Comment> {
        lines
            .iter()
            .map(|line| Comment::new(Offset::new(0, 0), (*line).into()))
            .collect()
    }

    /// The fixture is `ActiveRecord::Associations::ClassMethods#has_many`'s own comment: a labelled
    /// list of options, a paragraph under each label, then a run of examples. All three are two
    /// spaces in RDoc and mean different things.
    #[test]
    fn rdoc_written_in_a_gems_own_source_renders_as_rdoc() {
        let card = documentation(&comments(&[
            "# == Options",
            "#",
            "# [+:autosave+]",
            "#   If true, always save the associated objects. This option is implemented as a",
            "#   +before_save+ callback.",
            "# [+:inverse_of+]",
            "#   Specifies the name of the association.",
            "#   See {Bi-directional}[rdoc-ref:Associations::ClassMethods@Bi] for more detail.",
            "#",
            "# Option examples:",
            "#   has_many :comments, -> { order(\"posted_on\") }",
            "#   has_many :tags, as: :taggable",
        ]))
        .expect("a card");
        assert_eq!(
            card,
            "\
## Options

- **`:autosave`**
  If true, always save the associated objects. This option is implemented as a
  `before_save` callback.
- **`:inverse_of`**
  Specifies the name of the association.
  See Bi-directional for more detail.

Option examples:
    has_many :comments, -> { order(\"posted_on\") }
    has_many :tags, as: :taggable"
        );
    }

    /// One card, two spellings of the same thing: both must be read.
    #[test]
    fn plus_and_tt_are_the_same_markup() {
        let plus = documentation(&comments(&["# Set +:autosave+ to true."]));
        let tt = documentation(&comments(&["# Set <tt>:autosave</tt> to true."]));
        assert_eq!(plus.as_deref(), Some("Set `:autosave` to true."));
        assert_eq!(plus, tt);
    }

    /// What a `+` is when it is arithmetic, a word, or one of a pair with a space in it.
    #[test]
    fn a_plus_that_is_not_markup_stays_a_plus() {
        for line in [
            "# The sum of 1 + 2 is 3.",
            "# Written a+b+c in the source.",
            "# Use + to add and + to concatenate.",
        ] {
            let rendered = documentation(&comments(&[line])).expect("a card");
            assert_eq!(rendered, strip_marker(line), "{line}");
        }
    }

    /// `:nodoc:` is RDoc saying there is nothing here, and a card containing the word is worse than
    /// no card: a reader takes anything in a card as an answer.
    #[test]
    fn a_nodoc_comment_is_no_documentation_at_all() {
        assert_eq!(documentation(&comments(&["# :nodoc:"])), None);
        assert_eq!(documentation(&comments(&["# :nodoc: all"])), None);
        assert_eq!(documentation(&comments(&["# :stopdoc:"])), None);
        // And it only leads. A `:nodoc:` written *after* prose is someone discussing the directive,
        // and the prose above it is documentation.
        assert_eq!(
            documentation(&comments(&["# Marks it hidden.", "# :nodoc:"])).as_deref(),
            Some("Marks it hidden.\n:nodoc:")
        );
    }

    /// The guarantee that must not move: a verbatim block holds Ruby, and a generic in an example
    /// must not grow a backslash.
    #[test]
    fn a_verbatim_block_is_never_escaped_however_it_is_indented() {
        let card = documentation(&comments(&[
            "# Returns a hash:",
            "#   Hash<Symbol, untyped>",
            "# and a Array<Integer> in prose.",
        ]))
        .expect("a card");
        assert!(card.contains("    Hash<Symbol, untyped>"), "{card}");
        assert!(card.contains("a Array\\<Integer> in prose"), "{card}");
    }

    /// The shapes each RDoc reader declines, one per way of not being the thing.
    #[test]
    fn the_rdoc_spellings_that_are_not_markup() {
        let card = |lines: &[&str]| documentation(&comments(lines)).expect("a card");

        // A heading deeper than markdown has, and a run of `=` with no space after it.
        assert_eq!(card(&["# ======= Too deep"]), "======= Too deep");
        assert_eq!(card(&["# ==nospace"]), "==nospace");
        // A tab-indented block is verbatim and left exactly as written, at one tab and at two (the
        // second is the one that gets past the two-space test first).
        assert_eq!(card(&["# Prose.", "#\tstill_code"]), "Prose.\n\tstill_code");
        assert_eq!(card(&["# Prose.", "#\t\tdeeper"]), "Prose.\n\t\tdeeper");
        // `[a]b]` is not a label: RDoc's label runs to the *first* `]`, so a line with two is prose
        // that happens to start with a bracket.
        assert_eq!(card(&["# [a]b]"]), "[a]b]");
        // The other spelling of a labelled list, which `is_directive` would eat if it led.
        assert_eq!(
            card(&["# Options.", "# autosave::", "#   If true."]),
            "Options.\n- **autosave**\n  If true."
        );
        // A label with a space in it is a sentence ending in a colon pair, not a list; and neither
        // empty spelling of either form is a list.
        assert_eq!(card(&["# Prose.", "# see also::"]), "Prose.\nsee also::");
        assert_eq!(card(&["# Prose.", "# []"]), "Prose.\n[]");
        assert_eq!(card(&["# Prose.", "# ::"]), "Prose.\n::");
        // A link with a real target keeps it; one whose target is not a URL keeps only its words.
        assert_eq!(
            card(&["# See {the guide}[https://example.com/g] for more."]),
            "See [the guide](https://example.com/g) for more."
        );
        assert_eq!(
            card(&["# See {the guide}[not a url] for more."]),
            "See the guide for more."
        );
        // `++` has nothing between the pluses, and `+a+b` never closed: both are prose.
        assert_eq!(card(&["# An empty ++ pair."]), "An empty ++ pair.");
        assert_eq!(
            card(&["# Written +a+b in the source."]),
            "Written +a+b in the source."
        );
    }

    /// A `\\Word` is RDoc asking for the word without a link.
    #[test]
    fn a_suppressed_link_keeps_its_word_and_loses_its_backslash() {
        assert_eq!(
            documentation(&comments(&["# See \\Array for more."])).as_deref(),
            Some("See Array for more.")
        );
    }

    #[test]
    fn an_invented_name_is_shown_as_its_body_of_knowledge_says_only_where_it_is_generated() {
        // Where the graph holds no such namespace, or holds one no generator wrote (here, with no
        // definition at all), the name is the project's own and is spelled as written. A generated
        // one is spelled as its table says: the relations, route and view tests reach that half.
        assert_eq!(
            qualified_name(&no_graph(), "RouteHelpers#story_path()"),
            "RouteHelpers#story_path"
        );
        let declared = graph_holding(
            "ActiveRecordRelation",
            Namespace::Class(Box::new(ClassDeclaration::new(
                "ActiveRecordRelation".to_owned(),
                DeclarationId::from("Object"),
            ))),
        );
        assert_eq!(
            qualified_name(&declared, "ActiveRecordRelation#where()"),
            "ActiveRecordRelation#where"
        );
        assert_eq!(
            qualified_name(&declared, "ActiveRecordRelation"),
            "ActiveRecordRelation"
        );
    }

    #[test]
    fn rdocs_note_of_its_source_file_is_not_documentation() {
        // One line, above most of Ruby's own class docs; escaped, it was the card's first line.
        assert_eq!(
            documentation(&comments(&[
                "# <!-- rdoc-file=hash.c -->",
                "# `ENV` is a Hash-like accessor.",
            ]))
            .as_deref(),
            Some("`ENV` is a Hash-like accessor.")
        );
        // Only the whole-line comment: an opening with no close is some other HTML.
        assert!(is_rdoc_file("  <!-- rdoc-file=string.rb -->"));
        assert!(!is_rdoc_file("<!-- rdoc-file=string.rb"));
        assert!(!is_rdoc_file("<!-- a note -->"));
    }

    #[test]
    fn a_method_with_no_signature_at_all_renders_no_parameter_list() {
        // `Signatures` is `Simple(one)` or `Overloaded(many)`, and the second is a boxed slice the
        // type allows to be empty, even though rubydex builds it from RBS overloads and never
        // leaves it empty. It is a pre-1.0 dependency, so an empty one must give `Person#shout`, as
        // for `def shout`, not an index out of range.
        let graph = Graph::new();
        assert_eq!(
            parameter_list(&graph, &Signatures::Overloaded(Box::default()), &[]),
            ""
        );
    }

    #[test]
    fn a_top_level_singleton_method_is_named_after_its_own_class() {
        // The path is what a nested class needs (`Foo::Bar.baz`), and a top-level class has no
        // path: there the singleton *is* the whole name. Prepending an empty prefix would spell it
        // `.build`.
        assert_eq!(
            qualified_name(&no_graph(), "<Person>#build()"),
            "Person.build"
        );
        assert_eq!(
            qualified_name(&no_graph(), "Object::<Object>#puts()"),
            "Object.puts"
        );
    }

    #[test]
    fn a_singleton_class_is_named_by_the_class_it_hangs_off() {
        // The same three cases `qualified_name` has for a singleton *method*, asked of the class
        // itself: a nested one answers the path, a top-level one the whole name, and anything else
        // is not a singleton. A class object's name must be one a reader can open, and
        // `Person::<Person>` is not that.
        assert_eq!(class_object_of("Foo::Bar::<Bar>"), Some("Foo::Bar"));
        assert_eq!(class_object_of("<Person>"), Some("Person"));
        assert_eq!(class_object_of("Person"), None);
        // And the shape `singleton_parts` refuses: angle brackets naming somebody else.
        assert_eq!(class_object_of("Foo::<Bar>"), None);
    }

    #[test]
    fn the_other_two_shapes_a_directive_takes() {
        // `magic_comments_are_not_documentation` covers `word: value`. These are the two the word
        // test cannot reach: an emacs modeline, and a line whose colon has no word before it, which
        // is prose, not a directive.
        assert_eq!(documentation(&comments(&["# -*- coding: utf-8 -*-"])), None);
        assert_eq!(
            documentation(&comments(&["# : not a directive"])).as_deref(),
            Some(": not a directive")
        );
    }

    #[test]
    fn an_rdoc_call_sequence_survives_with_no_prose_under_it() {
        // `rdocs_header_becomes_a_signature_block` always has prose below. With none, every
        // remaining line has been drained, and there is still something worth showing, which is
        // what stops `documentation` answering `None`.
        let rendered = documentation(&comments(&[
            "# <!--",
            "#   rdoc-file=string.c",
            "#   - obj.freeze -> obj",
            "# -->",
        ]))
        .expect("a call sequence is documentation");
        assert_eq!(rendered, "```ruby\nobj.freeze -> obj\n```\n\n");
    }

    #[test]
    fn singleton_methods_are_spelled_the_way_ruby_writes_them() {
        // rubydex models `def self.build` as a member of a synthetic singleton class. Showing that
        // spelling would expose an implementation detail.
        assert_eq!(
            qualified_name(&no_graph(), "Person::<Person>#build()"),
            "Person.build"
        );
        assert_eq!(
            qualified_name(&no_graph(), "Person#shout()"),
            "Person#shout"
        );
        // The whole path, as in the instance-method spelling above: hover on two methods of one
        // class must not name the class two different ways.
        assert_eq!(
            qualified_name(&no_graph(), "Foo::Bar::<Bar>#baz()"),
            "Foo::Bar.baz"
        );
        // Not a method at all: namespaces and constants pass through untouched.
        assert_eq!(
            qualified_name(&no_graph(), "Person::MAX_AGE"),
            "Person::MAX_AGE"
        );
        assert_eq!(qualified_name(&no_graph(), "Person"), "Person");
    }

    #[test]
    fn a_type_whose_class_is_not_one_is_not_a_label_at_all() {
        // The gate the doc comment names, and why a raw rubydex key never reaches a reader: what a
        // margin draws must be a name somebody can open. A method declaration stands in for every
        // non-class shape (a constant, a `Namespace::Todo`, one of the query interface's two
        // sentinels), because the test is the kind, not the spelling.
        let mut graph = Graph::new();
        graph.declarations_mut().insert(
            DeclarationId::from("Person#shout()"),
            Declaration::Method(Box::new(MethodDeclaration::new(
                "Person#shout()".to_owned(),
                DeclarationId::from("Person"),
            ))),
        );
        assert_eq!(
            typed(
                &graph,
                &types::Typed::of(
                    DeclarationId::from("Person#shout()"),
                    types::Derivation::default(),
                ),
            ),
            None
        );
        // And a class the graph does not hold at all is the same answer by the line above it.
        assert_eq!(
            typed(
                &graph,
                &types::Typed::of(DeclarationId::from("Nowhere"), types::Derivation::default(),),
            ),
            None
        );
    }

    #[test]
    fn the_four_names_a_reader_writes_in_lower_case_are_drawn_in_lower_case() {
        // `TrueClass` is what the boolean fold leaves behind, so the rewrite walks the whole list,
        // not just its head, and every other name must pass through untouched. `bool | String` is
        // the shape: one exit answered a predicate and one a name, which is why the loop is a loop
        // and not a test on the first entry.
        //
        // The carrier alone is **not** the pair: a method that can only return `true` is not a
        // predicate, so it is drawn `true`, not `bool`. That is the distinction
        // `types::Folds::fold` makes one step earlier, read here off the flag it set instead of
        // made twice.
        let mut graph = Graph::new();
        for name in ["TrueClass", "FalseClass", "NilClass", "String"] {
            graph.declarations_mut().insert(
                DeclarationId::from(name),
                Declaration::Namespace(Namespace::Class(Box::new(ClassDeclaration::new(
                    name.to_owned(),
                    DeclarationId::from("Object"),
                )))),
            );
        }
        let spelling = |name: &str, boolean: bool| {
            let mut answer =
                types::Typed::of(DeclarationId::from(name), types::Derivation::default());
            answer.boolean = boolean;
            typed(&graph, &answer)
        };
        assert_eq!(spelling("TrueClass", true), Some("bool".to_owned()));
        assert_eq!(spelling("TrueClass", false), Some("true".to_owned()));
        assert_eq!(spelling("FalseClass", false), Some("false".to_owned()));
        assert_eq!(spelling("NilClass", false), Some("nil".to_owned()));
        assert_eq!(spelling("String", true), Some("String".to_owned()));
    }

    /// A graph holding one namespace under the key rubydex would have filed it under.
    fn graph_holding(key: &str, namespace: Namespace) -> Graph {
        let mut graph = Graph::new();
        graph
            .declarations_mut()
            .insert(DeclarationId::from(key), Declaration::Namespace(namespace));
        graph
    }

    const KEY: &str = "12345:678<anonymous>";

    #[test]
    fn a_class_ruby_never_named_is_spelled_as_the_call_that_built_it() {
        // `Class.new` with nothing binding it to a constant: rubydex keys it by document and
        // offset, a number an editor would otherwise print straight at the user.
        let graph = graph_holding(
            KEY,
            Namespace::Class(Box::new(ClassDeclaration::new(
                KEY.to_owned(),
                DeclarationId::from("Object"),
            ))),
        );
        assert_eq!(qualified_name(&graph, KEY), "Class.new");
        assert_eq!(
            qualified_name(&graph, &format!("{KEY}#call()")),
            "Class.new#call"
        );
        // Both halves of a singleton owner, which is why every occurrence is replaced, not just the
        // first: one spelling on both sides is what `singleton_parts` recognises, and it turns the
        // pair back into a `.`.
        assert_eq!(
            qualified_name(&graph, &format!("{KEY}::<{KEY}>#call()")),
            "Class.new.call"
        );
        // The picker splits the same name into the same two halves.
        assert_eq!(
            split_qualified(&graph, &format!("{KEY}#call()")),
            ("call".to_owned(), Some("Class.new".to_owned()))
        );
    }

    #[test]
    fn a_module_is_not_spelled_as_a_class() {
        // rubydex spells both the same, and many anonymous namespaces that own methods are modules,
        // so the declaration decides. Assuming would often be wrong.
        let graph = graph_holding(
            KEY,
            Namespace::Module(Box::new(ModuleDeclaration::new(
                KEY.to_owned(),
                DeclarationId::from("Object"),
            ))),
        );
        assert_eq!(qualified_name(&graph, KEY), "Module.new");
        assert_eq!(
            qualified_name(&graph, &format!("{KEY}#call()")),
            "Module.new#call"
        );
    }

    #[test]
    fn a_key_the_graph_does_not_hold_is_still_not_a_number() {
        // A name is rendered from whatever the request holds, and nothing guarantees the owner
        // survived the walk that reached its member. The commoner reading beats the raw key.
        assert_eq!(
            qualified_name(&no_graph(), &format!("{KEY}#call()")),
            "Class.new#call"
        );
    }

    #[test]
    fn a_suffix_with_no_key_in_front_of_it_is_left_alone() {
        // rubydex writes the key and suffix together, so none of these is a name it wrote. They are
        // here because the alternative (walking back over whatever precedes the suffix) swallows a
        // namespace that is really there; the last line proves it does not.
        assert_eq!(
            qualified_name(&no_graph(), "Foo<anonymous>"),
            "Foo<anonymous>"
        );
        assert_eq!(
            qualified_name(&no_graph(), "12:<anonymous>"),
            "12:<anonymous>"
        );
        assert_eq!(
            qualified_name(&no_graph(), ":5<anonymous>"),
            ":5<anonymous>"
        );
        assert_eq!(
            qualified_name(&no_graph(), "Foo::12:3<anonymous>"),
            "Foo::Class.new"
        );
    }

    #[test]
    fn an_anonymous_rest_parameter_is_written_once() {
        // `def initialize(*, **, &)` is ordinary Ruby 3, and rubydex records each of the three
        // under its own sigil, which `format!("**{name}")` would turn into `****`.
        assert_eq!(sigil("*", "*"), "*");
        assert_eq!(sigil("**", "**"), "**");
        assert_eq!(sigil("&", "&"), "&");
        assert_eq!(sigil("**", "options"), "**options");
        // Not a blanket strip: no parameter is really named `*args`, but a name that merely starts
        // with the sigil must not lose it.
        assert_eq!(sigil("*", "*args"), "**args");
    }

    #[test]
    fn a_symbol_list_gets_a_label_and_the_full_path_beside_it() {
        // The label matches the outline's spelling; the container is the *whole* path, because
        // `self.baz` alone does not say which class it hangs off.
        assert_eq!(
            split_qualified(&no_graph(), "Foo::Bar::<Bar>#baz()"),
            ("self.baz".to_owned(), Some("Foo::Bar".to_owned()))
        );
        // A top-level `class << Foo` has no prefix to fall back on, only the attached name.
        assert_eq!(
            split_qualified(&no_graph(), "<Person>#build()"),
            ("self.build".to_owned(), Some("Person".to_owned()))
        );
        assert_eq!(
            split_qualified(&no_graph(), "Person#shout()"),
            ("shout".to_owned(), Some("Person".to_owned()))
        );
        assert_eq!(
            split_qualified(&no_graph(), "Person::MAX_AGE"),
            ("MAX_AGE".to_owned(), Some("Person".to_owned()))
        );
        // Top level: a container of `""` would render as an empty column.
        assert_eq!(
            split_qualified(&no_graph(), "Person"),
            ("Person".to_owned(), None)
        );
    }

    #[test]
    fn magic_comments_are_not_documentation() {
        // Regression guard: rubydex allows one blank line between a comment block and the
        // definition below it, so the pragma at the top of a file attaches to the first class.
        let dropped = comments(&[
            "# frozen_string_literal: true",
            "# typed: strict",
            "# rubocop:disable Style/Documentation",
            "#",
            "# A person.",
        ]);
        assert_eq!(documentation(&dropped).unwrap(), "A person.");

        assert!(documentation(&comments(&["# encoding: utf-8"])).is_none());
        assert!(documentation(&comments(&["#!/usr/bin/env ruby"])).is_none());
        assert!(documentation(&[]).is_none());
    }

    #[test]
    fn rdocs_header_becomes_a_signature_block_instead_of_disappearing() {
        // Exactly the shape rbs core carries, one comment line each.
        let string_upcase = comments(&[
            "# <!--",
            "#   rdoc-file=string.c",
            "#   - upcase(mapping = :ascii) -> new_string",
            "# -->",
            "# Returns a new string containing the upcased characters in `self`.",
        ]);
        assert_eq!(
            documentation(&string_upcase).unwrap(),
            "```ruby\nupcase(mapping = :ascii) -> new_string\n```\n\nReturns a new string \
             containing the upcased characters in `self`."
        );

        // The block forms are the whole reason to keep the call-seq: the RBS signature has the
        // types, and only this spells out `each {|element| ... }`.
        let array_each = comments(&[
            "# <!--",
            "#   rdoc-file=array.c",
            "#   - each {|element| ... } -> self",
            "#   - each -> new_enumerator",
            "# -->",
            "# Iterates over the elements of `self`.",
        ]);
        assert!(
            documentation(&array_each)
                .unwrap()
                .starts_with("```ruby\neach {|element| ... } -> self\neach -> new_enumerator\n```")
        );
    }

    #[test]
    fn an_html_comment_that_is_not_rdocs_header_is_left_alone() {
        // No closer: dropping to the end of the block would eat the documentation. The opener
        // survives as text, escaped, because an HTML comment a renderer *does* understand would
        // silently take the rest of the card away.
        let unclosed = comments(&["# <!--", "# still prose, somehow"]);
        assert_eq!(
            documentation(&unclosed).unwrap(),
            "\\<!--\nstill prose, somehow"
        );
        // And a header with nothing but the file name leaves no stray code block behind.
        let bare = comments(&["# <!--", "#   rdoc-file=string.c", "# -->", "# Prose."]);
        assert_eq!(documentation(&bare).unwrap(), "Prose.");
    }

    #[test]
    fn prose_that_merely_contains_a_colon_survives() {
        assert_eq!(
            documentation(&comments(&["# TODO: explain this", "# Note: it is fine"])).unwrap(),
            "TODO: explain this\nNote: it is fine"
        );
    }

    #[test]
    fn indentation_inside_a_comment_block_is_preserved() {
        // Doc comments carry indented code samples; collapsing them would break the fences.
        assert_eq!(
            documentation(&comments(&["# Example:", "#     Person.new", "#"])).unwrap(),
            "Example:\n    Person.new"
        );
    }

    #[test]
    fn rdocs_html_becomes_the_markdown_that_means_the_same_thing() {
        // A `MarkupContent` is markdown and every client strips HTML from it, so a `<code>` span
        // would reach the user without its markup; the vendored signatures are full of them.
        assert_eq!(to_markdown("<code>:ascii</code>"), "`:ascii`");
        assert_eq!(to_markdown("<tt>nil</tt>"), "`nil`");
        assert_eq!(to_markdown("<em>self</em>"), "*self*");
        assert_eq!(to_markdown("<i>self</i>"), "*self*");
        assert_eq!(to_markdown("<strong>not</strong>"), "**not**");
        assert_eq!(to_markdown("<b>not</b>"), "**not**");
        // Nested, because emphasis around code is how RDoc writes a warning about a method.
        assert_eq!(
            to_markdown("<strong><code>nil</code></strong>"),
            "**`nil`**"
        );
    }

    #[test]
    fn a_tag_that_is_not_markup_keeps_its_angle_brackets() {
        // `<vowel>`, `<rhs>`, `<main>` and a whole `<html>` document all appear in the prose of
        // Ruby's own signatures. A renderer silently eats each one along with everything up to the
        // next `>`.
        assert_eq!(
            to_markdown("matches <vowel> here"),
            "matches \\<vowel> here"
        );
        // An opener with no `>` at all, and a tag ya-lsp knows with no closer.
        assert_eq!(to_markdown("a < b"), "a \\< b");
        assert_eq!(to_markdown("<code>unclosed"), "\\<code>unclosed");
    }

    #[test]
    fn code_is_left_exactly_as_it_was_written() {
        // The escape above must not reach a code sample: `Hash<Symbol, untyped>` in an example is
        // Ruby, and a backslash in front of it is a visible bug.
        assert_eq!(
            to_markdown("Prose <b>bold</b>:\n\n    Hash<Symbol, untyped>\n\n    more <em>x</em>"),
            "Prose **bold**:\n\n    Hash<Symbol, untyped>\n\n    more <em>x</em>"
        );
        // A tab is verbatim too, and a fenced block is verbatim including its fences.
        assert_eq!(to_markdown("\tHash<Symbol>"), "\tHash<Symbol>");
        assert_eq!(
            to_markdown("```ruby\nHash<Symbol>\n```\nafter <em>x</em>"),
            "```ruby\nHash<Symbol>\n```\nafter *x*"
        );
        // And a backtick span is already code, so what is inside it is not markup.
        assert_eq!(
            to_markdown("`Array<Integer>` and <b>b</b>"),
            "`Array<Integer>` and **b**"
        );
        // A stray opener is one character of text, not the start of a span that never ends.
        assert_eq!(to_markdown("a ` b <b>c</b>"), "a ` b **c**");
    }

    #[test]
    fn a_backtick_inside_a_code_tag_gets_a_fence_long_enough_to_hold_it() {
        // `<code>$`</code>` is in Ruby's own signatures (the global holding what preceded a match),
        // and a one-backtick fence would end the span mid-name.
        assert_eq!(to_markdown("<code>$`</code>"), "`` $` ``");
        assert_eq!(to_markdown("<code>`</code>"), "`` ` ``");
        assert_eq!(to_markdown("<code>a`b</code>"), "``a`b``");
    }

    #[test]
    fn rdocs_own_links_go_nowhere_and_are_flattened_to_their_words() {
        // `core/` has many of these, each pointing into a documentation tree the editor has never
        // seen. RDoc wraps them across lines, so the whole prose run is one unit.
        assert_eq!(
            to_markdown("see [Case Mapping](rdoc-ref:case_mapping.rdoc):"),
            "see Case Mapping:"
        );
        assert_eq!(
            to_markdown("see [Case\nMappings](rdoc-ref:case_mapping.rdoc@Case+Mappings)."),
            "see Case\nMappings."
        );
        // The label is prose too.
        assert_eq!(
            to_markdown("[the <code>x</code> form](rdoc-ref:a.rdoc)"),
            "the `x` form"
        );
        // A link the editor *can* follow is not RDoc's problem and is left alone.
        assert_eq!(
            to_markdown("[docs](https://ruby-lang.org)"),
            "[docs](https://ruby-lang.org)"
        );
        // And a bracket that is not a link at all stays a bracket, closed or not.
        assert_eq!(to_markdown("a[0] and b"), "a[0] and b");
        assert_eq!(to_markdown("see [x](rdoc-ref:a"), "see [x](rdoc-ref:a");
    }

    #[test]
    fn a_real_rdoc_comment_comes_out_readable() {
        // Taken from `String#upcase` in the vendored signatures, the shape this conversion exists
        // for: a call-seq header, prose with RDoc's backticks, an indented example, an HTML span
        // and a dead link, in one comment.
        let card = documentation(&comments(&[
            "# <!--",
            "#   rdoc-file=string.c",
            "#   - upcase(mapping = :ascii) -> new_string",
            "# -->",
            "# Returns a new string containing the upcased characters in `self`:",
            "#",
            "#     'hello'.upcase        # => \"HELLO\"",
            "#",
            "# The casing is affected by the given `mapping`, which may be",
            "# <code>:ascii</code>; see [Case",
            "# Mappings](rdoc-ref:case_mapping.rdoc@Case+Mappings).",
        ]))
        .unwrap();
        assert_eq!(
            card,
            "```ruby\nupcase(mapping = :ascii) -> new_string\n```\n\nReturns a new string \
             containing the upcased characters in `self`:\n\n    'hello'.upcase        # => \
             \"HELLO\"\n\nThe casing is affected by the given `mapping`, which may be\n\
             `:ascii`; see Case\nMappings."
        );
    }
}
