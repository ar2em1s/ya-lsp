//! `textDocument/inlayHint`: a derived type, drawn without being asked for.
//!
//! The one request in the crate that shows an answer nobody requested, and that changes what may be
//! said. A hover card is read after a deliberate keystroke, has room for a footnote, and is
//! understood as ya-lsp's opinion. A hint is painted into the margin of every line, wanted or not,
//! has room for nothing, and is read as fact. So the tier decides what may be drawn here, not just
//! how it is labelled.
//!
//! # The three tiers, and the one that cannot occur here
//!
//! - **Guessed**: matched on a name alone. **Never drawn.** A name-matched type painted beside
//!   every method in the file is exactly the failure this project is built against, and no label is
//!   small enough to fix it. The refusal tests [`Tier`], not a list of shapes, so the next rung
//!   added below the graph is refused by the same line instead of appearing in everybody's margin
//!   the day it ships.
//! - **Derived**: a signature, an assignment or a convention was followed. Drawn, with the footnote
//!   in the tooltip, the only room a hint has for one.
//! - **Resolved**: the code names the type. **Unreachable, by construction**, and that is the one
//!   thing worth knowing about this module.
//!
//! A hint exists exactly where the code does *not* state the type. Where it does (`x = 1`,
//! `x = Foo.new`, `x = Foo`), the label would repeat a word on the line, and [`worth_saying`]
//! refuses it as noise. Those are exactly the bindings whose type is resolved: **a type the code
//! states is a type the margin need not repeat.**
//!
//! So every drawn hint is derived and has a tooltip, and there is no second kind on screen to tell
//! it apart from, which is why labels carry no marker. `inlayHint/resolve` makes the tooltip cheap,
//! not rare: the sentence is built only for the hint somebody points at, not for every line on
//! every scroll.
//!
//! # Three families, and why not a fourth
//!
//! A block parameter, a local assigned from a call, and a method's declared return. What they share
//! is the paragraph above: the line does not already say the type. A server that draws noise gets
//! turned off, and then the three useful families are gone too.

use std::collections::HashMap;

use ruby_prism::{DefNode, Visit};
use rubydex::model::{
    definitions::Definition,
    graph::Graph,
    ids::{DeclarationId, UriId},
};

use crate::workspace::{DocUri, config::HintsConfig};

use super::{
    cursor::{self, Binding, Receiver},
    hover, locator, render,
    synthesized::generated_prefix,
    types::{self, Derivation, Sources, Tier},
};

/// Which family a hint belongs to, and so which setting silences it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// `stories.each do |story|` — what the called method says its block receives.
    BlockParameter,
    /// `author = story.author` — what the call on the right hands back.
    Local,
    /// `def title` — what a signature says the method returns.
    Return,
}

/// Why a label is the type it is, in the shape a tooltip is rendered from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Because {
    /// A receiver ya-lsp typed, and what it followed to get there. Empty means the resolved tier:
    /// the code named the type and nothing was followed.
    ///
    /// **Boxed** because of the other variant: a [`Derivation`] carries a footnote's worth of text
    /// for every rung (mostly `None` on any one answer), while the other variant carries nothing.
    /// Hints are built one per binding across a visible range, so the enum's size is paid per
    /// label, not only by the few that followed anything.
    Followed(Box<Derivation>),
    /// A signature declares what the method returns, and Ruby has no syntax for saying so.
    ///
    /// Always the derived tier, honestly, not as a shortcut. A signature is a *claim* about a
    /// method: nothing checks it unless a type checker runs, and where core RBS is overridden by
    /// the `def` under the label, it is a claim about a method no longer there.
    /// [`annotations`](super::annotations) makes the same argument about the two hand-written
    /// syntaxes.
    Declared,
}

/// One label, and everything needed to draw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub family: Family,
    /// Where the label goes, as a byte offset into the text this was read from.
    pub at: u32,
    /// The label exactly as it is drawn, separator and footnote marker included.
    pub label: String,
    pub because: Because,
}

impl Hint {
    /// Which tier this answer is. [`Tier::Guessed`] never reaches here; see the module docs.
    #[must_use]
    pub fn tier(&self) -> Tier {
        match &self.because {
            Because::Followed(derivation) => derivation.tier(),
            Because::Declared => Tier::Derived,
        }
    }

    /// The offset of the instance-variable assignment this type came from, if any.
    ///
    /// Returned as an offset, not rendered, for [`hover::markdown`]'s reason: an offset is only a
    /// line once you have its text, and the caller drawing the tooltip holds it.
    #[must_use]
    pub fn assignment(&self) -> Option<u32> {
        match &self.because {
            Because::Followed(derivation) => derivation.assignment,
            Because::Declared => None,
        }
    }

    /// The footnote the marker promised, or `None` where the label carries no marker.
    #[must_use]
    pub fn note(&self, assignment_line: Option<u32>) -> Option<String> {
        match &self.because {
            Because::Followed(derivation) => {
                let notes = hover::provenance(derivation, assignment_line);
                (!notes.is_empty()).then(|| notes.join("\n\n"))
            }
            Because::Declared => Some(DECLARED.to_owned()),
        }
    }
}

/// What a return hint's tooltip says.
///
/// Deliberately not one of `hover`'s lines: those all answer "what did ya-lsp follow to type the
/// *receiver*", and this answers a different question about a different thing: what the method
/// itself was declared to return, by a file other than the one being read.
const DECLARED: &str = "Return type taken from a signature — what the method is declared to \
                        return, not what this body was read to return.";

