//! `textDocument/documentHighlight`: every place in *this* file that means the same thing.
//!
//! Without a provider, VS Code highlights occurrences by matching words, which is wrong in three
//! ordinary ways: it lights up `name` inside a comment, inside a string, and in a scope unrelated
//! to the one under the cursor. Those three are the bar, and they are the fixture.
//!
//! # Three sources, asked in this order
//!
//! 1. **Locals, parameters and instance variables** come from [`scopes`](super::scopes), which
//!    walks the buffer, through [`locator::occurrences_at`]. The graph models none of them (see
//!    that module for why). It is asked first because it is the half that can say *no*: it claims
//!    the cursor only when the cursor really is on a variable, and its `None` lets a constant or a
//!    call fall through.
//! 2. **Constants and methods** come from the graph through [`references`], scoped to one document.
//!    Nothing new is computed: a constant is exact and a method is matched by name, the trade
//!    `textDocument/references` makes. Within one file it is a much better trade than across a
//!    workspace: the other `render` in this file really is likely to be the same one.
//! 3. **A macro's `:symbol`** comes from the buffer again, **last**. rubydex records a call, not
//!    its arguments, so unlike an instance variable there is no span the graph answers wrongly;
//!    there is one it does not answer at all.
//!
//! # Where the three meet
//!
//! `@name = 1` is the one span two of them could answer. rubydex records the assignment as a
//! declaration, though it records no reference to one. The scope walk wins, and must: the graph
//! would answer with the single place the variable is written and none of the places it is read, a
//! highlight that looks like it worked.
//!
//! The symbol is the opposite case, and takes the opposite order. Where the graph *does* speak at
//! one (`attr_reader :count` files a definition whose name span is the symbol), it already names
//! the declaration the buffer walk would have found, with the reference machinery attached. Asking
//! it first costs nothing and keeps one path through `locate` for every target the graph holds.

use std::collections::HashSet;

use lsp_types::DocumentHighlightKind;
use rubydex::model::{graph::Graph, ids::UriId};

use super::{cursor, environment, indexed::Indexed, locator, references, synthesized::Synthesized};

/// What [`find`] asks of its caller, which reads what this module does not: receivers' types and
/// other documents' text.
#[derive(Clone, Copy)]
pub struct Asked<'a> {
    /// Resolves a method-name symbol ([`locator::resolve_named`]). Asked only where a macro's
    /// symbol is, and before it: it parses the buffer, which every other cursor would pay for
    /// nothing.
    pub named: &'a dyn Fn() -> Option<(locator::Symbol, locator::Resolution)>,
    /// Finds a method's name handed as a symbol, as `references` does ([`references::Named`]), so
    /// the two agree.
    pub handed: &'a references::Named<'a>,
    /// Where a loose instance-variable occurrence's block runs ([`locator::occurrences_at`]), so
    /// the variable lit is the one `definition` jumps within.
    pub rebound: &'a dyn Fn(u32) -> Option<i32>,
    /// An instance variable a symbol names ([`locator::named_variable`]) and where this document
    /// writes it: what `definition` jumps to there, so both are lit.
    pub variable: &'a dyn Fn() -> Option<NamedVariable>,
    /// Where this document writes the instance variable read at an offset without spelling it
    /// (`types::instance_writes`): an `instance_variable_set`, an `attr_writer`. `definition`
    /// lands there when the file writes it nowhere by name.
    pub unspelled: &'a dyn Fn(u32) -> Vec<(u32, u32)>,
}

/// A symbol naming an instance variable, and the spans this document writes it at
/// ([`Asked::variable`]).
pub type NamedVariable = (locator::Symbol, Vec<(u32, u32)>);

/// One place to draw, in bytes into the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Highlight {
    pub start: u32,
    pub end: u32,
    pub kind: DocumentHighlightKind,
}

