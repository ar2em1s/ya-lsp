//! `textDocument/inlayHint` — a derived type, drawn without being asked for.
//!
//! The one request in the crate that shows an answer nobody requested, and that changes what
//! may be said. A hover card is read after a deliberate keystroke, has room for a footnote, and
//! is understood as ya-lsp's opinion. A hint is painted into the margin of every line whether
//! anyone wanted it or not, has room for nothing, and is read as fact — which is why the tier
//! decides what may be drawn here rather than merely how it is labelled.
//!
//! # The three tiers, and the one of them that cannot occur here
//!
//! - **Guessed** — matched on a name alone. **Never drawn.** A name-matched type painted into
//!   the margin of every method in the file is precisely the failure this project is organised
//!   against, and there is no label small enough to fix it. The refusal is a test on [`Tier`]
//!   and not a list of shapes, which is the difference between a rule and a coincidence: the
//!   next rung added below the graph is refused by the same line rather than appearing in
//!   everybody's margin the day it ships.
//! - **Derived** — a signature, an assignment or a convention was followed. Drawn, with the
//!   footnote in the tooltip, which is the only room a hint has for one.
//! - **Resolved** — the code names the type. **Unreachable, by construction**, and that is the
//!   one thing worth knowing about this module.
//!
//! A hint exists exactly where the code does *not* say the type. Where it does — `x = 1`,
//! `x = Foo.new`, `x = Foo` — the label would repeat a word already on the line, and
//! [`worth_saying`] refuses it as noise. Those are precisely the bindings whose type is
//! resolved, so the tier that would need no footnote and the shapes that need no hint are the
//! same set, seen from two sides: **a type the code states is a type the margin does not have
//! to repeat.**
//!
//! So every hint that is drawn is derived, every one carries a tooltip, and there is no second
//! kind on screen to distinguish it from — which is why the labels carry no marker. What
//! `inlayHint/resolve` buys is not a rare tooltip but a cheap one: the sentence is built for
//! the hint somebody pointed at instead of for every line of the file on every scroll.
//!
//! # Three families, and why not a fourth
//!
//! A block parameter, a local assigned from a call, and a method's declared return. What they
//! have in common is the paragraph above — the line does not already say the type. A server
//! that draws noise gets turned off, at which point the three that are worth something are gone
//! as well.

use std::collections::HashMap;

use ruby_prism::{DefNode, Visit};
use rubydex::model::{
    declaration::{Declaration, Namespace},
    definitions::Definition,
    graph::Graph,
    ids::{DeclarationId, UriId},
};

use crate::workspace::{DocUri, config::HintsConfig};