/// Every hint for the part of `source` inside `within`, in source order.
///
/// `within` is the range the editor has on screen, and it bounds the *work*, not the answer;
/// [`cursor::bindings_in`] takes it for that reason. One parse either way; the range saves every
/// graph lookup after it.
///
/// **The buffer's offsets are the graph's here, by the request, not by assumption.**
/// `textDocument/inlayHint` is not one of the three requests that answer between a keystroke and
/// its index, so `Analysis::serve` has settled first and the two texts are one string, as for
/// `documentHighlight` and `references`.
#[must_use]
pub fn of(
    sources: &Sources<'_>,
    uri: &DocUri,
    source: &str,
    within: (u32, u32),
    shown: &HintsConfig,
) -> Vec<Hint> {
    let uri_id = UriId::from(uri.as_str());
    // Walked once per document, asked once per binding. `Scope::at` walks every definition in the
    // document, so asking it per candidate is quadratic in the file; see [`types::Scope::bodies`],
    // which exists for callers with many cursors, like this one. A memo instead of one walk,
    // because the body rung reads other documents and needs the same question answered about each.
    let walked = types::Walked::new();
    // The second per-hint cost this avoids: the body rung reads a method's body from the document
    // that declares it, once per `def`. Without the memo, each ask would re-read and re-parse the
    // whole document; see `Sources::read_bodies`.
    let read_bodies = types::ReadBodies::new();
    // Both handed down, because `method_receiver` has an arm that places an offset of its own (a
    // captured `self`) and would otherwise redo the hoisted walk once per hint.
    let sources = &Sources {
        walked: Some(&walked),
        read_bodies: Some(&read_bodies),
        ..*sources
    };
    let mut hints: Vec<Hint> = cursor::bindings_in(source, within)
        .into_iter()
        .filter(|bound| match bound.binding {
            Binding::BlockParameter => shown.block_parameters,
            Binding::Local => shown.locals,
        })
        .filter(|bound| worth_saying(&bound.was))
        .filter_map(|bound| {
            let scope = sources.scope_at(uri_id, bound.name.0);
            let typed = types::method_receiver(sources, uri_id, &bound.was, &scope)?;
            Some(Hint {
                family: match bound.binding {
                    Binding::BlockParameter => Family::BlockParameter,
                    Binding::Local => Family::Local,
                },
                at: bound.name.1,
                label: label(sources.graph, &typed)?,
                because: Because::Followed(Box::new(typed.derivation)),
            })
        })
        .collect();

    if shown.returns {
        hints.extend(returns(sources, uri, uri_id, source, within));
    }
    // The bottom tier is refused here, once, after every family has answered: a test on the tier,
    // never on the shape that produced it. See the module docs.
    hints.retain(|hint| hint.tier() != Tier::Guessed);
    hints.sort_by_key(|hint| hint.at);
    hints
}

/// Whether a binding's shape says anything its line does not already say.
///
/// The whole "not every local" rule, and it is about *shape*, not type: `x = Foo.new`, `x = Foo`,
/// `x = "s"` and `x = 1` all name their class in the assignment, and a label repeating it is noise
/// on an already clear line. What remains is a call whose return nothing writes down, which is
/// worth a margin.
///
/// A block parameter is always worth saying (nothing about `|story|` says what it holds), and it
/// arrives here as the one shape that can only come from a signature.
fn worth_saying(was: &Receiver) -> bool {
    match was {
        Receiver::Returned { .. } | Receiver::Yielded { .. } => true,
        // The two wrappers are not shapes of their own: an instance variable carries where it was
        // written and a spelled local carries its name, and either is worth what it wraps.
        Receiver::Assigned { was, .. } | Receiver::Spelled { was, .. } => worth_saying(was),
        // One position of a tuple, worth exactly what its call is worth:
        // `read_io, write_io = IO.pipe` names no class on its line either. Without this arm, the
        // wrap `cursor::bindings_in` applies would read as "not worth saying", and every
        // destructured target would go unlabelled, including those the tuple table answers.
        Receiver::Destructured { of, .. } => worth_saying(of),
        _ => false,
    }
}

/// A label for the declaration a type resolved to, or `None` where there is nothing to draw.
///
/// **Classes and modules only.** A singleton class is a *class object*'s type, which Ruby cannot
/// spell (`Foo::<Foo>` is rubydex's name, `singleton(Foo)` RBS's), and a `Namespace::Todo` is a
/// name nothing defines, so a label from one points at nothing a reader could check. Both are types
/// this module cannot spell exactly, the same test [`annotations`](super::annotations) applies to
/// what it declares.
fn label(graph: &Graph, typed: &types::Typed) -> Option<String> {
    Some(format!(": {}", render::typed(graph, typed)?))
}

/// Every `def` in `within` whose return a signature declares and the source does not.
///
/// Two halves that agree by construction: the anchor comes from Prism (the end of the parameter
/// list, the only place ` -> String` reads as Ruby), and the declaration from the graph, joined by
/// the name span both parsers report for the same bytes.
fn returns(
    sources: &Sources<'_>,
    uri: &DocUri,
    uri_id: UriId,
    source: &str,
    within: (u32, u32),
) -> Vec<Hint> {
    let graph = sources.graph;
    let Some(document) = graph.documents().get(&uri_id) else {
        return Vec::new();
    };
    let methods: HashMap<(u32, u32), DeclarationId> = document
        .definitions()
        .iter()
        .filter_map(|id| graph.definitions().get(id))
        .filter(|definition| matches!(definition, Definition::Method(_)))
        .filter_map(|definition| {
            let name = definition.name_offset()?;
            Some((
                (name.start(), name.end()),
                *graph.definition_to_declaration_id(definition)?,
            ))
        })
        .collect();
    // The documents whose declarations were written *here*: a Sorbet `sig` or YARD `@return` two
    // lines up is the source stating the return type, so repeating it in the margin is the noise
    // this third family is defined against. A prefix, not one URI, because one source writes one
    // generated document per body, and the prefix is exact thanks to the trailing `#` in
    // `generated_prefix`.
    let written_here = generated_prefix(uri.as_str());

    let parsed = ruby_prism::parse(source.as_bytes());
    let mut walk = Defs {
        within,
        found: Vec::new(),
    };
    walk.visit(&parsed.node());

    walk.found
        .into_iter()
        .filter_map(|(name, at)| {
            if value_is_discarded(&source[name.0 as usize..name.1 as usize]) {
                return None;
            }
            let id = *methods.get(&name)?;
            if locator::definitions_of(graph, id).iter().any(|definition| {
                graph
                    .documents()
                    .get(definition.uri_id())
                    .is_some_and(|document| document.uri().starts_with(&written_here))
            }) {
                return None;
            }
            // `Return::Same` is the receiver, which at a `def` is the class the label is already
            // written inside; the two query-interface sentinels are names no file declares. None of
            // the three is a class to draw; see `types::ELEMENT`.
            let (declared, because) = match sources.types.declared_return(id) {
                // A class, or the boolean pair; either way the facets the signature declared travel
                // onto the label (`types::Typed::declared_as`).
                Some(returns) => (
                    types::Typed::declared_as(graph, returns)?,
                    Because::Declared,
                ),
                // **The half about the method, not a receiver.** Nothing declares what an
                // application's own `def` returns, so this reads the body instead. It is the same
                // [`types::body_return`] a chain reaches through `from_body`, so a margin and a
                // card cannot disagree, and it arrives as [`Because::Followed`], so the tier rule
                // below still holds: a body whose exit was guessed is guessed, and a guess is never
                // painted into a margin.
                None => {
                    let typed = types::body_return(sources, id)?;
                    let because = Because::Followed(Box::new(typed.derivation.clone()));
                    (typed, because)
                }
            };
            Some(Hint {
                family: Family::Return,
                at,
                label: format!(" -> {}", render::typed(graph, &declared)?),
                because,
            })
        })
        .collect()
}

