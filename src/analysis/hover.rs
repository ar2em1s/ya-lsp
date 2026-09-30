//! `textDocument/hover` — what the thing under the cursor is, as markdown.

use rubydex::model::{
    declaration::Declaration,
    definitions::Definition,
    graph::Graph,
    ids::{DeclarationId, StringId, UriId},
    visibility::Visibility,
};

use super::{
    locator::{self, Resolution},
    render,
    synthesized::{self, Synthesized},
    types,
};

/// The one line a card carries about ya-lsp's confidence: the answer rests on a name alone. The
/// method was matched on its name because the receiver's type is unknown, or the receiver's type,
/// a variable's or a return was read off a name. Which of those is not the reader's question.
const GUESSED: &str = "Guessed from name alone.";

/// Markdown for a resolved cursor, or `None` when there is nothing worth saying.
///
/// # The shape of a card
///
/// **The answer, then ya-lsp's lines, then the source's prose:**
/// 1. A fenced signature: a method with its parameters and `-> T`, a class, or a variable or
///    constant with `: T`.
/// 2. ya-lsp's own lines, one italic line each: [`GUESSED`] and *Defined in N places.* Above the
///    prose, because a class's documentation runs to hundreds of lines and a line under it is
///    never seen.
/// 3. A rule, and the documentation comment a person wrote.
///
/// **A card says what the answer is, never how ya-lsp found it** (decided 2026-09-29): not the
/// signatures a type was derived through, not the file a generator read, not the body a return
/// was read out of. The one tier a reader must act on is a guess, so that is the one line about
/// confidence; a derived answer reads as a resolved one, and the jump shows where either came from.
#[must_use]
pub fn markdown(
    synthesized: &Synthesized,
    modifiers: &locator::Modifiers<'_>,
    sources: &types::Sources<'_>,
    resolution: &Resolution,
    cursor: Option<&str>,
    at_this_call: Option<&types::Typed>,
) -> Option<String> {
    let guessed = !resolution.precise || resolution.derivation.tier() == types::Tier::Guessed;
    match resolution.declarations.as_slice() {
        [] => None,
        [only] => card(
            synthesized,
            modifiers,
            sources,
            *only,
            cursor,
            at_this_call,
            guessed,
        ),
        // Several, exactly: one member on each class the receiver can be (a concern's including
        // classes). Otherwise only the name-based fallback produces more than one, and
        // naming one would present a coin flip as an answer. `definition` lists them either way,
        // so the card only counts.
        many => Some(listed(
            sources.graph,
            many,
            if resolution.precise {
                "definitions"
            } else {
                "possible definitions"
            },
            guessed,
        )),
    }
}

/// The card on an instance variable: the variable, as the graph names it where it declares one
/// (`Shelf::Book#@title`) or as the text spells it, and what it holds where ya-lsp typed it.
///
/// One shape with every other variable's and constant's card (`Owner#@name: T`, RBS's spelling),
/// never the card of the class it holds: hovering `@story` and reading `class Story` does not say
/// the card is about `@story`. `None` where there is neither a declaration to name nor a type a
/// reader can read, so the cursor falls to the rungs below.
#[must_use]
pub fn variable(
    synthesized: &Synthesized,
    sources: &types::Sources<'_>,
    declaration: Option<DeclarationId>,
    written: &str,
    typed: Option<&types::Typed>,
    cursor: Option<&str>,
) -> Option<String> {
    let graph: &Graph = sources.graph;
    let held = typed.and_then(|typed| Some((render::typed(graph, typed)?, typed)));
    let declared = declaration.and_then(|id| Some((id, graph.declarations().get(&id)?)));
    if held.is_none() && declared.is_none() {
        return None;
    }
    let mut notes = Vec::new();
    if held
        .as_ref()
        .is_some_and(|(_, typed)| typed.derivation.tier() == types::Tier::Guessed)
    {
        notes.push(GUESSED.to_owned());
    }
    let mut name = written.to_owned();
    let mut documentation = None;
    if let Some((id, declared)) = declared {
        name = render::qualified_name(graph, declared.name());
        let places = locator::places(graph, synthesized, sources.layout, id, cursor).len();
        if places > 1 {
            notes.push(defined_in(places));
        }
        documentation = written_documentation(synthesized, &locator::definitions_of(graph, id));
    }
    let answer = match held {
        Some((spelled, _)) => format!("{name}: {spelled}"),
        None => name,
    };
    Some(compose(&answer, &notes, documentation.as_deref()))
}

/// The card on a literal key a member looks up: what the main locale holds under it,
/// as the YAML the body of knowledge renders (`Knowledge::keyed_entry`), the key first.
#[must_use]
pub fn keyed(yaml: &str) -> String {
    format!("```yaml\n{yaml}\n```")
}

/// One card: the fenced answer, ya-lsp's lines, and the source's prose under a rule.
///
/// The one place that shape lives, so every card puts what it knows in the same order.
fn compose(answer: &str, notes: &[String], documentation: Option<&str>) -> String {
    let mut card = format!("```ruby\n{answer}\n```");
    for note in notes {
        card.push_str(&format!("\n\n*{note}*"));
    }
    if let Some(documentation) = documentation {
        card.push_str("\n\n---\n\n");
        card.push_str(documentation);
    }
    card
}

