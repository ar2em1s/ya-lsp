//! `textDocument/references` — every place a name is used.
//!
//! # Two mechanisms, one request
//!
//! Constants are exact. rubydex's resolver links every constant reference to the declaration it
//! resolves to and files it under that declaration, so answering is a lookup, not a search, and
//! the result is the truth: `Foo` inside `module Bar` and `Bar::Foo` at the top level are the
//! same reference, and a `Foo` that means something else is not in the set.
//!
//! Methods are not, and cannot be. Nothing in ya-lsp infers types, so `person.name` and
//! `response.name` are indistinguishable — there is no receiver to resolve. rubydex records
//! method references but never links them to a declaration, so the only question we can answer
//! is "what is spelled this way". That is genuinely useful for an unusual name and close to
//! useless for `call`, `id`, or `name`, and the honest thing is to say so rather than to dress
//! a grep up as an index.
//!
//! # Scope
//!
//! Both mechanisms are confined to the user's own code. For methods the reason is noise — a
//! Rails bundle spells `name` tens of thousands of times and not one of those is an answer. For
//! constants the reason is that the result is a work list: nobody is going to edit a gem.

use std::{cmp::Reverse, collections::HashSet};

use rubydex::model::{
    declaration::Declaration,
    graph::Graph,
    ids::{DeclarationId, StringId, UriId},
};

use super::locator::{self, Located, Resolution, Target};

/// One place a name appears.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reference {
    /// The graph's own spelling of the document URI.
    pub uri: String,
    pub start: u32,
    pub end: u32,
    /// `true` where this is the place the name is *declared* rather than used.
    ///
    /// `textDocument/references` has no use for the distinction — a work list is a work list —
    /// but `documentHighlight` draws a write differently from a read, and where a name is
    /// written down is already decided here rather than being worth deciding twice.
    pub write: bool,
}

/// Every reference to whatever the cursor is on, inside `scope`.
///
/// `scope` is the set of documents that count as the user's own code; the caller owns that
/// definition because a vendored bundle sits inside the workspace root and must not count.
#[must_use]
pub fn find(
    graph: &Graph,
    located: &Located<'_>,
    resolution: &Resolution,
    scope: &HashSet<UriId>,
    include_declaration: bool,
) -> Vec<Reference> {
    let mut found = match located.target {
        // A call, whatever it did or did not resolve to. Deliberately not routed through the
        // resolution: an unresolved call — a method that only exists after some metaprogramming
        // ran — still has call sites, and they are exactly what was asked for.
        Target::Call(reference) => by_name(graph, &[*reference.str()], scope),
        Target::Constant(_) => by_declaration(graph, &resolution.declarations, scope),
        // The definition itself. Which mechanism applies depends on what was defined, and the
        // resolution is the only thing that knows: `def` and `attr_reader` are both methods,
        // `class` and `=` are both constants.
        Target::Definition(_) => {
            let names = method_names(graph, &resolution.declarations);
            if names.is_empty() {
                by_declaration(graph, &resolution.declarations, scope)
            } else {
                by_name(graph, &names, scope)
            }
        }
    };

    // A redirected resolution is deliberately not the declaration of the name under the cursor
    // — `Foo.new` answers with `Foo#initialize` — so it has no declaration site to add here.
    if include_declaration && !resolution.redirected {
        found.extend(declaration_sites(graph, &resolution.declarations, scope));
    }

    // References arrive per declaration and per document, in hash order. Sorting makes the list
    // read down the file, and adjacent duplicates — the same span reached through two
    // declarations of one reopened class — collapse.
    //
    // `write` is sorted on but deliberately not compared by the dedup: one span reached both as
    // a declaration and as a reference is one place, not two, and the place it is declared is
    // what it is. Leaving it in the comparison would have emitted the same location twice for
    // every caller, `textDocument/references` included.
    found.sort_unstable_by(|left, right| {
        (&left.uri, left.start, left.end, Reverse(left.write)).cmp(&(
            &right.uri,
            right.start,
            right.end,
            Reverse(right.write),
        ))
    });
    // Compared as one tuple rather than as a chain of `&&`: the same comparison, without
    // three short-circuit arms in a file held at 100% of branches for a reason.
    found.dedup_by(|left, right| {
        (&left.uri, left.start, left.end) == (&right.uri, right.start, right.end)
    });
    found
}

/// Exact: the references the resolver linked to these declarations.
fn by_declaration(
    graph: &Graph,
    declarations: &[DeclarationId],
    scope: &HashSet<UriId>,
) -> Vec<Reference> {
    let mut found = Vec::new();
    // One `filter_map` over both steps: a declaration that is not a constant has no constant
    // references, which is the same nothing as an id the graph does not hold, and neither is a
    // case with anything to do about it here.
    for references in declarations
        .iter()
        .filter_map(|id| graph.declarations().get(id)?.constant_references())
    {
        for reference in references
            .iter()
            .filter_map(|id| graph.constant_references().get(id))
        {
            if !scope.contains(&reference.uri_id()) {
                continue;
            }
            // rubydex fabricates a reference to `<Foo>` for every call with a constant or
            // implicit receiver, and for an implicit one it spans the whole call. They are
            // attached to the singleton class rather than to `Foo`, so they should not be
            // reachable from here at all — but a `class << self` under the cursor resolves to
            // exactly that declaration, and bytes the user never wrote must never be listed.
            if is_synthetic(graph, reference.name_id()) {
                continue;
            }
            found.extend(at(graph, reference.uri_id(), reference.offset()));
        }
    }
    found
}

