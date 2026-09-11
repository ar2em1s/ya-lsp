//! `textDocument/documentHighlight` — every place in *this* file that means the same thing.
//!
//! Without a provider VS Code highlights occurrences by matching words, which is wrong in three
//! ordinary ways: it lights up `name` inside a comment, inside a string, and in a scope that has
//! nothing to do with the one under the cursor. Those three are the bar, and they are the fixture.
//!
//! # Three sources, asked in one order
//!
//! Locals, parameters and instance variables come from [`scopes`], which walks the buffer. The
//! graph models none of them — see that module for why — and it is asked first because it is the
//! half that can say *no*: it claims the cursor only when the cursor really is on a variable, and
//! `None` from it is what lets a constant or a call fall through.
//!
//! Constants and methods come from the graph through [`references`], scoped to one document.
//! Nothing new is computed for them; a constant is exact and a method is matched by name, the
//! same trade `textDocument/references` makes. Confined to one file it is a much better trade
//! than across a workspace: the other `render` in this file really is likely to be the same one.
//!
//! A macro's `:symbol` comes from the buffer again, and **last**. rubydex records a call and not
//! its arguments, so unlike an instance variable there is no span the graph answers for wrongly —
//! there is one it does not answer for at all.
//!
//! # Where the three meet
//!
//! `@name = 1` is the one span two of them could answer for — rubydex records the assignment as a
//! declaration even though it records no reference to one. The scope walk wins, and has to: the
//! graph would answer with the single place the variable is written and none of the places it is
//! read, which is a highlight that looks like it worked.
//!
//! The symbol is the opposite case and takes the opposite order. Where the graph *does* speak at
//! one — `attr_reader :count` files a definition whose name span is the symbol — it is already
//! naming the declaration the buffer walk would have found, with the reference machinery attached
//! to it. Asking it first costs nothing and keeps one path through `locate` for every target the
//! graph holds.

use std::collections::HashSet;

use lsp_types::DocumentHighlightKind;
use rubydex::model::{graph::Graph, ids::UriId};

use super::{locator, references, scopes, synthesized::Synthesized};

/// One place to draw, in bytes into the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Highlight {
    pub start: u32,
    pub end: u32,
    pub kind: DocumentHighlightKind,
}

/// Everywhere in `source` that names whatever `offset` is on.
///
/// Empty when the cursor is on nothing this can speak for — a comment, a string, a keyword —
/// which the caller turns into a `null` so the client may fall back to its own word matching.
#[must_use]
pub fn find(
    graph: &Graph,
    synthesized: &Synthesized,
    uri_id: UriId,
    source: &str,
    offset: u32,
) -> Vec<Highlight> {
    if let Some(occurrences) = scopes::occurrences(source, offset) {
        return occurrences
            .into_iter()
            .map(|at| Highlight {
                start: at.start,
                end: at.end,
                kind: kind(at.write),
            })
            .collect();
    }

    // One document, which is the whole difference between this and `textDocument/references`.
    let scope = HashSet::from([uri_id]);
    let found = locator::locate(graph, uri_id, offset)
        .into_iter()
        // As in goto-definition and references: several targets can share the narrowest span,
        // so take the first that has something to say rather than the first that exists.
        .find_map(|located| {
            let resolution = locator::resolve(graph, &located);
            // Always with the declaration: the `def` and the `class` line are exactly what a
            // reader scanning a file for a name wants lit up, and `includeDeclaration` — the
            // client's way of saying otherwise — is not a field this request has.
            let found = references::find(graph, synthesized, &located, &resolution, &scope, true);
            (!found.is_empty()).then_some(found)
        });
    let Some(found) = found else {
        return macro_symbol(graph, synthesized, uri_id, source, offset, &scope);
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

/// A macro's `:symbol`, which the graph holds no target for at all.
///
/// **After the graph and not before it**, which is the opposite order from the scope walk above
/// and for the stated reason: at `@name = 1` the graph answers and answers wrongly, while at a
/// symbol it simply does not answer. Where it does — `attr_reader :count` files a definition whose
/// name span *is* the symbol — its answer is the declaration this would have found anyway, with
/// the reference machinery already attached.
///
/// The symbol's own span is added because half the macros do not record it: one that *declares* a
/// name — `belongs_to`, `enum`, `scope` — has already put it there as the declaration's place, and
/// one that only *names* an existing method — `before_action`, `validate` — leaves the cursor's
/// own word unlit. It goes in as a read, which is what it is; a declaration's place arrives as a
/// write from [`references::to_member`] and is left alone.
///
/// Keeping the two requests in step is the point. `definition` answers here, so a highlight set
/// that did not contain the span it lands on would be exactly the disagreement this module's rule
/// about `@name = 1` exists to prevent, one cursor shape over.
fn macro_symbol(
    graph: &Graph,
    synthesized: &Synthesized,
    uri_id: UriId,
    source: &str,
    offset: u32,
    scope: &HashSet<UriId>,
) -> Vec<Highlight> {
    let Some((symbol, resolution)) = locator::resolve_symbol(graph, uri_id, source, offset, offset)
    else {
        return Vec::new();
    };
    let mut found: Vec<Highlight> = references::to_member(
        graph,
        synthesized,
        &symbol.name,
        &resolution.declarations,
        scope,
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

/// LSP's third kind, `Text`, is for a match nothing is known about — a word search. Everything
/// here is one or the other, so it is never the answer.
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
        // The whole file's answer, drawn. Four things are pinned by what is *not* marked here,
        // and every one of them is a way the editor's own word matching is wrong: the `name` in
        // the comment, the `name` inside the string, the `name` that is a method, and the two
        // `name`s belonging to other scopes — `initialize`'s parameter above and the block
        // parameter that shadows this one on the line in between.
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
        // Prism resolved these, not us: the block parameter and the read beside it are one
        // variable at depth 0, and the `[name]` three characters to their left is another at
        // depth 1. Nothing in `scopes` says the word "shadow".
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
        // The graph's half, and the one place the two halves could have disagreed about who
        // owns a name: `name` in `shout` is a call because no local is spelled that way there,
        // and a parameter named `name` two methods up must not join it.
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
    fn an_instance_variable_belongs_to_whatever_self_is() {
        // `@name` in an instance method and `@name` in `def self.rename` are two variables —
        // one hangs off an instance of `Person` and the other off `Person` itself — and Ruby
        // will happily let a file use both. Joining them is the kind of wrong that reads as
        // right, which is why `scopes` tracks what `self` is rather than only the class body.
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
        // The bar all of this is measured against. Both of these are positions where an
        // editor matching words lights the file up, and both answer `null` — which is also what
        // hands the fallback back to the client for exactly the positions ya-lsp cannot speak
        // for, rather than replacing it with an empty list everywhere.
        let mut harness = Harness::new();
        let uri = harness.write("lib/person.rb", OCCURRENCES);
        harness.index();

        assert_eq!(harness.highlight_map(&uri, &on("# A name")), "null");
        assert_eq!(harness.highlight_map(&uri, &on("label = \"name")), "null");
    }
}