/// Reopened classes and monkey-patched methods are the norm in Ruby, and a card cannot show the
/// code that is elsewhere.
///
/// **The count is the list**, asked of the function `definition` answers from
/// ([`locator::places`]): a generated declaration that maps to a line is a place and one that maps
/// to nothing is not; an annotation typing a method the user wrote is the same place, not two; a
/// signature, or a copy the project would not load, is not a place. A second arithmetic would let
/// a card claim a number no jump can produce.
fn defined_in(places: usize) -> String {
    format!("Defined in {places} places.")
}

/// The documentation comment a person wrote above one of these definitions.
///
/// **Only a file's prose**: a generated definition's comment is ya-lsp's note about where the
/// declaration came from, which a card does not say.
fn written_documentation(synthesized: &Synthesized, definitions: &[&Definition]) -> Option<String> {
    definitions
        .iter()
        .filter(|definition| !synthesized.is_generated(definition.uri_id()))
        .find_map(|definition| render::documentation(definition.comments()))
}

/// What a method with no value to show hands back, where that is worth a word on its card: every
/// overload of its signature says `void` or `bot`, or every path through its `def`s raises
/// ([`types::never_returns`]). A reader then knows the call's value is not one to use.
fn nothing(
    sources: &types::Sources<'_>,
    declaration: &Declaration,
    id: DeclarationId,
) -> Option<types::Nothing> {
    if !matches!(declaration, Declaration::Method(_)) {
        return None;
    }
    sources.types.nothing(id).or_else(|| {
        (types::raises(sources, id) && types::never_returns(sources, id))
            .then_some(types::Nothing::Bot)
    })
}

fn card(
    synthesized: &Synthesized,
    modifiers: &locator::Modifiers<'_>,
    sources: &types::Sources<'_>,
    declaration_id: DeclarationId,
    cursor: Option<&str>,
    at_this_call: Option<&types::Typed>,
    guessed: bool,
) -> Option<String> {
    let graph: &Graph = sources.graph;
    let declaration = graph.declarations().get(&declaration_id)?;
    let definitions = locator::definitions_of(graph, declaration_id);

    // **What the method returns, or what the constant holds**, beside the name the reader hovered
    // (decided 2026-09-29): what a signature declares, else the method's body read, else this
    // call's type where neither answers alone, as for an overload the arguments pick.
    // The card says nothing else about the type, so it is always here when ya-lsp has one.
    // **An `initialize`'s own return is no answer**: `new` discards it and hands back
    // the object, so at a `Foo.new` the card says what this call is, and at the `def` nothing.
    // roundhouse's second opinion found `Sponge.new` carded `Sponge#initialize -> Integer`.
    let constructor = declaration.name().ends_with("#initialize()");
    let held = match declaration {
        Declaration::Method(_) if constructor => at_this_call
            .filter(|typed| render::typed(graph, typed).is_some())
            .cloned(),
        Declaration::Method(_) => types::method_return(sources, declaration_id)
            .map(|returned| returned.typed)
            .or_else(|| {
                at_this_call
                    .filter(|typed| render::typed(graph, typed).is_some())
                    .cloned()
            }),
        Declaration::Constant(_) => types::constant_type(sources, declaration_id),
        _ => None,
    };
    // A type read through a guess is a guess: what makes the card agree with a margin
    // that draws nothing here.
    let guessed = guessed
        || held
            .as_ref()
            .is_some_and(|typed| typed.derivation.tier() == types::Tier::Guessed);
    // Through `render`, not off the declaration: a class nothing named is keyed
    // `<id>:<offset><anonymous>`, and a key must never be printed at a reader. `!` where one of the
    // method's own `def`s writes `raise`.
    let spelled = match (declaration, &held) {
        (Declaration::Method(_), Some(typed)) => {
            render::returned(graph, typed, types::raises(sources, declaration_id))
                .map_or_else(String::new, |spelled| format!(" -> {spelled}"))
        }
        (Declaration::Method(_), None) => nothing(sources, declaration, declaration_id)
            .map_or_else(String::new, |nothing| format!(" -> {}", nothing.spelled())),
        (_, Some(typed)) => {
            render::typed(graph, typed).map_or_else(String::new, |spelled| format!(": {spelled}"))
        }
        (_, None) => String::new(),
    };

    // Before the signature, which reads a generated method's parameters off the `def` its place
    // is.
    let places = locator::places(graph, synthesized, sources.layout, declaration_id, cursor);
    let mut notes = Vec::new();
    if guessed {
        notes.push(GUESSED.to_owned());
    }
    // Not on a class or a module: nearly every one is reopened, `module Rails` in 145 places, and
    // the count said nothing a reader acts on.
    if places.len() > 1 && !matches!(declaration, Declaration::Namespace(_)) {
        notes.push(defined_in(places.len()));
    }
    Some(compose(
        &format!(
            "{}{spelled}",
            signature(
                sources,
                modifiers,
                declaration_id,
                declaration,
                &definitions,
                &places
            )
        ),
        &notes,
        written_documentation(synthesized, &definitions).as_deref(),
    ))
}

