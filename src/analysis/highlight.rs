//! `textDocument/documentHighlight` — every place in *this* file that means the same thing.
//!
//! Without a provider VS Code highlights occurrences by matching words, which is wrong in three
//! ordinary ways: it lights up `name` inside a comment, inside a string, and in a scope that has
//! nothing to do with the one under the cursor. Those three are the bar, and they are the fixture.
//!
//! # Two halves, two sources
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
//! # Where the halves meet
//!
//! `@name = 1` is the one span both could answer for — rubydex records the assignment as a
//! declaration even though it records no reference to one. The scope walk wins, and has to: the
//! graph would answer with the single place the variable is written and none of the places it is
//! read, which is a highlight that looks like it worked.

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
    locator::locate(graph, uri_id, offset)
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
        })
        .unwrap_or_default()
        .into_iter()
        .map(|reference| Highlight {
            start: reference.start,
            end: reference.end,
            kind: kind(reference.write),
        })
        .collect()
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