/// Name-based: every call spelled one of `names`, in the user's own files.
///
/// Walks the scope's documents rather than the graph's whole reference table. With a bundle
/// indexed the two differ by an order of magnitude, and every reference outside the scope would
/// be discarded anyway.
fn by_name(graph: &Graph, names: &[StringId], scope: &HashSet<UriId>) -> Vec<Reference> {
    let mut found = Vec::new();
    // `filter_map` to match the inner loop: `scope` is built from the graph's own documents, so
    // a miss is a lookup that yields nothing rather than a case with anything to do about it.
    for document in scope
        .iter()
        .filter_map(|uri_id| graph.documents().get(uri_id))
    {
        for reference in document
            .method_references()
            .iter()
            .filter_map(|id| graph.method_references().get(id))
        {
            if names.contains(reference.str()) {
                found.extend(at(graph, reference.uri_id(), reference.offset()));
            }
        }
    }
    found
}

/// Where the declarations themselves are written, for `includeDeclaration`.
fn declaration_sites(
    graph: &Graph,
    declarations: &[DeclarationId],
    scope: &HashSet<UriId>,
) -> Vec<Reference> {
    declarations
        .iter()
        .flat_map(|id| locator::sites(graph, *id))
        .filter(|site| scope.contains(&UriId::from(site.uri.as_str())))
        .map(|site| Reference {
            uri: site.uri,
            start: site.selection.0,
            end: site.selection.1,
            write: true,
        })
        .collect()
}

/// The interned call-site spellings of every method among `declarations`.
///
/// Both spellings, because rubydex records a call as the bare `shout` but an `alias` as the
/// parenthesised `shout()`. Missing the second silently loses every aliased call.
///
/// `StringId` is a pure hash of the string, so these are built without touching the graph and
/// compared as integers — which is what makes scanning a workspace's references affordable.
fn method_names(graph: &Graph, declarations: &[DeclarationId]) -> Vec<StringId> {
    let mut names = Vec::new();
    for declaration in declarations
        .iter()
        .filter_map(|id| graph.declarations().get(id))
    {
        if !matches!(declaration, Declaration::Method(_)) {
            continue;
        }
        let name = declaration.name();
        let bare = name
            .rsplit_once('#')
            .map_or(name, |(_, method)| method)
            .strip_suffix("()")
            .unwrap_or(name);
        for spelling in [StringId::from(bare), StringId::from(&*format!("{bare}()"))] {
            if !names.contains(&spelling) {
                names.push(spelling);
            }
        }
    }
    names
}

fn is_synthetic(graph: &Graph, name_id: &rubydex::model::ids::NameId) -> bool {
    graph
        .names()
        .get(name_id)
        .and_then(|name| graph.strings().get(name.str()))
        .is_some_and(|string| string.starts_with('<'))
}

fn at(graph: &Graph, uri_id: UriId, offset: &rubydex::offset::Offset) -> Option<Reference> {
    Some(Reference {
        uri: graph.documents().get(&uri_id)?.uri().to_owned(),
        start: offset.start(),
        end: offset.end(),
        write: false,
    })
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use rubydex::{
        indexing::{self, LanguageId},
        resolution::Resolver,
    };

    /// Every declaration whose name ends in `suffix`, in a graph built from one source.
    fn declarations_named(graph: &Graph, suffix: &str) -> Vec<DeclarationId> {
        graph
            .declarations()
            .iter()
            .filter(|(_, declaration)| declaration.name().ends_with(suffix))
            .map(|(id, _)| *id)
            .collect()
    }

    #[test]
    fn two_declarations_spelled_the_same_contribute_one_pair_of_spellings() {
        // `names` is scanned against every method reference in the workspace, once per
        // reference, so a duplicate is not merely untidy — it is a second string comparison per
        // call site for an answer already known. Two classes defining `shout` is the ordinary
        // way a resolution comes to hold more than one declaration of one name.
        let mut graph = Graph::new();
        indexing::index_source(
            &mut graph,
            "file:///fixture/hr.rb",
            "class Person\n  def shout\n  end\nend\n\nclass Siren\n  def shout\n  end\nend\n",
            &LanguageId::Ruby,
        );
        Resolver::new(&mut graph).resolve();

        let shouts = declarations_named(&graph, "#shout()");
        assert_eq!(shouts.len(), 2, "two classes, two declarations");

        // Both spellings, because rubydex records a call as `shout` and an `alias` as `shout()`
        // — and each of them once, however many declarations were spelled that way.
        let names = method_names(&graph, &shouts);
        assert_eq!(
            names,
            vec![StringId::from("shout"), StringId::from("shout()")]
        );
    }
}