/// Whether Ruby's own call syntax throws this method's return value away.
///
/// Two cases, one rule, and the rule is about the **language**, not the body: for these, the
/// expression a caller writes does not evaluate to what the method returned, so a margin stating it
/// would be true but unreachable.
///
/// - **`initialize`.** Nobody calls it directly: `Report.new` allocates, runs it, **discards its
///   value** and returns the object. Keying on the name is not invented here: Ruby makes this one
///   private by name at definition, which is why `completion::ALWAYS_PRIVATE` already lists it.
/// - **A setter.** `obj.title = v` evaluates to `v` whatever `def title=` returns, and so does
///   `obj[k] = v`, with no exceptions. The name ends in `=` and the byte before is not one of
///   `=<>!`, which is exactly Ruby's grammar for `title=` versus `==`, `!=`, `<=`, `>=`, `===`.
///
/// **This is the margin's rule, not the module's.** `types::body_return` still answers for both, so
/// a hover card on a setter still says what the body returns, and a chain through one still steps.
/// The refusal lives here because a hint is the answer nobody asked for and has a higher bar than a
/// card. Constructors hit it most, because their last line is usually an assignment, which returns
/// its value.
///
/// The setter rule is permanent: the honest label would be the argument's type, which nothing
/// declares.
fn value_is_discarded(name: &str) -> bool {
    if name == "initialize" {
        return true;
    }
    match name.as_bytes() {
        [.., before, b'='] => !matches!(before, b'=' | b'<' | b'>' | b'!'),
        _ => false,
    }
}

/// Where a `def`'s return label goes, for every `def` whose name starts inside the range.
struct Defs {
    within: (u32, u32),
    /// The name span, and the offset the label is drawn at.
    found: Vec<((u32, u32), u32)>,
}

impl<'pr> Visit<'pr> for Defs {
    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        let name = node.name_loc();
        let name = (name.start_offset() as u32, name.end_offset() as u32);
        // Past the closing parenthesis if there is one, past the parameters if written without any,
        // and past the name if there are none: `def title(a)`, `def title a` and `def title` all
        // end their signature differently, and a label at the name would land inside the first
        // one's parameter list.
        let at = node
            .rparen_loc()
            .map(|paren| paren.end_offset() as u32)
            .or_else(|| {
                node.parameters()
                    .map(|parameters| parameters.location().end_offset() as u32)
            })
            .unwrap_or(name.1);
        // From the name to where the label goes: the span this `def` occupies as far as a hint is
        // concerned (see [`cursor::overlaps`]). Testing the name alone would miss a `def` whose
        // label is in the window and whose name is one line above it.
        if cursor::overlaps((name.0, at), self.within) {
            self.found.push((name, at));
        }
        ruby_prism::visit_def_node(self, node);
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::testing::*;