fn signature(
    sources: &types::Sources<'_>,
    modifiers: &locator::Modifiers<'_>,
    declaration_id: DeclarationId,
    declaration: &Declaration,
    definitions: &[&Definition],
    places: &[locator::Site],
) -> String {
    let graph: &Graph = sources.graph;
    let name = declaration.name();
    match declaration {
        Declaration::Namespace(namespace) => match namespace {
            // `Class.new` is the whole construct. There is no `class Foo` line to echo, so the call
            // stands alone, as `class << Book` does below.
            rubydex::model::declaration::Namespace::Class(_)
            | rubydex::model::declaration::Namespace::Module(_)
                if render::is_anonymous(name) =>
            {
                render::qualified_name(graph, name)
            }
            rubydex::model::declaration::Namespace::Class(_) => format!("class {name}"),
            rubydex::model::declaration::Namespace::Module(_) => format!("module {name}"),
            // `Person::<Person>` is rubydex's name for what the source writes as `class << self`
            // inside `class Person`.
            rubydex::model::declaration::Namespace::SingletonClass(_) => {
                format!("class << {}", attached_name(name))
            }
            rubydex::model::declaration::Namespace::Todo(_) => name.to_owned(),
        },
        Declaration::Method(_) => {
            let method = definitions.iter().find_map(|definition| match definition {
                Definition::Method(method) => Some(method),
                _ => None,
            });
            // An alias takes the parameters of the method it renames, since calling it runs that
            // method: `alias :l :localize` is `l(object, **options)`.
            let parameters = aliased(graph, declaration_id, declaration, definitions)
                .and_then(|target| {
                    let renamed = locator::definitions_of(graph, target);
                    parameters(sources, &renamed, &[])
                })
                .or_else(|| parameters(sources, definitions, places))
                .unwrap_or_default();
            let visibility = match method.map(|method| method.visibility()) {
                Some(Visibility::Public) | None => String::new(),
                // **The record is reread before the word is printed.** A bare `private` inside a
                // block is recorded against every `def` below the block. Trusting it would print
                // *private* over a public method: the false sentence the gate refuses to jump on,
                // arriving in the card instead. See `locator::Modifiers`.
                Some(Visibility::Private | Visibility::ModuleFunction)
                    if !modifiers.confirm(graph, declaration_id) =>
                {
                    String::new()
                }
                Some(other) => format!("{other} "),
            };
            format!(
                "{visibility}{}{parameters}",
                render::qualified_name(graph, name)
            )
        }
        _ => name.to_owned(),
    }
}

/// A method's parameters as its card prints them: the names and defaults a person wrote, where a
/// Ruby `def` says them ([`written_def`]); an RBS signature's own otherwise, where rubydex calls an
/// unnamed one `arg0`. `None` where no definition is a method's.
fn parameters(
    sources: &types::Sources<'_>,
    definitions: &[&Definition],
    places: &[locator::Site],
) -> Option<String> {
    let graph: &Graph = sources.graph;
    if let Some((written, Definition::Method(ruby))) = written_def(graph, definitions, places) {
        return Some(render::parameter_list(
            graph,
            ruby.signatures(),
            &types::written_defaults(sources, written, ruby.offset().start(), ruby.offset().end()),
        ));
    }
    definitions.iter().find_map(|definition| match definition {
        Definition::Method(method) => Some(render::parameter_list(graph, method.signatures(), &[])),
        _ => None,
    })
}

/// The method an alias renames, where the name is nothing but aliases: each agrees on the old
/// name, which is looked up on the alias's class through its ancestors, as Ruby does when the
/// alias runs (`types::renamed`'s rule).
///
/// - **A `def` or a signature of the name is its own**, and says its own parameters.
/// - **ya-lsp's generated row beside an alias is not**: i18n's `l` is `alias :l :localize` plus
///   a row whose first arm (`default:`) exists only to type a call, and printed it as if
///   `default:` were required.
/// - **One step.** An alias of an alias prints what the first one's name has.
fn aliased(
    graph: &Graph,
    declaration_id: DeclarationId,
    declaration: &Declaration,
    definitions: &[&Definition],
) -> Option<DeclarationId> {
    let mut old: Option<&str> = None;
    for definition in definitions {
        let Definition::MethodAlias(alias) = definition else {
            let uri = graph.documents().get(definition.uri_id())?.uri();
            if uri.starts_with(synthesized::GENERATED_SCHEME) {
                continue;
            }
            return None;
        };
        // rubydex's old name carries its parentheses (`size()`); see `Types::adopt_aliases`.
        let name = graph
            .strings()
            .get(alias.old_name_str_id())?
            .as_str()
            .trim_end_matches("()");
        if old.is_some_and(|held| held != name) {
            return None;
        }
        old = Some(name);
    }
    let target = locator::find_member(
        graph,
        *declaration.owner_id(),
        StringId::from(&format!("{}()", old?)),
    )
    .ok()?;
    (target != declaration_id).then_some(target)
}