use super::{
    cursor::{self, Binding, Receiver},
    hover, locator,
    synthesized::generated_prefix,
    types::{self, Derivation, Return, Sources, Tier},
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
    /// A receiver ya-lsp typed, and what it followed to get there. Empty is the resolved tier:
    /// the code named the type and nothing was followed at all.
    ///
    /// **Boxed**, and the other variant is why: a [`Derivation`] carries a footnote's worth of
    /// text for every rung there is, most of them `None` on any one answer, and the variant
    /// beside it carries nothing at all. Hints are built one per binding across a whole visible
    /// range, so the enum's size is paid for every label rather than for the few that followed
    /// anything.
    Followed(Box<Derivation>),
    /// A signature declares what the method hands back, and Ruby has no syntax for saying so.
    ///
    /// Always the derived tier, and that is the honest reading rather than a shortcut. A
    /// signature is a *claim* about a method — nothing checks it unless a type checker is run,
    /// and where it is core RBS being overridden by the `def` under the label it is a claim
    /// about a method that is no longer there. See [`annotations`](super::annotations), which
    /// makes the same argument about the two syntaxes a person writes by hand.
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
    /// Which tier this answer is. [`Tier::Guessed`] never reaches here — see the module docs.
    #[must_use]
    pub fn tier(&self) -> Tier {
        match &self.because {
            Because::Followed(derivation) => derivation.tier(),
            Because::Declared => Tier::Derived,
        }
    }

    /// The offset of the instance-variable assignment this type came from, if one did.
    ///
    /// Handed out rather than rendered, for [`hover::markdown`]'s reason: an offset is only a
    /// line once you have the text it indexes, and the caller that draws the tooltip is the one
    /// holding it.
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
/// *receiver*", and this answers a different question about a different thing — what the method
/// itself was declared to hand back, by a file that is not the one being read.
const DECLARED: &str = "Return type taken from a signature — what the method is declared to \
                        return, not what this body was read to return.";

/// Every hint for the part of `source` inside `within`, in source order.
///
/// `within` is the range the editor has on screen, and it bounds the *work* rather than the
/// answer — [`cursor::bindings_in`] takes it for exactly that reason. One parse either way; what
/// the range buys is every graph lookup after it.
///
/// **The buffer's offsets are the graph's here, and that is a property of the request rather
/// than an assumption.** `textDocument/inlayHint` is not one of the three that answer between a
/// keystroke and its index, so `Analysis::serve` has settled before this runs and the two texts
/// are one string — the same footing `documentHighlight` and `references` answer on.
#[must_use]
pub fn of(
    sources: &Sources<'_>,
    uri: &DocUri,
    source: &str,
    within: (u32, u32),
    shown: &HintsConfig,
) -> Vec<Hint> {
    let uri_id = UriId::from(uri.as_str());
    // Read once, asked once per binding. `Scope::at` walks every definition in the document, so
    // asking it per candidate is quadratic in the file — see [`types::Scope::bodies`], which
    // exists because this request is the first caller to have more than one cursor.
    let bodies = types::Scope::bodies(sources.graph, uri_id);
    // And handed down, because `method_receiver` has an arm that places an offset of its own —
    // a captured `self` — and would otherwise do the walk this hoisted out, once per hint.
    let sources = &Sources {
        bodies: Some(&bodies),
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
            let scope = bodies.at(bound.name.0);
            let typed = types::method_receiver(sources, uri_id, &bound.was, &scope)?;
            Some(Hint {
                family: match bound.binding {
                    Binding::BlockParameter => Family::BlockParameter,
                    Binding::Local => Family::Local,
                },
                at: bound.name.1,
                label: label(sources.graph, typed.declaration)?,
                because: Because::Followed(Box::new(typed.derivation)),
            })
        })
        .collect();

    if shown.returns {
        hints.extend(returns(sources, uri, uri_id, source, within));
    }
    // The bottom tier is refused here, once, after every family has had its say — a test on the
    // tier and never on the shape that produced it. See the module docs.
    hints.retain(|hint| hint.tier() != Tier::Guessed);
    hints.sort_by_key(|hint| hint.at);
    hints
}

/// Whether a binding's shape says anything the line it is on does not already.
///
/// The whole of the "not every local" rule, and it is about the *shape* rather than the type:
/// `x = Foo.new`, `x = Foo`, `x = "s"` and `x = 1` all name their class in the assignment, and a
/// label repeating it is a word of noise on a line that was already clear. What is left is the
/// call whose return nothing writes down, which is the case worth a margin.
///
/// A block parameter is always worth saying — nothing about `|story|` says what it holds — and
/// reaches here as the one shape that can only have come from a signature.
fn worth_saying(was: &Receiver) -> bool {
    match was {
        Receiver::Returned { .. } | Receiver::Yielded { .. } => true,
        // The two wrappers are not shapes of their own: an instance variable carries where it
        // was written and a spelled local carries its name, and what either one is worth saying
        // about is whatever it wraps.
        Receiver::Assigned { was, .. } | Receiver::Spelled { was, .. } => worth_saying(was),
        _ => false,
    }
}

/// A label for the declaration a type resolved to, or `None` where there is nothing to draw.
///
/// **Classes and modules only.** A singleton class is the type of a *class object*, which Ruby
/// has no way to spell — `Foo::<Foo>` is rubydex's name for it and `singleton(Foo)` is RBS's —
/// and a `Namespace::Todo` is a name the workspace references and nothing defines, so a label
/// drawn from one points at nothing a reader could go and check. Both are a type this module
/// cannot spell exactly, which is the same test [`annotations`](super::annotations) applies to
/// what it will declare.
fn label(graph: &Graph, declaration: DeclarationId) -> Option<String> {
    Some(format!(": {}", class_name(graph, declaration)?))
}