/// Everywhere in `source` that names whatever `offset` is on.
///
/// Empty when the cursor is on nothing this can speak for (a comment, a string, a keyword); the
/// caller turns that into `null`, so the client may fall back to its own word matching.
///
/// `asked` is what this module needs the caller for ([`Asked`]).
#[must_use]
pub fn find(
    graph: &Indexed,
    synthesized: &Synthesized,
    uri_id: UriId,
    text: &cursor::Parsed<'_>,
    offset: u32,
    layout: environment::Layout<'_>,
    asked: &Asked<'_>,
) -> Vec<Highlight> {
    let Asked {
        named,
        handed,
        rebound,
        variable,
        unspelled,
    } = *asked;
    if let Some((name, under, occurrences)) = locator::occurrences_at(text, offset, rebound) {
        let mut lit: Vec<Highlight> = occurrences
            .iter()
            .map(|at| Highlight {
                start: at.start,
                end: at.end,
                kind: kind(at.write),
            })
            .collect();
        // An instance variable the file writes nowhere by name: `definition` goes to every write
        // the type side folds, and those in this file are lit too, as writes.
        // They are writes, and nothing lit is: no span is lit twice.
        if name.starts_with('@') && !occurrences.iter().any(|at| at.write) {
            lit.extend(
                unspelled(under.start)
                    .into_iter()
                    .map(|(start, end)| Highlight {
                        start,
                        end,
                        kind: kind(true),
                    }),
            );
            lit.sort_by_key(|at| at.start);
        }
        return lit;
    }

    // One document, which is the whole difference between this and `textDocument/references`.
    let scope = HashSet::from([uri_id]);
    let found = locator::locate(graph, uri_id, offset)
        .into_iter()
        // As in goto-definition and references: several targets can share the narrowest span, so
        // take the first with something to say, not the first that exists.
        .find_map(|located| {
            // Never the tree fence (a use under `spec/` is a use), and always the other one, for
            // `references`' reason: a document outside the project is not one this list is about.
            // The scope here is the cursor's own document, so the second gate can only speak about
            // a scratch file lighting up its own names, which is exactly the case its cursor rule
            // turns it off for.
            let resolution = locator::resolve(
                graph,
                &located,
                environment::Fence::uses(locator::uri_of(graph, uri_id), layout),
            );
            // Always with the declaration: the `def` and the `class` line are exactly what a reader
            // scanning a file for a name wants lit, and this request has no `includeDeclaration`
            // field to say otherwise.
            let found = references::find(
                graph,
                synthesized,
                &located,
                &resolution,
                &scope,
                true,
                handed,
            );
            (!found.is_empty()).then_some(found)
        });
    let Some(found) = found else {
        if let Some((symbol, writes)) = variable() {
            let mut lit = vec![Highlight {
                start: symbol.start,
                end: symbol.end,
                kind: kind(false),
            }];
            lit.extend(writes.into_iter().map(|(start, end)| Highlight {
                start,
                end,
                kind: kind(true),
            }));
            return lit;
        }
        let Some((symbol, resolution)) =
            named().or_else(|| locator::resolve_symbol(graph, uri_id, text, offset, offset))
        else {
            return Vec::new();
        };
        return symbol_highlights(graph, synthesized, &symbol, &resolution, &scope, handed);
    };
    found
        .into_iter()
        .map(|reference| Highlight {
            start: reference.start,
            end: reference.end,
            kind: kind(reference.write),
        })
        .collect()
}

/// A symbol argument, which the graph holds no target for at all: a macro's `:symbol`, or a method
/// name handed to `send` and its kin.
///
/// **After the graph, not before it**: the opposite order from the scope walk, for the stated
/// reason. At `@name = 1` the graph answers wrongly; at a symbol it simply does not answer. Where
/// it does (`attr_reader :count` files a definition whose name span *is* the symbol), its answer is
/// the declaration this would have found anyway, with the reference machinery attached.
///
/// The symbol's own span is added because half the macros do not record it:
/// - one that *declares* a name (`belongs_to`, `enum`, `scope`) already put it there as the
///   declaration's place;
/// - one that only *names* an existing method (`before_action`, `validate`) leaves the cursor's own
///   word unlit.
///
/// It goes in as a read, which is what it is; a declaration's place arrives as a write from
/// [`references::to_member`] and is left alone.
///
/// Keeping the two requests in step is the point. `definition` answers here, so a highlight set
/// missing the span it lands on would be exactly the disagreement this module's `@name = 1` rule
/// prevents, one cursor shape over.
fn symbol_highlights(
    graph: &Graph,
    synthesized: &Synthesized,
    symbol: &locator::Symbol,
    resolution: &locator::Resolution,
    scope: &HashSet<UriId>,
    handed: &references::Named<'_>,
) -> Vec<Highlight> {
    let mut found: Vec<Highlight> = references::to_member(
        graph,
        synthesized,
        &symbol.name,
        &resolution.declarations,
        scope,
        handed,
    )
    .into_iter()
    .map(|reference| Highlight {
        start: reference.start,
        end: reference.end,
        kind: kind(reference.write),
    })
    .collect();
    if !found
        .iter()
        .any(|at| (at.start, at.end) == (symbol.start, symbol.end))
    {
        let cursor = Highlight {
            start: symbol.start,
            end: symbol.end,
            kind: kind(false),
        };
        let index = found
            .iter()
            .position(|at| at.start > symbol.start)
            .unwrap_or(found.len());
        found.insert(index, cursor);
    }
    found
}