/// The Ruby `def` a method's parameters are read from, and its document's URI: the method's own
/// where one is Ruby, else the `def` at one of its places, which is what a generated declaration
/// was written from (`ActiveRecord::Base.sanitize_sql_for_order` is ActiveRecord's
/// `def sanitize_sql_for_order(condition)` in `Sanitization::ClassMethods`).
///
/// `None` where neither is Ruby: a signature alone, or a method a generator made from nothing a
/// person wrote.
fn written_def<'g>(
    graph: &'g Graph,
    definitions: &[&'g Definition],
    places: &[locator::Site],
) -> Option<(&'g str, &'g Definition)> {
    let ruby =
        |uri: &str| !uri.ends_with(".rbs") && !uri.starts_with(synthesized::GENERATED_SCHEME);
    let own = definitions.iter().find_map(|definition| {
        let uri = graph.documents().get(definition.uri_id())?.uri();
        (matches!(definition, Definition::Method(_)) && ruby(uri)).then_some((uri, *definition))
    });
    own.or_else(|| {
        places
            .iter()
            .filter(|site| ruby(&site.uri))
            .find_map(|site| {
                let document = graph.documents().get(&UriId::from(site.uri.as_str()))?;
                document
                    .definitions()
                    .iter()
                    .filter_map(|id| graph.definitions().get(id))
                    .find(|definition| {
                        matches!(definition, Definition::Method(_))
                            && locator::spans(definition).1 == site.selection
                    })
                    .map(|definition| (document.uri(), definition))
            })
    })
}

/// `Person::<Person>` -> `Person`, `Shelf::Book::<Book>` -> `Shelf::Book`, `<Person>` -> `Person`.
///
/// The part *before* the `::<`, not inside it. rubydex writes the attached name unqualified in the
/// brackets, so reading it there would give `class << Book` for a class every other card calls
/// `Shelf::Book`. A top-level module hides the difference, since both spellings are the same
/// string.
fn attached_name(name: &str) -> &str {
    match name.rsplit_once("::<") {
        Some((attached, _)) => attached,
        None => name.trim_start_matches('<').trim_end_matches('>'),
    }
}