    /// What a generic was written holding, drawn beside its head.
    ///
    /// `Typed` carries a generic's positions, and this is where the margin spells them:
    /// `"a,b".scan(",")` and `[1, 2].map { |n| "x" }` draw `Array[String]`, not a bare `Array`.
    #[test]
    fn a_generic_is_drawn_holding_what_it_was_written_holding() {
        let source = "\
class Ledger
  def use
    parts = \"a,b\".scan(\",\")
    names = [1, 2].map { |n| \"x\" }
    blanks = [1, 2].map { }
    keys = { \"a\" => 1 }.keys
    table = \"hi\".table
    mixed = [1, \"x\"].first(1)
    plain = \"hi\".upcase
  end
end
";
        // A position the harvest refused is `untyped` and keeps its place; a blank would break the
        // rule, because `Hash[untyped]` would read as a hash of one thing.
        let (mut harness, uri) = with_declared_types(
            source,
            "class String\n  def table: () -> Hash[String, untyped]\nend\n",
        );
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def use -> String
    parts: Array[String] = \"a,b\".scan(\",\")
    names: Array[String] = [1, 2].map { |n| \"x\" }
    names = [1, 2].map { |n: Integer| \"x\" }
    blanks: Array[nil] = [1, 2].map { }
    keys: Array[String] = { \"a\" => 1 }.keys
    table: Hash[String, untyped] = \"hi\".table
    mixed: Array = [1, \"x\"].first(1)
    plain: String = \"hi\".upcase"
        );
    }

    /// A workspace whose signatures are [`TYPED_RBS`] plus `rbs`, holding one Ruby file.
    ///
    /// [`with_types`]'s shape with a second half: the hint families read what a signature says
    /// about the *user's own* classes, which nothing that types core receivers can stand in for.
    fn with_declared_types(source: &str, rbs: &str) -> (Harness, DocUri) {
        with_declared_types_and_config(source, rbs, "")
    }

    fn with_declared_types_and_config(source: &str, rbs: &str, config: &str) -> (Harness, DocUri) {
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(
            signatures.join("core/core.rbs"),
            format!("{TYPED_RBS}\n{rbs}"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            format!(
                "[gems]\nenabled = false\n\n[rbs]\npath = {:?}\n{config}",
                signatures.display().to_string()
            ),
        )
        .unwrap();

        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        let uri = harness.write("app/report.rb", source);
        harness.index();
        harness.index_gems();
        (harness, uri)
    }

    const HINTS: &str = "\
class Person
  def shout
  end
end

class Report
  def initialize
    @title = \"hi\".upcase
  end

  def headline
  end

  def bindings(first, second)
    plain = \"hi\"
    derived = \"hi\".upcase
    chained = \"hi\".upcase.length
    made = Report.new
    guessed = person.shout
    from_ivar = @title.length
    bare = @title
    \"hi\".bytes do |byte|
      byte
    end
    each do |unyielded|
      unyielded
    end
  end

  def label name
  end

  def chainable
  end

  def slug=(value)
    @slug = value.to_s
  end
end
";

    /// How many bindings [`HINTS`] holds, whether or not each is worth saying.
    ///
    /// Seven locals and two block parameters, all inside `def bindings`. Not the hint count (most
    /// are refused by [`worth_saying`] or by tier) but the number of candidates a whole-document
    /// request classifies, which `hints_answer_for_the_range_they_were_asked_about` counts.
    const BINDINGS_IN_HINTS: usize = 9;

    const HINTS_RBS: &str = "\
class Person
  def shout: () -> String
end

class Report
  def headline: () -> String
  def bindings: (untyped first, untyped second) -> Integer
  def label: (untyped name) -> String
  def chainable: () -> self
end
";

    #[test]
    fn every_inlay_hint_in_one_file_drawn_side_by_side() {
        // `GALLERY`'s treatment for the answer nobody asks for. Read the margin down the page: what
        // is *absent* is as much the assertion as what is drawn, and the absences come from
        // different rules.
        //
        // - `plain = "hi"` and `made = Report.new` state the class on the line. A label repeating
        //   it is noise, and those are exactly the resolved bindings, which is why no hint here is
        //   resolved.
        // - `guessed = person.shout` types to `Person` from six letters and nothing else. Its card
        //   says so and is read on purpose; the margin refuses it outright.
        // - `each do |unyielded|` writes no receiver, so it is a `yield`, not a call, and what a
        //   `yield` hands over comes from the method body, not a signature. Nothing to read.
        // - `def initialize` and `def slug=` both have a readable body (each ends in an assignment,
        //   which returns its value), and neither draws, because Ruby's call syntax discards that
        //   value: `Report.new` returns the object, and `report.slug = v` returns `v`.
        //   `value_is_discarded` is the rule, and only the margin's: a card over either still shows
        //   what the body returns.
        // - `def chainable` is declared `-> self`, which at the `def` means the class the label
        //   would be written inside. Repeating the line above is the first rule again, one
        //   construct out.
        let (mut harness, uri) = with_declared_types(HINTS, HINTS_RBS);

        assert_eq!(
            drawn_hints(HINTS, &harness.hints_in(&uri)),
            "  def shout -> String
  def headline -> String
  def bindings(first, second) -> Integer
    derived: String = \"hi\".upcase
    chained: Integer = \"hi\".upcase.length
    from_ivar: Integer = @title.length
    bare: String = @title
    \"hi\".bytes do |byte: Integer|
  def label name -> String"
        );
    }

    #[test]
    fn a_type_matched_on_a_name_alone_is_never_drawn_in_the_margin() {
        // The refusal, asserted **by tier, not by example**: the same expression is asked twice, as
        // a card and as a label. The card names `Person#shout` and says in its last line what that
        // rests on (the six letters of `person`), because a card is read on purpose and has room to
        // qualify itself. The margin has no room, is read as fact, and is drawn on every line, so
        // the bottom tier never appears in it.
        let (mut harness, uri) = with_declared_types(HINTS, HINTS_RBS);

        let card = harness.hover_at(&uri, HINTS, "shout\n    from");
        let card = card["contents"]["value"].as_str().unwrap_or("null");
        assert!(card.contains("Person#shout"), "{card}");
        assert!(
            card.contains("Type guessed from the name `person` alone"),
            "{card}"
        );

        assert!(
            !drawn_hints(HINTS, &harness.hints_in(&uri)).contains("guessed"),
            "a guess reached the margin"
        );
    }

    #[test]
    fn a_mailers_template_is_a_margin_the_renderer_rung_reaches() {
        // `inlayHint` needs nothing extra for a template, so the view↔renderer rung (the controller
        // a template's directory names, or the mailer where there is no controller) decides what is
        // painted into the margin. That rung widening to mailer views adds answers here too, and a
        // template's card is what a label here is built from.
        //
        // The population moves, not the tier. Every such answer is a convention, therefore
        // *Derived*, which is already drawn; the refusal below is the same test on [`Tier`], in the
        // same file and request.
        let dir = tempfile::tempdir().expect("tempdir");
        let signatures = dir.path().join("sig");
        std::fs::create_dir_all(signatures.join("core")).unwrap();
        std::fs::write(
            signatures.join("core/core.rbs"),
            format!(
                "{TYPED_RBS}\nclass Story\n  def headline: () -> String\nend\n\n\
                 class Person\n  def shout: () -> String\nend\n"
            ),
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
            "app/models/story.rb",
            "class Story\n  def headline\n  end\nend\n",
        );
        harness.write(
            "app/models/person.rb",
            "class Person\n  def shout\n  end\nend\n",
        );
        harness.write(
            "app/mailers/user_mailer.rb",
            "class UserMailer < ApplicationMailer\n  def digest\n    @story = Story.new\n  \
             end\nend\n",
        );
        let template = "<% headline = @story.headline %>\n<% shouted = @person.shout %>\n";
        let view = harness.write("app/views/user_mailer/digest.html.erb", template);
        harness.index();
        harness.index_gems();

        // `@story` is the mailer's, reached through a convention that names the class and the line,
        // so the local it types is drawn. `@person` is six letters and nothing else, and the margin
        // is where that answer may not appear.
        assert_eq!(
            drawn_hints(template, &harness.hints_in(&view)),
            "<% headline: String = @story.headline %>"
        );
    }

    #[test]
    fn a_tooltip_is_the_card_s_footnote_and_is_built_only_when_asked_for() {
        // Two halves of one contract. No hint ships its tooltip (every hint on screen would
        // otherwise carry a paragraph of markdown), and every hint ships the `data` that fetches
        // one, because every drawn hint is derived and has something to say.
        //
        // The tooltip reuses `hover::provenance`'s sentences on purpose: the margin and the card
        // are never on screen together, so two wordings for one answer would be a difference nobody
        // could notice.
        let (mut harness, uri) = with_declared_types(HINTS, HINTS_RBS);
        let hints = harness.hints_in(&uri);
        let hints = hints.as_array().expect("hints").clone();

        for hint in &hints {
            assert_eq!(hint["tooltip"], serde_json::Value::Null, "{hint}");
            assert!(hint["data"]["at"].is_number(), "{hint}");
        }

        // The one hint with two footnotes: a chain of signatures, and the instance-variable
        // assignment the chain started from, named by the line a reader can go to (line 8 of this
        // file), not an offset.
        let ivar = hints
            .iter()
            .find(|hint| hint["position"]["line"] == 19)
            .expect("the hint on `from_ivar`");
        let resolved = harness.ask("inlayHint/resolve", ivar.clone());
        assert_eq!(
            resolved["tooltip"]["value"].as_str().unwrap_or("null"),
            "Type derived through `String#upcase()` \u{2192} `String#length()` — from what those \
             methods declare, not from this expression.\n\n\
             Type taken from the assignment on line 8, which may not be the one that ran."
        );

        // And the third family's, which is not one of `hover`'s lines and could not be: those
        // answer what was followed to type a *receiver*, and this answers what the method itself
        // was declared to return, by a file other than the one being read.
        let declared = hints
            .iter()
            .find(|hint| hint["position"]["line"] == 10)
            .expect("the hint on `def headline`");
        let resolved = harness.ask("inlayHint/resolve", declared.clone());
        assert_eq!(
            resolved["tooltip"]["value"].as_str().unwrap_or("null"),
            "Return type taken from a signature — what the method is declared to return, not \
             what this body was read to return."
        );
    }

    #[test]
    fn hints_answer_for_the_range_they_were_asked_about() {
        // The protocol asks for the window the editor shows, and the range bounds the *work*:
        // `cursor::bindings_in` takes it, so a chain outside the window is never classified, rather
        // than classified and dropped.
        let (mut harness, uri) = with_declared_types(HINTS, HINTS_RBS);

        assert_eq!(
            drawn_hints(HINTS, &harness.hints_within(&uri, (15, 0), (16, 99))),
            "    derived: String = \"hi\".upcase
    chained: Integer = \"hi\".upcase.length"
        );
        // The answer above cannot prove that on its own: a version that classified the whole file
        // and filtered afterwards returns the same list. What separates them is work. The window
        // classified the two bindings it drew; the whole file classifies every binding. Counted,
        // not timed: a clock says one thing on a quiet machine and another on a busy one, which is
        // what `cursor::CLASSIFIED` is for.
        assert_eq!(cursor::classifications_taken(), 2);
        harness.hints_in(&uri);
        assert_eq!(cursor::classifications_taken(), BINDINGS_IN_HINTS);

        // A window with no binding answers `null`, not an empty array, as every other list here
        // does: nothing to say is not an empty answer.
        assert_eq!(
            harness.hints_within(&uri, (2, 0), (4, 0)),
            serde_json::Value::Null
        );
    }

    #[test]
    fn a_name_the_call_syntax_discards_the_value_of_is_not_labelled() {
        // `HINTS` pins the two absences beside everything drawn; this is the boundary between them
        // and the five operators that merely *look* like setters. `==`, `===`, `!=`, `<=` and `>=`
        // end in `=`, none is an assignment, and each keeps its label, which is why the rule reads
        // the byte before the `=`, not the `=` alone. `[]=` is on the other side: `report[k] = v`
        // evaluates to `v` exactly like `report.title = v`.
        let source = "\
class Report
  def ==(other)
    \"x\"
  end

  def ===(other)
    \"x\"
  end

  def !=(other)
    \"x\"
  end

  def <=(other)
    \"x\"
  end

  def >=(other)
    \"x\"
  end

  def title=(value)
    \"x\"
  end

  def []=(key, value)
    \"x\"
  end

  def initialize
    \"x\"
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "");
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def ==(other) -> String
  def ===(other) -> String
  def !=(other) -> String
  def <=(other) -> String
  def >=(other) -> String"
        );
    }

    #[test]
    fn each_family_of_hint_is_silenced_by_its_own_setting() {
        // Three families, three flags, and no fourth flag for all of them: every client that asks
        // for inlay hints has its own switch, and a duplicate setting would be a second place to
        // look when the margin is empty.
        for (setting, gone) in [
            ("block_parameters", "|byte: Integer|"),
            ("locals", "derived: String"),
            ("returns", "def headline -> String"),
        ] {
            let (mut harness, uri) = with_declared_types_and_config(
                HINTS,
                HINTS_RBS,
                &format!("\n[hints]\n{setting} = false\n"),
            );
            let drawn = drawn_hints(HINTS, &harness.hints_in(&uri));
            assert!(!drawn.contains(gone), "{setting} was set false:\n{drawn}");
            // And only that one: turning off one family must not take another with it.
            assert_eq!(
                drawn.lines().count(),
                match setting {
                    // Nine in all: four returns, four locals and the block's parameter.
                    "returns" => 5,
                    "locals" => 5,
                    _ => 8,
                },
                "{setting} took more than its own family:\n{drawn}"
            );
        }
    }

    #[test]
    fn a_file_the_index_never_saw_still_gets_the_hints_that_need_no_document() {
        // `tmp/**/*` is excluded by default, so this file is on disk, open in the editor, and
        // absent from the graph. The two halves then answer differently, and both are right: a
        // local's type is read from the buffer and looked up by class name, which needs no
        // document, while a `def`'s declared return is keyed by the declaration rubydex filed for
        // it, and there is none. So the margin is thinner, not empty.
        let source = "\
def stray
  derived = \"hi\".upcase
end
";
        let (mut harness, _) = with_declared_types("class Kept\nend\n", "");
        let uri = harness.write("tmp/stray.rb", source);
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  derived: String = \"hi\".upcase"
        );
    }

    #[test]
    fn a_return_the_file_itself_declares_is_not_repeated_in_its_margin() {
        // "RBS declares it and the source does not" is the whole third family, and a YARD tag two
        // lines up is the source declaring it. The type is the same either way (`annotations`
        // generates RBS from the tag just like a `sig/` file), so what tells the two apart is
        // *which document* the declaration was generated from: a pure function of this one's URI.
        let source = "\
class Ledger
  # @return [String]
  def stamped
  end

  def plain
  end
end
";
        let (mut harness, uri) =
            with_declared_types(source, "class Ledger\n  def plain: () -> String\nend\n");

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def plain -> String"
        );
    }

    #[test]
    fn a_method_that_falls_out_of_a_conditional_is_declined_rather_than_labelled() {
        // The margin's half of the fall-through rule: `cursor::returns_in` files the `nil`, and
        // this is what a reader sees. `guarded` is a real method in miniature: one branch, no
        // `else`, so the value on the common path is `nil`. Without the `nil` it would draw a flat
        // `-> String`, wrong on the majority path, and with two unmerged answers it would draw
        // nothing. It draws **`-> String?`**, the whole sentence.
        //
        // The other two are the controls. `both` writes every branch and answers plainly, so the
        // mark is about the missing branch, not conditionals. `empty` has no branch at all, and
        // `nil` is what it returns, not a gap: the one case the mark cannot cover, because there is
        // no other answer to sit on.
        let source = "\
class Ledger
  def guarded
    if stamped?
      \"x\"
    end
  end

  def both
    if stamped?
      \"x\"
    else
      \"y\"
    end
  end

  def empty
    if stamped?
    end
  end
end
";
        // `NilClass` is declared here because `TYPED_RBS` is a handful of classes, not core, and a
        // class the graph never heard of is not a label, as for any other literal.
        let (mut harness, uri) = with_declared_types(source, "class NilClass\nend\n");

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def guarded -> String?
  def both -> String
  def empty -> nil"
        );
    }

    #[test]
    fn a_method_whose_guard_bails_with_a_bare_return_is_marked_too() {
        // The same rule in the spelling real guards use. `guarded` is `return if query.blank?`
        // above the work: it returns `nil` whenever the guard takes, so it draws the work's type
        // **with the mark**, the whole sentence rather than half.
        //
        // `always` is the control that keeps the keyword: a `return` with a value answers plainly,
        // so the mark is about the missing value, not `return`. `bare` is the whole method, and
        // `nil` is a right answer, not a gap.
        let source = "\
class Ledger
  def guarded
    return if stamped?
    \"x\"
  end

  def always
    return \"x\" if stamped?
    \"y\"
  end

  def bare
    return
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "class NilClass\nend\n");

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def guarded -> String?
  def always -> String
  def bare -> nil"
        );
    }