/// LSP's third kind, `Text`, is for a match nothing is known about: a word search. Everything here
/// is one or the other, so it is never the answer.
fn kind(write: bool) -> DocumentHighlightKind {
    if write {
        DocumentHighlightKind::WRITE
    } else {
        DocumentHighlightKind::READ
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use crate::analysis::testing::*;

    #[test]
    fn a_local_is_highlighted_in_its_own_scope_and_in_no_other() {
        // The whole file's answer, drawn. Four things are pinned by what is *not* marked, each a
        // way the editor's word matching is wrong:
        // - the `name` in the comment;
        // - the `name` inside the string;
        // - the `name` that is a method;
        // - the two `name`s in other scopes: `initialize`'s parameter above, and the block
        //   parameter shadowing this one on the line between.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", OCCURRENCES);
        harness.index();

        assert_eq!(
            harness.highlight_map(&uri, &on("def greet(name")),
            "  def greet(name)\n\
             \u{20}           wwww\n\
             \u{20}   [name].each { |name| label = name }\n\
             \u{20}    rrrr\n\
             \u{20}   name + label\n\
             \u{20}   rrrr"
        );
    }

    #[test]
    fn a_block_parameter_shadows_the_local_it_is_spelled_like() {
        // Prism resolved these, not us: the block parameter and the read beside it are one variable
        // at depth 0, and the `[name]` three characters to their left is another at depth 1.
        // Nothing in `scopes` says the word "shadow".
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", OCCURRENCES);
        harness.index();

        assert_eq!(
            harness.highlight_map(&uri, &on("{ |name")),
            "    [name].each { |name| label = name }\n\
             \u{20}                  wwww          rrrr"
        );
    }

    #[test]
    fn each_method_keeps_its_own_parameter() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", OCCURRENCES);
        harness.index();

        assert_eq!(
            harness.highlight_map(&uri, &on("def initialize(name")),
            "  def initialize(name)\n\
             \u{20}                wwww\n\
             \u{20}   @name = name.strip\n\
             \u{20}           rrrr"
        );
    }

    #[test]
    fn a_method_is_highlighted_where_it_is_defined_and_where_it_is_called() {
        // The graph's half, and the one place the two halves could disagree about who owns a name:
        // `name` in `shout` is a call because no local is spelled that way there, and a parameter
        // named `name` two methods up must not join it.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", OCCURRENCES);
        harness.index();

        assert_eq!(
            harness.highlight_map(&uri, &on("  def shout\n    name")),
            "  def name\n\
             \u{20}     wwww\n\
             \u{20}   name.upcase\n\
             \u{20}   rrrr"
        );
    }

    #[test]
    fn a_call_in_a_block_of_a_class_body_lights_the_class_s_own_def() {
        // rubydex names the class object as the receiver of a bare word in a block written into
        // the body, as for a statement of it, and the class object has no `render_error`. The block
        // may run on an instance (a `rescue_from … do` runs on the controller), and this
        // request has no buffer to tell the two apart, so the name list keeps the class's own
        // `def` rather than proving it unreachable. Made certain, that call lost its `def`.
        let mut harness = Harness::new();
        let source = "\
class Handler
  [1].each { render_error }

  def render_error
  end
end
";
        let uri = harness.write("lib/handler.rb", source);
        harness.index();

        assert_eq!(
            harness.highlight_map(&uri, &cursor_after(source, "{ render")),
            "  [1].each { render_error }\n\
             \u{20}            rrrrrrrrrrrr\n\
             \u{20} def render_error\n\
             \u{20}     wwwwwwwwwwww"
        );
    }

    #[test]
    fn an_instance_variable_belongs_to_whatever_self_is() {
        // `@name` in an instance method and `@name` in `def self.rename` are two variables: one
        // hangs off an instance of `Person`, the other off `Person` itself, and Ruby happily lets a
        // file use both. Joining them is the kind of wrong that reads as right, which is why
        // `scopes` tracks what `self` is, not only the class body.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", OCCURRENCES);
        harness.index();

        assert_eq!(
            harness.highlight_map(&uri, &on("initialize(name)\n    @name")),
            "    @name = name.strip\n\
             \u{20}   wwwww\n\
             \u{20}   @name\n\
             \u{20}   rrrrr"
        );
        assert_eq!(
            harness.highlight_map(&uri, &on("rename(name)\n    @name")),
            "    @name = name\n\
             \u{20}   wwwww"
        );
    }

    #[test]
    fn a_constant_is_highlighted_exactly() {
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", OCCURRENCES);
        harness.index();

        assert_eq!(
            harness.highlight_map(&uri, &on("@limit = MAX")),
            "  MAX = 10\n\
             \u{20} www\n\
             \u{20}   @limit = MAX\n\
             \u{20}            rrr"
        );
    }

    #[test]
    fn a_name_in_a_comment_or_a_string_is_not_an_occurrence_of_anything() {
        // The bar all of this is measured against. Both are positions where an editor matching
        // words lights up the file, and both answer `null`, which hands the fallback back to the
        // client exactly where ya-lsp cannot speak, instead of replacing it with an empty list
        // everywhere.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", OCCURRENCES);
        harness.index();

        assert_eq!(harness.highlight_map(&uri, &on("# A name")), "null");
        assert_eq!(harness.highlight_map(&uri, &on("label = \"name")), "null");
    }
}