/// Several declarations as one card: how many, under `**N {heading}**`.
///
/// **No rows** (decided 2026-09-29): `definition` at the same cursor lists every one, with its
/// file, which is where a reader picks. Counted as spelled, so two `Class.new`s nothing named are
/// one.
fn listed(graph: &Graph, declarations: &[DeclarationId], heading: &str, guessed: bool) -> String {
    let mut names: Vec<String> = declarations
        .iter()
        .filter_map(|id| graph.declarations().get(id))
        .map(|declaration| render::qualified_name(graph, declaration.name()))
        .collect();
    names.sort();
    names.dedup();
    let mut card = format!("**{} {heading}**", names.len());
    if guessed {
        card.push_str(&format!("\n\n*{GUESSED}*"));
    }
    card
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::testing::*;

    // `hover::card` is a different function with the same name. The tests want the harness helper,
    // and an explicit import outranks both globs.
    use crate::analysis::testing::card;

    /// A method with no value to hand back says so on its card: every overload of its signature
    /// says `void` or `bot`, or every path through its `def` raises.
    #[test]
    fn a_method_that_hands_back_nothing_says_so() {
        let source = "\
class Gate
  def ping; end
  def stop; end
  def boom
    raise \"no\"
  end
  def maybe
    raise \"no\" if rand
  end
  def mixed; end
end

Gate.new.ping
Gate.new.stop
Gate.new.boom
Gate.new.maybe
Gate.new.mixed
";
        let mut harness = signed(
            &[(
                "core/gate.rbs",
                "class Gate\n  def ping: () -> void\n  def stop: () -> bot\n  \
                 def mixed: () -> void | (Integer) -> bot\nend\n",
            )],
            "",
        );
        let uri = harness.write("app/gate.rb", source);
        harness.index();
        let line = |harness: &mut Harness, needle: &str| {
            card(harness, &uri, source, needle)
                .lines()
                .nth(1)
                .unwrap_or_default()
                .to_owned()
        };
        assert_eq!(line(&mut harness, "ping\nGate"), "Gate#ping -> void");
        assert_eq!(line(&mut harness, "stop\nGate"), "Gate#stop -> bot");
        assert_eq!(line(&mut harness, "boom\nGate"), "Gate#boom -> bot");
        assert_eq!(line(&mut harness, "maybe\nGate"), "Gate#maybe");
        assert_eq!(line(&mut harness, "mixed\n"), "Gate#mixed");
    }

    #[test]
    fn an_initialize_card_says_what_new_hands_back_and_never_its_own_return() {
        // `new` discards what `initialize` returns. Roundhouse's second opinion found
        // `Sponge.new` carded `Sponge#initialize -> Integer`, `@timeout`'s type.
        let source = "\
class Sponge
  def initialize
    @timeout = 10
  end
end

sponge = Sponge.new
";
        let (mut harness, uri) = with_types(source);
        let at_new = card(&mut harness, &uri, source, "new\n");
        assert!(
            at_new.contains("Sponge#initialize") && !at_new.contains("Integer"),
            "{at_new}"
        );
        let at_def = card(&mut harness, &uri, source, "initialize\n");
        assert!(!at_def.contains(" -> "), "{at_def}");
    }

    #[test]
    fn a_return_read_out_of_a_body_that_rested_on_a_guess_says_so() {
        // `LineItem` comes from the parameter's name alone, so the card's return is a
        // guess, whatever else it was read through, and the margin draws nothing for it.
        let source = "\
class LineItem; end

class C
  def helper(line_item)
    line_item
  end

  def t
    helper(1)
  end
end
";
        let mut harness = Harness::new();
        let uri = harness.write("app/c.rb", source);
        harness.index();
        assert_eq!(
            card(&mut harness, &uri, source, "helper(1)"),
            "```ruby\nC#helper(line_item) -> LineItem\n```\n\n*Guessed from name alone.*"
        );
    }

    /// A hover parses its buffer **once** ([`cursor::Parsed`]): the scope walk, the call's type,
    /// the misfiled-call check, the named-symbol checks and the cursor's shape all walk one tree.
    /// Only the first hover over a text parses it again, for its variables' writes, which are held
    /// after that.
    #[test]
    fn a_hover_parses_its_buffer_once() {
        let mut harness = Harness::new();
        let source = "\
class Story
  def title = \"x\"
end

class Shelf
  def show
    story = Story.new
    story.title.size
  end
end
";
        let uri = harness.write("lib/shelf.rb", source);
        harness.index();
        crate::analysis::cursor::parses_taken();
        let cold = harness.hover_at(&uri, source, "title.size");
        let cold_parses = crate::analysis::cursor::parses_taken();
        let warm = harness.hover_at(&uri, source, "Story.new");
        let warm_parses = crate::analysis::cursor::parses_taken();
        assert!(
            cold["contents"]["value"]
                .as_str()
                .is_some_and(|card| card.contains("Story#title")),
            "{cold}"
        );
        assert!(!warm.is_null(), "the second cursor is answered too");
        assert_eq!((cold_parses, warm_parses), (2, 1));
    }

    #[test]
    fn a_singleton_class_hovers_as_the_source_wrote_it() {
        assert_eq!(attached_name("Person::<Person>"), "Person");
        assert_eq!(attached_name("<Person>"), "Person");
        assert_eq!(attached_name("Person"), "Person");
    }

    #[test]
    fn hover_shows_the_signature_and_the_comment_above_it() {
        let (mut harness, uri) = library();
        let hover = harness.hover_at(&uri, LIBRARY, "shout(volume");
        let markdown = hover["contents"]["value"].as_str().expect("markdown");

        assert!(
            markdown.contains("Person#shout(volume = 1, *rest, sep:, &block)"),
            "{markdown}"
        );
        assert!(markdown.contains("Shout it."), "{markdown}");
        assert_eq!(hover["contents"]["kind"], "markdown");
    }

    #[test]
    fn an_anonymous_rest_parameter_hovers_as_ruby_wrote_it() {
        // rubydex records an anonymous `*`, `**` or `&` under the sigil itself, not an empty name,
        // so adding a sigil in `render` would spell `**` as `****`. The pure test in `render` pins
        // the spelling; this pins rubydex's convention, which rubydex may change.
        let mut harness = Harness::new();
        let source = "class Relay\n  def send_on(one, *, k:, **, &)\n  end\nend\n";
        let uri = harness.write("lib/relay.rb", source);
        harness.index();

        let markdown = harness.hover_at(&uri, source, "send_on")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(
            markdown.contains("Relay#send_on(one, *, k:, **, &)"),
            "{markdown}"
        );
    }

    #[test]
    fn every_kind_of_parameter_ruby_has_is_spelled_the_way_it_was_written() {
        // `render::parameter_list` has an arm per `Parameter` variant, including an optional
        // keyword and a forwarding `...`. `def call(retries: 3)` is ordinary Ruby, and hover is the
        // only place a reader learns the argument is optional and what it defaults to: `retries:`
        // and `retries: 3` say different things.
        let mut harness = Harness::new();
        let source = "class Job\n  def call(one, two = 1, *rest, key:, opt: 2, **kw, &blk)\n                        end\n\n  def forward(...)\n  end\nend\n";
        let uri = harness.write("lib/job.rb", source);
        harness.index();

        let markdown = harness.hover_at(&uri, source, "call(one")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(
            markdown.contains("Job#call(one, two = 1, *rest, key:, opt: 2, **kw, &blk)"),
            "{markdown}"
        );

        let forwarding = harness.hover_at(&uri, source, "forward(")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(forwarding.contains("Job#forward(...)"), "{forwarding}");
    }

    #[test]
    fn a_default_is_printed_where_it_fits_on_the_line() {
        // What a `def` writes is what a reader wants to know about an optional argument, up to the
        // point where the signature line stops being one: a long default, or one over several
        // lines, stays `...`.
        let mut harness = Harness::new();
        let source = "class Job\n  def run(short = 1, long = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\", \
                      split = [\n    1,\n  ], key: :low)\n  end\nend\n";
        let uri = harness.write("lib/job.rb", source);
        harness.index();
        assert_eq!(
            card(&mut harness, &uri, source, "run("),
            "```ruby\nJob#run(short = 1, long = ..., split = ..., key: :low)\n```"
        );
    }

    #[test]
    fn an_alias_is_called_with_the_parameters_of_the_method_it_renames() {
        // Calling an alias runs the method it renames, so its card has that method's parameters,
        // defaults included, found on the alias's class through its ancestors. A name two aliases
        // disagree about, or one renaming nothing, has none to borrow, and says so as before.
        let mut harness = Harness::new();
        harness.write(
            "lib/base.rb",
            "class Base
  def put(key, value)
  end
end
",
        );
        let source = "class Shelf < Base
  def fetch(key, fallback = nil)
  end
  \
                      alias get fetch
  alias_method :read, :fetch
  alias save put
  \
                      alias store fetch
  alias store put
  alias lost missing
end
\
                      Shelf.new.get(1)\nShelf.new.read(1)\nShelf.new.save(1, 2)\n\
                      Shelf.new.store(1)\nShelf.new.lost\n";
        let uri = harness.write("lib/shelf.rb", source);
        harness.index();
        for (needle, expected) in [
            ("get(1)", "Shelf#get(key, fallback = nil)"),
            ("read(1)", "Shelf#read(key, fallback = nil)"),
            ("save(1, 2)", "Shelf#save(key, value)"),
            ("store(1)", "Shelf#store\n"),
            ("lost\n", "Shelf#lost\n"),
        ] {
            let shown = card(&mut harness, &uri, source, needle);
            assert!(shown.contains(expected), "{needle}: {shown}");
        }
    }

    #[test]
    fn a_typed_variable_is_the_variable_and_what_it_holds() {
        // The variable's own card, never its type's class: `Box#@v`, typed, with the count of the
        // places it is written, as a method's card counts its `def`s.
        let source = "class Box\n  def initialize\n    @v = \"x\"\n  end\n\n  def b\n    \
                      @v = \"y\"\n  end\n\n  def c\n    @v\n  end\nend\n";
        let (mut harness, uri) = with_signatures(source);
        assert_eq!(
            card(&mut harness, &uri, source, "@v\n  end\nend"),
            "```ruby\nBox#@v: String\n```\n\n*Defined in 2 places.*"
        );
    }

    #[test]
    fn a_singleton_method_hovers_as_ruby_spells_it() {
        // rubydex calls this `Person::<Person>#build()`. Showing that would show the user the
        // index's internals.
        let (mut harness, uri) = library();
        let markdown = harness.hover_at(&uri, LIBRARY, "build(name)")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Person.build(name)"), "{markdown}");
    }

    #[test]
    fn hover_names_every_construct_the_way_ruby_writes_it() {
        // `hover::signature` has an arm per kind of declaration, and each must be asserted.
        // Otherwise a module hovering as `class`, or a private method without its visibility, would
        // ship under a green suite.
        let mut harness = Harness::new();
        let source = "\
# A place to keep things.
module Storage
  LIMIT = 10

  class << self
    # Wipe it.
    def reset
    end
  end

  # Stash a thing.
  private def stash(thing)
  end

  protected def peek
  end
end
";
        let uri = harness.write("app/storage.rb", source);
        harness.index();

        let markdown = |harness: &mut Harness, needle: &str| -> String {
            harness.hover_at(&uri, source, needle)["contents"]["value"]
                .as_str()
                .unwrap_or_else(|| panic!("no hover on {needle:?}"))
                .to_owned()
        };

        let module = markdown(&mut harness, "Storage\n");
        assert!(module.contains("module Storage"), "{module}");
        assert!(module.contains("A place to keep things."), "{module}");

        // rubydex spells this `Storage::<Storage>`, which is not what the file says. The cursor is
        // on `self`, not the keyword: a definition matches its *name* span, which for
        // `class << self` is the receiver, so hover does not fire over `class` either.
        assert!(harness.hover_at(&uri, source, "class << self").is_null());
        let singleton = markdown(&mut harness, "self");
        assert!(singleton.contains("class << Storage"), "{singleton}");

        // The visibility prefix, the main reason to hover a method you did not write: `stash` is
        // callable from inside `Storage` and nowhere else.
        let private = markdown(&mut harness, "stash(thing)");
        assert!(private.contains("private "), "{private}");
        assert!(private.contains("Storage#stash(thing)"), "{private}");
        assert!(private.contains("Stash a thing."), "{private}");

        let protected = markdown(&mut harness, "peek\n");
        assert!(protected.contains("protected "), "{protected}");

        // A constant is neither a namespace nor a method, and has no signature to render.
        let constant = markdown(&mut harness, "LIMIT");
        assert!(constant.contains("Storage::LIMIT"), "{constant}");
    }

    /// Every construct in [`GALLERY`], in source order, with its card drawn under it.
    fn gallery_cards(harness: &mut Harness, uri: &DocUri) -> String {
        [
            "Shelf\n",
            "LIMIT = 10",
            "CAP",
            "Book < Object",
            "@@printed",
            "@title = title",
            "title(upcase:",
            "name title",
            "open(*paths)",
            "self\n",
            "shut",
            "hide",
            "peek",
            "$shelf",
        ]
        .into_iter()
        .map(|needle| {
            let found = harness.hover_at(uri, GALLERY, needle);
            let card = found["contents"]["value"].as_str().unwrap_or("null");
            let drawn: String = card
                .lines()
                .map(|line| {
                    if line.is_empty() {
                        "\n".to_owned()
                    } else {
                        format!("  {line}\n")
                    }
                })
                .collect();
            format!("{}\n{drawn}", needle.trim_end())
        })
        .collect()
    }

    #[test]
    fn every_hover_card_in_one_file_drawn_side_by_side() {
        // The first-ten treatment, for an answer that is not a list. `ANCESTRY` pins ten rows
        // because a ranking is a composition; a hover card is too. Checked one `contains` at a
        // time, no two cards are read side by side, and one card can drift (say, the singleton card
        // dropping its namespace: `class << Book` above `private Shelf::Book#hide`) unnoticed.
        //
        // Pinned whole, and pinned *together*: this catches one card drifting from the others,
        // which per-card assertions cannot see.
        let mut harness = Harness::new();
        let uri = harness.write("app/shelf.rb", GALLERY);
        harness.index();

        assert_eq!(
            gallery_cards(&mut harness, &uri),
            "\
Shelf
  ```ruby
  module Shelf
  ```

  ---

  Everything on a shelf.
LIMIT = 10
  ```ruby
  Shelf::LIMIT
  ```
CAP
  ```ruby
  Shelf::CAP
  ```
Book < Object
  ```ruby
  class Shelf::Book
  ```

  ---

  A thing on it.
@@printed
  ```ruby
  Shelf::Book#@@printed
  ```
@title = title
  ```ruby
  Shelf::Book#@title
  ```
title(upcase:
  ```ruby
  Shelf::Book#title(upcase: false, &block)
  ```

  ---

  What it is called.
name title
  ```ruby
  Shelf::Book#name(upcase: false, &block)
  ```
open(*paths)
  ```ruby
  Shelf::Book.open(*paths)
  ```
self
  ```ruby
  class << Shelf::Book
  ```
shut
  ```ruby
  Shelf::Book.shut
  ```
hide
  ```ruby
  private Shelf::Book#hide
  ```
peek
  ```ruby
  protected Shelf::Book#peek
  ```
$shelf
  ```ruby
  $shelf
  ```
"
        );
    }

    /// A `Class.new` and a `Module.new` with nothing binding either to a constant.
    const UNNAMED: &str = "\
helper = Class.new do
  def hop(a)
    self
  end
end

another = Class.new do
  def hop(a)
  end
end

mixin = Module.new do
  def hop(a)
  end
end

class Alpha
  def hop(a)
  end
end

class Beta
  def hop(a)
  end
end

anything.hop(1)
";

    #[test]
    fn a_class_ruby_never_named_hovers_as_the_call_that_built_it() {
        // `Class.new` is an expression, so what it builds has no name until something binds it to a
        // constant. Where nothing does, rubydex keys it by document and offset. A key is not a
        // name, and no card may print one at the user. Such classes are common in real apps, and
        // they often own methods a cursor can reach.
        let mut harness = Harness::new();
        let uri = harness.write("app/unnamed.rb", UNNAMED);
        harness.index();

        // The class itself, which `self` inside its body is how a cursor reaches.
        assert_eq!(
            card(&mut harness, &uri, UNNAMED, "self\n"),
            "```ruby\nClass.new\n```"
        );
        // The method, from its own `def`, and its return, which the body rung reads from the `self`
        // in it. That return is the anonymous class too, so the label goes through `render` as
        // well, or the key would appear twice on one line.
        assert_eq!(
            card(&mut harness, &uri, UNNAMED, "hop(a)\n    self"),
            "```ruby\nClass.new#hop(a) -> Class.new\n```"
        );
        // And a module, which rubydex spells exactly like the class, so the declaration is asked,
        // not assumed. In real apps most of these are modules, so a guess would usually be wrong.
        assert_eq!(
            card(
                &mut harness,
                &uri,
                UNNAMED,
                "hop(a)\n  end\nend\n\nclass Alpha"
            ),
            "```ruby\nModule.new#hop(a)\n```"
        );
    }

    #[test]
    fn a_list_of_candidates_counts_each_name_a_reader_could_look_up_once() {
        // Five declarations of `hop`, two in classes Ruby never named. The card lists none of them
        // (`definition` at the same cursor does, with their files) and counts them as spelled, so
        // the two `Class.new`s are one: the count is what the jump's list will show.
        let mut harness = Harness::new();
        let uri = harness.write("app/unnamed.rb", UNNAMED);
        harness.index();

        assert_eq!(
            card(&mut harness, &uri, UNNAMED, "hop(1)"),
            "**4 possible definitions**\n\n*Guessed from name alone.*"
        );
    }

    #[test]
    fn a_reopened_class_is_not_counted_and_a_reopened_method_is() {
        // Nearly every class is reopened somewhere (`module Rails` in 145 places), so the count on
        // a namespace said nothing a reader acts on (decided 2026-09-29). A method written twice is
        // one a reader needs to know is also written elsewhere.
        let (mut harness, uri) = library();
        let class = harness.hover_at(&uri, LIBRARY, "Person\n  MAX_AGE")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(class.contains("class Person"), "{class}");
        assert!(class.contains("Someone with a name."), "{class}");
        assert!(!class.contains("Defined in"), "{class}");
    }

    #[test]
    fn a_core_method_hovers_as_rdoc_written_in_markdown() {
        // The whole card, not a `contains`. A hover card is a composition, and asserting parts one
        // `contains` at a time is how the two shapes of one answer (a guessed single match and a
        // guessed list) drift apart.
        //
        // Pinned along the way:
        // - `<code>self</code>` reaches the user as markdown, not a span a client silently eats;
        // - `[Case Mapping](rdoc-ref:…)` loses a link that goes nowhere but keeps its words;
        // - the call-seq is lifted out of RDoc's HTML header as Ruby;
        // - the indented example survives untouched.
        //
        // **No footnote at all.** `greeting` is a local assigned a string literal. Completion types
        // it exactly, and so does hover, through the same `types::method_receiver`, instead of
        // matching on the name. It is not *derived* either: a literal assigned one line up is code
        // the reader can see.
        let source = "greeting = \"hello\"\ngreeting.upcase\n";
        let (mut harness, uri) = with_signatures(source);
        assert_eq!(
            card(&mut harness, &uri, source, "upcase"),
            "```ruby\n\
             String#upcase(mapping = ...) -> String\n\
             ```\n\
             \n\
             ---\n\
             \n\
             ```ruby\n\
             upcase(mapping = :ascii) -> new_string\n\
             ```\n\
             \n\
             Returns a new string containing `self`'s upcased characters:\n\
             \n\
             \u{20}   'hello'.upcase # => \"HELLO\"\n\
             \n\
             See Case Mapping."
        );
    }

    #[test]
    fn a_stdlib_method_hovers_the_same_way_a_core_one_does() {
        // A different directory under the rbs root, same card. `<tt>` is RDoc's other spelling of
        // `<code>` and appears throughout the vendored signatures; it must not leak either. No
        // footnote, for the same reason as above: `parser = OptionParser.new` is a receiver the
        // code names.
        let source = "parser = OptionParser.new\nparser.parse!\n";
        let (mut harness, uri) = with_signatures(source);
        assert_eq!(
            card(&mut harness, &uri, source, "parse!\n"),
            "```ruby\n\
             OptionParser#parse!(argv = ...)\n\
             ```\n\
             \n\
             ---\n\
             \n\
             ```ruby\n\
             parse!(argv = default_argv) -> argv\n\
             ```\n\
             \n\
             Parses `argv` in place and returns what is left of it."
        );
    }

    #[test]
    fn every_shape_of_card_puts_what_it_knows_in_the_same_place() {
        // The four cards side by side: the only way to see the convention. The answer first, then
        // ya-lsp's one line about it, then the documentation.
        // - A precise hit says nothing extra, however it was found.
        // - A reopened class says nothing either: the count is a method's.
        // - A guess says it is a guess.
        // - A guess with several candidates says the same line, under the count.
        let source =
            "class Radio\n  def shout; end\nend\n\nPerson.build(\"x\")\nthing.shout\nthing.extra\n";
        let (mut harness, uri) = with_signatures(source);

        // Precise: a constant receiver is the one thing rubydex names without inference. The return
        // is the body rung's, which the card does not say: how a type was found is not the
        // reader's question.
        assert_eq!(
            card(&mut harness, &uri, source, "build("),
            "```ruby\nPerson.build(name) -> Person\n```\n\n---\n\nBuild one."
        );

        // Reopened, and a class: no count.
        assert_eq!(
            card(&mut harness, &uri, source, "Person.build"),
            "```ruby\nclass Person\n```\n\n---\n\nSomeone with a name.\n\nReopened below."
        );

        // One name-based match: a whole card, and the caveat under it.
        assert_eq!(
            card(&mut harness, &uri, source, "extra"),
            "```ruby\nPerson#extra\n```\n\n*Guessed from name alone.*"
        );

        // Several: a list, with the same caveat in the same place. Naming one would present a coin
        // flip as an answer.
        assert_eq!(
            card(&mut harness, &uri, source, "shout\n"),
            "**2 possible definitions**\n\n*Guessed from name alone.*"
        );
    }

    #[test]
    fn a_guessed_card_says_so_in_one_line_whatever_was_guessed() {
        // Four guesses that once had four sentences: a class that has no such method, a class
        // object that has none, a receiver whose class was read off its name, and a constant no
        // file defines. Which one it was is not the reader's question; that it is a guess is.
        //
        // `Radio` is a module so that `Person.dial` has a candidate at all: a class object could
        // reach a module's method through an `extend`, never a class's instance method.
        let source = "module Radio\n  def tune; end\n  def dial; end\n  def amp; end\n  \
                      def hum; end\nend\n\n\
                      person = Person.new(\"x\")\nperson.tune\nPerson.dial\n@person.amp\n\
                      gadget = Unknown.new\ngadget.hum\n";
        let (mut harness, uri) = with_signatures(source);
        for (needle, member) in [
            ("tune\nPerson", "Radio#tune"),
            ("dial\n@person", "Radio#dial"),
            ("amp\n", "Radio#amp"),
            ("hum\n", "Radio#hum"),
        ] {
            assert_eq!(
                card(&mut harness, &uri, source, needle),
                format!("```ruby\n{member}\n```\n\n*Guessed from name alone.*")
            );
        }
    }

    #[test]
    fn hover_reads_a_method_that_no_def_wrote() {
        // `attr_reader :name` declares a method whose definition is not a `Definition::Method`, so
        // the signature lookup finds no parameters or visibility. It must still name the method
        // instead of falling through to a bare string: `attr_reader` declares a large share of a
        // Rails app's methods.
        let (mut harness, uri) = library();
        let markdown = harness.hover_at(&uri, LIBRARY, "name\n\n  # Build")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Person#name"), "{markdown}");
        assert!(
            !markdown.contains('('),
            "no parameter list to render: {markdown}"
        );
    }
}