fn class_name(graph: &Graph, declaration: DeclarationId) -> Option<&str> {
    let declaration = graph.declarations().get(&declaration)?;
    matches!(
        declaration,
        Declaration::Namespace(Namespace::Class(_) | Namespace::Module(_))
    )
    .then(|| declaration.name())
}

/// Every `def` in `within` whose return a signature declares and the source does not.
///
/// Two halves that have to agree, and they agree by construction: the anchor comes from Prism —
/// the end of the parameter list, which is the only place ` -> String` reads as Ruby — and the
/// declaration comes from the graph, joined to it by the name span the two parsers both report
/// for the same bytes.
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
    // The documents whose declarations were written *here*: a Sorbet `sig` or a YARD
    // `@return` two lines up is the source saying the return type, so a margin repeating it is
    // the noise this module's third family is defined against. A prefix and not one URI, because
    // one source writes one generated document per body — and the prefix is exact, which is what
    // the trailing `#` in `generated_prefix` is for.
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
            // written inside; the two query-interface sentinels are names no file declares. None
            // of the three is a class to draw — see `types::ELEMENT`.
            let Return::Class(class) = sources.types.declared_return(id)? else {
                return None;
            };
            let declared = types::declared(graph, class)?;
            Some(Hint {
                family: Family::Return,
                at,
                label: format!(" -> {}", class_name(graph, declared)?),
                because: Because::Declared,
            })
        })
        .collect()
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
        // Past the closing parenthesis where there is one, past the parameters where they were
        // written without any, and past the name where there are none: `def title(a)`,
        // `def title a` and `def title` all end their signature somewhere different, and a label
        // drawn at the name would land inside the parameter list of the first.
        let at = node
            .rparen_loc()
            .map(|paren| paren.end_offset() as u32)
            .or_else(|| {
                node.parameters()
                    .map(|parameters| parameters.location().end_offset() as u32)
            })
            .unwrap_or(name.1);
        // From the name to where the label goes, which is the span this `def` occupies as far as
        // a hint is concerned — see [`cursor::overlaps`]. Testing the name alone would miss a
        // `def` whose label is inside the window and whose name is one character above it.
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

    /// A workspace whose only signatures are [`TYPED_RBS`], plus one file of the user's code.
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
end
";

    /// How many bindings [`HINTS`] holds, whatever any of them is worth saying about.
    ///
    /// Seven locals and two block parameters, all inside `def bindings`. Not the number of
    /// hints — most of these are refused by [`worth_saying`] or by their tier — but the number
    /// of candidates a whole-document request works out the shape of, which is what
    /// `hints_answer_for_the_range_they_were_asked_about` counts.
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
        // `GALLERY`'s treatment for the one answer nobody asks for. Read the margin down the
        // page: what is *absent* is as much the assertion as what is drawn, and three of the
        // absences are three different rules.
        //
        // - `plain = "hi"` and `made = Report.new` say the class on the line. A label repeating
        //   it is a word of noise, and those are exactly the bindings whose type is resolved —
        //   which is why no hint anywhere in this file is.
        // - `guessed = person.shout` types to `Person`, from six letters and nothing else. The
        //   card for it says so and is read on purpose; the margin refuses it outright.
        // - `each do |unyielded|` writes no receiver, so it is a `yield` rather than a call,
        //   and what a `yield` hands over is the body of the method that wrote it rather than
        //   anything a signature declares. There is nothing to read.
        // - `def initialize` has no signature, so there is nothing to declare about it, and
        //   `def chainable` is declared `-> self`, which at the `def` means the class the label
        //   would be written inside. A margin repeating the line above it is the first rule
        //   again, one construct further out.
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
        // The refusal, asserted **by tier and not by example**: the same expression is asked
        // twice, once as a card and once as a label. The card names `Person#shout` and says in
        // its last line what that rests on — the six letters of `person` — because a card is
        // read after a deliberate keystroke and has room to qualify itself. The margin has no
        // room, is read as fact, and is drawn on every line whether anybody wanted it or not,
        // so the bottom tier does not appear in it at all.
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
        // `inlayHint` is one of the requests a template needs nothing extra for, so the
        // view↔renderer rung — the controller a template's directory names, or the mailer where
        // there is no controller — decides what is painted into a margin nobody asked for. That
        // is why widening it to a mailer's views was measured before it shipped rather than
        // after: over the six corpora the card answered **29 more** of the 1,301 template reads
        // drawn, and a template's card is what a label here is built from.
        //
        // What moved is the population and not the tier. Every one of those answers is a
        // convention and therefore *Derived*, which was already drawn; the refusal below is the
        // same test on [`Tier`] it always was, in the same file and from the same request.
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

        // `@story` is the mailer's, reached through a convention that names the class and the
        // line — so the local it types is drawn. `@person` is six letters and nothing else, and
        // the margin is where that answer is not allowed to appear.
        assert_eq!(
            drawn_hints(template, &harness.hints_in(&view)),
            "<% headline: String = @story.headline %>"
        );
    }

    #[test]
    fn a_tooltip_is_the_card_s_footnote_and_is_built_only_when_asked_for() {
        // Two halves of one contract. Nothing ships a tooltip — every hint on screen would
        // otherwise carry a paragraph of markdown — and every hint ships the `data` that buys
        // one back, because every hint that is drawn is derived and so has something to say.
        //
        // What it says is `hover::provenance`'s own sentences, which is the point of sharing
        // them: the margin and the card are never on screen together, so two wordings for one
        // answer would be a difference nobody could ever see.
        let (mut harness, uri) = with_declared_types(HINTS, HINTS_RBS);
        let hints = harness.hints_in(&uri);
        let hints = hints.as_array().expect("hints").clone();

        for hint in &hints {
            assert_eq!(hint["tooltip"], serde_json::Value::Null, "{hint}");
            assert!(hint["data"]["at"].is_number(), "{hint}");
        }

        // The one hint with two footnotes: a chain of signatures, and the instance-variable
        // assignment the chain started from — named by the line a reader can go to, which is
        // line 8 of this file and not an offset into it.
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
        // all answer what was followed to type a *receiver*, and this answers what the method
        // itself was declared to hand back — by a file that is not the one being read.
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
        // The protocol asks for the window the editor is showing, and the range bounds the
        // *work*: `cursor::bindings_in` takes it, so a chain outside the window is never
        // classified rather than classified and then dropped.
        let (mut harness, uri) = with_declared_types(HINTS, HINTS_RBS);

        assert_eq!(
            drawn_hints(HINTS, &harness.hints_within(&uri, (15, 0), (16, 99))),
            "    derived: String = \"hi\".upcase
    chained: Integer = \"hi\".upcase.length"
        );
        // And the answer above is not the claim, because it cannot be: a version that classified
        // the whole file and filtered afterwards returns exactly that list. What separates the
        // two is work. The window classified the two bindings it drew; the same file asked whole
        // classifies every binding in it. Counted rather than timed — a clock says one thing on
        // a quiet machine and another on a busy one, which is what `cursor::CLASSIFIED` is for.
        assert_eq!(cursor::classifications_taken(), 2);
        harness.hints_in(&uri);
        assert_eq!(cursor::classifications_taken(), BINDINGS_IN_HINTS);

        // And a window with no binding in it answers `null` rather than an empty array, for the
        // reason every other list here does: nothing to say is not the same as an empty answer.
        assert_eq!(
            harness.hints_within(&uri, (2, 0), (4, 0)),
            serde_json::Value::Null
        );
    }

    #[test]
    fn each_family_of_hint_is_silenced_by_its_own_setting() {
        // Three families, three flags, and no fourth flag turning the lot off: every client
        // that asks for inlay hints has a switch of its own, and a setting duplicating it would
        // be a second place to look when the margin is empty.
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
        // absent from the graph. The two halves then answer differently and both are right: a
        // local's type is read out of the buffer and looked up by class name, which needs no
        // document — while a `def`'s declared return is keyed by the declaration rubydex filed
        // for it, and there is none. So the margin is thinner rather than empty.
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
        // "RBS declares it and the source does not" is the whole of the third family, and a
        // YARD tag two lines up is the source declaring it. The type is the same either way —
        // `annotations` generates RBS from the tag exactly as a `sig/` file would — so what
        // tells the two apart is *which document* the declaration was generated from, which is
        // a pure function of this one's URI.
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
}