    #[test]
    fn a_method_that_answers_true_or_false_is_labelled_boolean() {
        // Ruby has no boolean class, and RBS calls the pair `bool`. `predicate` writes the two
        // halves in two branches, as an application does; `flagged` in the RBS beside it is
        // declared `bool`, as a signature does (core RBS uses it heavily). Both draw the same word,
        // because they are the same type and the word is already in the bundle.
        //
        // `only_true` is the control, and why the fold is of the **pair**: a method that can only
        // return `true` is not a predicate, and `true` is both narrower than `bool` and correct.
        // `maybe` is both folds at once.
        let source = "\
class Gate
  def predicate
    if stamped?
      true
    else
      false
    end
  end

  def only_true
    true
  end

  def maybe
    return if stamped?
    predicate
  end

  def read
    held = Switch.new.flagged
    held
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class NilClass\nend\n\nclass TrueClass\nend\n\nclass FalseClass\nend\n\n\
             class Switch\n  def flagged: () -> bool\nend\n",
        );

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def predicate -> bool
  def only_true -> true
  def maybe -> bool?
  def read -> bool
    held: bool = Switch.new.flagged"
        );
    }

    #[test]
    fn two_signature_documents_declaring_one_method_are_merged_and_never_the_last_one_read() {
        // One method declared on one class by two documents: each document's arms are kept, and the
        // partition is settled over their union, the same computation as for a single document, one
        // level out. Otherwise whichever document was read last would silently win.
        let source = "class Reader
  def both_declare_it
    Widget.new.plus
  end

  def only_the_first
    Widget.new.only_core
  end

  def only_the_second
    Widget.new.only_extra
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class Widget\n  def plus: () -> String\n  def only_core: () -> String\nend\n",
        );
        harness.write(
            "sig/extra.rbs",
            "class Widget\n  def plus: () -> Integer\n  def only_extra: () -> Integer\nend\n",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // `plus` draws nothing: two arms of one arity naming two classes is a union of names,
            // which this module drops. Picking one would be confident and depend only on read
            // order.
            //
            // The other two show this is a merge, not a refusal to read a method two documents
            // mention: a name only one document declares still answers.
            "  def only_the_first -> String
  def only_the_second -> Integer"
        );
    }

    #[test]
    fn a_ruby_alias_and_an_alias_method_are_the_method_they_rename() {
        // An RBS `alias map collect` is the method it renames in all four tables. The two Ruby
        // spellings are the same fact, so `Array#blank?` (ActiveSupport's
        // `alias_method :blank?, :empty?`) answers like `Array#empty? -> bool`, the real
        // declaration one name away in `vendor/rbs`.
        //
        // Both spellings are here because they are two Prism nodes with one meaning, and the
        // singleton pair is here because `alias` can rename a `self.` method too.
        let source = "class Reader
  def by_keyword
    Widget.new.renamed
  end

  def by_call
    Widget.new.called
  end

  def on_the_singleton
    Widget.built
  end

  def of_nothing
    Widget.new.of_untyped
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class Widget\n  def size: () -> Integer\n  def self.make: () -> String\nend\n",
        );
        harness.write(
            "app/widget.rb",
            "class Widget\n  alias renamed size\n  alias_method :called, :size\n\n  \
             class << self\n    alias built make\n  end\n\n  \
             def anonymous\n    yield\n  end\n  alias of_untyped anonymous\nend\n",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // `of_nothing` is the refusal that shows this is a copy, not an invention: the target's
            // own row was never filed, so the alias gets nothing instead of a guess.
            "  def by_keyword -> Integer
  def by_call -> Integer
  def on_the_singleton -> String"
        );
    }

    #[test]
    fn two_gems_defining_one_method_differently_answer_the_union_and_never_one_of_them() {
        // ya-lsp indexes a gem's whole `lib/` instead of tracing real `require`s, so a file almost
        // no application loads contributes a definition as real as one every application loads.
        // `Symbol#as_json` is the live case: ActiveSupport's returns a `String` (`name`), and
        // Ruby's `json` stdlib gem declares an unrelated method of the same name returning a 2-key
        // `Hash`. Both bodies are written out here, splat parameter and all, because the *arity* of
        // the two `def`s differs and their shared declaration does not.
        //
        // **The answer must be the union, never one of the two.** A chain cannot step off a union
        // ([`types::Typed::one`] refuses it), so a confident pick would be a wrong answer where
        // this is a refusal a label can still print.
        let source = "class Reader\n  def read\n    Widget.new.as_json\n  end\n\n  \
                      def agreeing\n    Widget.new.same\n  end\nend\n";
        let (mut harness, uri) = with_declared_types(
            source,
            "class Widget\n  def name: () -> String\n  def to_s: () -> String\nend\n",
        );
        harness.write(
            "app/a.rb",
            "class Widget\n  def as_json(options = nil)\n    name\n  end\n\n  \
             def same\n    to_s\n  end\nend\n",
        );
        harness.write(
            "app/b.rb",
            "class Widget\n  def as_json(*)\n    { \"id\" => to_s, \"s\" => to_s }\n  end\n\n  \
             def same\n    name\n  end\nend\n",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // And two bodies that *agree* are still one answer, showing the rule above is a fold,
            // not a refusal to read a reopened method.
            "  def read -> String | Hash
  def agreeing -> String"
        );
    }

    #[test]
    fn a_bare_type_name_in_a_signature_is_resolved_from_where_it_was_written() {
        // RBS resolves an unqualified reference the way Ruby resolves a constant: its own enclosing
        // scope first, then each enclosing scope, then the top level. `Thing` written inside
        // `Outer::Holder` names `Outer::Thing`, a *sibling* of `Holder`, not a class nested in it.
        // Looking the name up exactly as spelled would miss it in hand-curated gem signatures.
        let source = "\
class Reader
  def sibling
    Outer::Holder.new.get
  end

  def top_level
    Outer::Holder.new.plain
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "\
class Thing
end

module Outer
  class Thing
  end

  class Holder
    def get: () -> Thing
    def plain: () -> ::Thing
  end
end
",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // The nested one for the bare name, and the top-level one where `::` says so. Fully
            // qualified, which shows the bare name was resolved from `Outer`, not from the top
            // level where the other `Thing` is.
            "  def sibling -> Outer::Thing
  def top_level -> Thing"
        );
    }

    #[test]
    fn a_signature_line_beside_a_real_def_is_not_a_body_with_nothing_in_it() {
        // An RBS `def:` line and a real `def` are two `Definition::Method`s of one declaration. The
        // body walk must not require *both* to produce a readable body: a signature file parsed as
        // Ruby never has an exit at that span, so requiring it would throw away a body the walk
        // already read correctly, and installing real gem signatures would make coverage worse.
        let source = "\
class Reader
  def read
    Widget.new.murky
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            // `untyped` is the signature saying nothing, which is what sends this to the body.
            "\
class Widget
  def murky: () -> untyped
end
",
        );
        harness.write(
            "app/widget.rb",
            "\
class Widget
  def murky
    \"x\"
  end
end
",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def read -> String"
        );
    }

    #[test]
    fn the_weakest_exit_decides_the_tier_of_the_whole_body() {
        // Two exits naming one class, reached two ways: a literal, which the code states outright,
        // and a bare word nothing resolves, matched on the name alone. The class is the same, the
        // *tier* is not, and a guess is never painted into a margin, so the whole label goes
        // instead of half of it being trustworthy.
        let source = "\
class Reader
  def both_tiers(flag)
    return \"literal\" if flag
    string
  end

  def resolved_only(flag)
    return \"literal\" if flag
    \"other\"
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "");
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // `both_tiers` is a `String` by both exits and is drawn by neither.
            "  def resolved_only(flag) -> String"
        );
    }

    #[test]
    fn two_arms_one_arity_are_told_apart_by_what_the_call_passed() {
        // `vendor/rbs/core/integer.rbs` declares `Integer#+` four ways (`(Integer) -> Integer`,
        // `(Float) -> Float`, `(Rational) -> Rational`, `(Complex) -> Complex`), and bigdecimal
        // reopens `Integer` for a fifth. One arity, five answers, so the bucket disagrees and
        // `1 + 2` would have no type. What tells them apart is written three characters away: the
        // argument.
        //
        // `through_an_ancestor` is the case an equality test would miss, and why the comparison
        // walks the chain: `Float#+` declares `(Numeric) -> Float`, and `1.5 + 1` hands it an
        // `Integer`.
        //
        // The last five are refusals and must stay refusals:
        //
        // 1. An arm whose parameter this cannot read cannot be ruled out.
        // 2. An arm with an *optional* positional has no fixed position-to-parameter map.
        // 3. A call writing no argument has nothing to pick with.
        // 4. Two arms fitting alike are the disagreement the partition already refused, from the
        //    other side.
        // 5. A splat is a count nothing here can know.
        let source = "\
class Reader
  def by_the_first
    Widget.new.pick(1)
  end

  def by_the_second
    Widget.new.pick(\"x\")
  end

  def through_an_ancestor
    Widget.new.wider(Derived.new)
  end

  def one_arm_unreadable
    Widget.new.murky(1)
  end

  def an_optional_lines_up_with_nothing
    Widget.new.loose(1)
  end

  def nothing_to_pick_with
    Widget.new.bare
  end

  def two_arms_fit_alike
    Widget.new.twinned(1)
  end

  def a_splat_cannot_be_counted
    Widget.new.pick(*args)
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "\
class Base
end

class Derived < Base
end

class Widget
  def pick: (Integer) -> String
          | (String) -> Integer
  def wider: (Base) -> String
           | (Integer) -> Float
  def murky: (Integer) -> String
           | (untyped) -> Integer
  def loose: (?Integer) -> String
           | (?String) -> Integer
  def bare: () -> String
          | () -> Integer
  def twinned: (Integer) -> String
             | (Integer) -> Float
end
",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def by_the_first -> String
  def by_the_second -> Integer
  def through_an_ancestor -> String"
        );
    }

    #[test]
    fn a_signature_two_gems_bodies_dispute_answers_the_union_and_never_the_signature_alone() {
        // The live shape. `Symbol#as_json` is declared by `vendor/rbs/stdlib/json/0/json.rbs`
        // (Ruby's signature for the opt-in `json/add/symbol.rb` feature, which reopens thirteen
        // core classes), so the signature rung would answer `Hash[String, String]` and never read
        // ActiveSupport's real body, for every `as_json` like it in every Rails application.
        //
        // **The signature is joined, not demoted.** Where two gems wrote a body, the signature can
        // describe at most one, so both answers stand and [`types::Typed::one`] makes the pair
        // terminal. `undisputed` is the other half and the whole cost argument: a single body is
        // the one the signature beside it describes (every `sig/` and every `.gem_rbs_collection`
        // entry), and it keeps exactly its answer, even where the body disagrees outright.
        //
        // `agreed` is the quiet half: two bodies, both read, both matching the signature, which is
        // returned untouched instead of rebuilt around the same class. `renamed` shows why an alias
        // is not a second body: it has no body of its own, so it can neither dispute a signature
        // nor be disputed, and the `Integer` its signature declares survives a `name` that returns
        // a `String`.
        //
        // The last two are facets crossing the join: a body with a `nil` exit marks the pair
        // nilable, and a body whose exit is a predicate marks it `bool`. Both are OR-ed, not
        // compared, as `body_return` folds them over its own exits, so neither is lost by the half
        // that did not earn it.
        let source = "\
class Reader
  def disputed
    Widget.new.as_json
  end

  def undisputed
    Widget.new.only_one_body
  end

  def agreed
    Widget.new.two_bodies_one_answer
  end

  def renamed
    Widget.new.by_another_name
  end

  def maybe_missing
    Widget.new.sometimes_nil
  end

  def predicate_body
    Widget.new.answers_a_flag
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "\
class Widget
  def name: () -> String
  def to_s: () -> String
  def as_json: (*untyped) -> Hash[String, String]
  def only_one_body: () -> Integer
  def two_bodies_one_answer: () -> String
  def by_another_name: () -> Integer
  def sometimes_nil: () -> Integer
  def answers_a_flag: () -> Integer
  def flag: () -> bool
end
",
        );
        harness.write(
            "app/a.rb",
            "\
class Widget
  def as_json(options = nil)
    name
  end

  def only_one_body
    name
  end

  def two_bodies_one_answer
    name
  end

  def by_another_name
    name
  end

  def sometimes_nil
    return nil if name
    name
  end

  def answers_a_flag
    flag
  end
end
",
        );
        harness.write(
            "app/b.rb",
            "\
class Widget
  def as_json(*)
    { \"id\" => to_s }
  end

  def two_bodies_one_answer
    to_s
  end

  alias_method :by_another_name, :name

  def sometimes_nil
    to_s
  end

  def answers_a_flag
    flag
  end
end
",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def disputed -> Hash | String
  def undisputed -> Integer
  def agreed -> String
  def renamed -> Integer
  def maybe_missing -> Integer? | String
  def predicate_body -> Integer | bool"
        );
    }

    #[test]
    fn a_bool_receiver_asks_both_halves_and_folds_where_they_answer_differently() {
        // `bool` carries `TrueClass` and means the pair, which is right while the two declare the
        // same names, and ActiveSupport is exactly the gem that breaks that. It reopens both with
        // `blank?` and opposite bodies, so the carrier is no longer arbitrary: through
        // `TrueClass`'s own override, `"x".empty?.blank?` would confidently answer `false` about a
        // value that is really undetermined.
        let source = "\
class Ledger
  def split_answer
    Switch.new.flagged.blank?
  end

  def agreed_answer
    Switch.new.flagged.to_s
  end

  def one_half_only
    true.blank?
  end

  def shared_member
    Switch.new.flagged.itself_name
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class NilClass\nend\n\n\
             class TrueClass\n  def blank?: () -> false\n  def to_s: () -> \"true\"\nend\n\n\
             class FalseClass\n  def blank?: () -> true\n  def to_s: () -> \"false\"\nend\n\n\
             class Object\n  def itself_name: () -> String\nend\n\n\
             class Switch\n  def flagged: () -> bool\nend\n",
        );

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // The two halves disagree (`false` against `true`), and the pair they name is exactly
            // `bool`: the honest answer, and the one the receiver already had.
            "  def split_answer -> bool
  def agreed_answer -> String
  def one_half_only -> false
  def shared_member -> String"
        );
    }

    #[test]
    fn folding_the_two_halves_of_a_bool_keeps_the_mark_the_pair_and_the_weaker_tier() {
        // The fold itself, in the three ways the two halves can differ other than by class. Each is
        // the rule the module already applies to two signature arms and two body exits, asked of
        // the two halves of a `bool`.
        let source = "\
class Ledger
  def one_half_is_nilable
    Switch.new.flagged.marked
  end

  def the_halves_are_the_pair
    Switch.new.flagged.paired
  end

  def one_half_is_a_body
    Switch.new.flagged.bodied
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class NilClass\nend\n\n\
             class TrueClass\n  def marked: () -> String?\n  def paired: () -> true\n  \
             def bodied: () -> String\nend\n\n\
             class FalseClass\n  def marked: () -> String\n  def paired: () -> false\nend\n\n\
             class Switch\n  def flagged: () -> bool\nend\n",
        );
        // The undeclared half is written in Ruby, so its answer is *derived* from a body, while the
        // other half's is read off a signature. The pair rests on the weaker of the two, which is
        // `body_return`'s and `shortcut`'s rule.
        harness.write(
            "app/halves.rb",
            "class FalseClass\n  def bodied\n    \"x\"\n  end\nend\n",
        );
        harness.index();

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // A mark on either half marks the answer; `true` beside `false` is the pair, which is
            // what `bool` means and so folds back to the receiver's own word; and two halves
            // agreeing on `String` agree whichever rung each read it from.
            "  def one_half_is_nilable -> String?
  def the_halves_are_the_pair -> bool
  def one_half_is_a_body -> String"
        );
    }

    #[test]
    fn a_unary_bang_is_folded_against_the_operand_and_answers_bool_when_it_cannot_be() {
        // `!` has the same guarantee as `&&` and `||` (Ruby returns one of two values whatever the
        // operand), so it folds the same way. The three cases: an operand that can only be truthy,
        // one that can only be falsy, and one nothing can say anything about, which `&&` has no
        // equivalent of and which is the one that pays.
        let source = "\
class Ledger
  def not_a_bool
    !Switch.new.flagged
  end

  def not_a_string
    !\"x\"
  end

  def not_unknown
    !@mystery
  end

  def not_nil
    !nil
  end

  def double
    !!Switch.new.flagged
  end

  def blankish
    respond_to?(:empty?) ? !!Switch.new.flagged : false
  end
end
";
        let (mut harness, uri) = with_declared_types(
            source,
            "class NilClass\nend\n\nclass TrueClass\n  def !: () -> false\nend\n\n\
             class FalseClass\n  def !: () -> true\nend\n\n\
             class String\n  def !: () -> false\nend\n\n\
             class Switch\n  def flagged: () -> bool\nend\n",
        );

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def not_a_bool -> bool
  def not_a_string -> false
  def not_unknown -> bool
  def not_nil -> true
  def double -> bool
  def blankish -> bool"
        );
    }

    #[test]
    fn a_destructured_target_is_the_position_it_is_and_never_the_whole_value() {
        // **The margin asks the same question as `locator`, or it answers a different one.**
        // `blobs, actions = prepare` is three types, not one: each name holds a position of what
        // the call returned. Without the `Receiver::Destructured` wrap the rest of the crate
        // applies, `cursor::bindings_in` would ask "what does `prepare` return", and draw that one
        // `Array` on *every* name left of the `=`, as in `: Array` three times over
        // `blobs, actions, error = ...` in real code. Nothing declares a tuple for `prepare`, so
        // the honest answer is no label at all, which is what a card at a use of `blobs` says too.
        //
        // `pair` is the other half of the rule, and why the wrap is not just a refusal: a tuple
        // that *is* declared answers each position exactly, and the margin is where someone
        // counting names left of an `=` wants to read it.
        let source = "\
class Ledger
  def prepare
    [\"x\"]
  end

  def use
    blobs, actions = prepare
    word, size = \"hi\".pair
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "");

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            // **`Array[String]`, not `Array`**, and the difference is where the label came from:
            // this is the *body* rung, which read `[\"x\"]` and knows what the literal held. A
            // return drawn from a **signature** stays the bare head: `Typed::declared_as` has no
            // receiver and no value, so a position has nothing to describe.
            "  def prepare -> Array[String]
    word: String, size = \"hi\".pair
    word, size: Integer = \"hi\".pair"
        );
    }

    #[test]
    fn two_classes_left_after_the_folds_are_drawn_as_the_union_they_are() {
        // The residue, and the one thing it may do. `split` returns a `String` on one path and an
        // `Integer` on the other: neither fold applies, and there is no single class. The margin
        // says what the method does, and that is all a union gets: there is no single class for a
        // `.` to answer from or a chain to step to. Both stop at `types::Typed::one`, which answers
        // `None` for a union.
        //
        // `with_nil` is the same residue with a `nil`, and the mark goes on the head, not around
        // the whole: `String? | Integer` and `(String | Integer)?` are one type, and the first
        // needs no brackets in a margin with no room for them.
        let source = "\
class Ledger
  def split
    if stamped?
      \"x\"
    else
      1
    end
  end

  def with_nil
    return if stamped?
    split
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "class NilClass\nend\n");

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def split -> String | Integer
  def with_nil -> String? | Integer"
        );
    }

    #[test]
    fn the_mark_is_drawn_on_a_local_as_well_as_on_a_return() {
        // The facet reaches every family that draws a type, not only return labels: telling a
        // reader that `held` is a `String` without saying it can be `nil` is the same half-truth in
        // another place. `plain` is the control (same rung, same shape, unmarked answer), and both
        // take their type from a method one `def` away, the hop the mark has to travel.
        let source = "\
class Ledger
  def guarded
    return if stamped?
    \"x\"
  end

  def always
    \"y\"
  end

  def read
    held = guarded
    plain = always
    [held, plain]
  end
end
";
        let (mut harness, uri) = with_declared_types(source, "class NilClass\nend\n");

        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "  def guarded -> String?
  def always -> String
  def read -> Array
    held: String? = guarded
    plain: String = always"
        );
    }
}
