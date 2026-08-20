//! `workspace/symbol` — find a declaration anywhere in the project or its gems.
//!
//! # Why this needs a ranking of its own
//!
//! rubydex's `declaration_search` answers "does this name match", not "how well": its fuzzy
//! mode is a subsequence test whose score is always the query's own length, so every hit ties.
//! At gem scale that matters enormously — a Rails bundle indexes six figures of declarations,
//! and a two-letter query subsequence-matches a large fraction of them. Handing the editor an
//! unranked, unbounded slice of that is indistinguishable from handing it nothing.
//!
//! So the graph-wide filter stays rubydex's (it is parallel, and it is the cheap half), and the
//! ordering is ours: the user's own code first, and within that an exact name beats a prefix
//! beats a substring beats a subsequence. See [`rank`] for why the workspace outranks the match
//! rather than the other way round — it is not the obvious order, and the obvious one measured
//! badly.

use std::collections::HashSet;

use lsp_types::{SymbolKind, SymbolTag};
use rubydex::{
    model::{
        declaration::{Declaration, Namespace},
        definitions::Definition,
        graph::Graph,
        ids::{DeclarationId, UriId},
    },
    query::{self, MatchMode},
};

use super::{
    locator::{self, Site},
    render, symbols,
};

/// One search result, ready to be turned into an LSP symbol once its file has been read.
#[derive(Debug, Clone)]
pub struct Hit {
    /// What the picker lists: `shout`, `self.build`, `MAX_AGE`, `Bar`.
    pub name: String,
    /// The path printed beside it. `None` only at the top level.
    pub container: Option<String>,
    pub kind: SymbolKind,
    pub tags: Option<Vec<SymbolTag>>,
    /// Where to jump. One site per declaration, not one per definition: `ActiveRecord::Base` is
    /// reopened hundreds of times and listing each would bury every other result.
    pub site: Site,
}

/// The best `limit` declarations matching `query`.
///
/// `own` is the set of documents that are the user's own code. It is a parameter rather than a
/// rule in here because "the user's code" also has to exclude a *vendored* bundle, which lives
/// inside the workspace root — a fact this module has no business knowing. It arrives as a set
/// of ids rather than a URI predicate because it is consulted once per *definition* of every
/// candidate, and at gem scale that is a six-figure number of string comparisons per keystroke.
#[must_use]
pub fn search(graph: &Graph, query: &str, limit: usize, own: &HashSet<UriId>) -> Vec<Hit> {
    if limit == 0 {
        return Vec::new();
    }

    let mut ranked: Vec<Ranked> = query::declaration_search(graph, query, &MatchMode::Fuzzy)
        .into_iter()
        .filter_map(|id| {
            let declaration = graph.declarations().get(&id)?;
            // A declaration with no definitions is a placeholder the resolver invented for a
            // namespace it never saw — `Foo::Bar` mentioned by a reference to a `Foo` that does
            // not exist. There is nowhere to jump.
            if !is_listable(declaration) || declaration.has_no_definitions() {
                return None;
            }
            let name = declaration.name();
            Some(Ranked {
                own: declared_in(graph, declaration, own),
                tier: tier(query, name),
                simple_len: render::last_segment(name).len(),
                name,
                id,
            })
        })
        .collect();

    // Partition rather than sort. A one-letter query subsequence-matches most of a Rails
    // bundle, and ordering a hundred thousand candidates to show two hundred is work nobody
    // reads: `select_nth_unstable_by` is linear, and only the part that survives gets sorted.
    if ranked.len() > limit {
        ranked.select_nth_unstable_by(limit, rank);
        ranked.truncate(limit);
    }
    ranked.sort_unstable_by(rank);

    // Sites are computed only for the survivors: it means reaching into every definition of a
    // declaration, and doing that for a hundred thousand candidates would cost more than the
    // search does.
    ranked
        .into_iter()
        .filter_map(|entry| {
            let definition = pick_definition(graph, entry.id, own)?;
            let (name, container) = render::split_qualified(entry.name);
            Some(Hit {
                name,
                container,
                kind: symbols::kind_of(definition),
                tags: definition
                    .is_deprecated()
                    .then(|| vec![SymbolTag::DEPRECATED]),
                site: locator::site(graph, definition)?,
            })
        })
        .collect()
}

/// A candidate, with everything the sort needs and nothing that costs a lookup.
struct Ranked<'g> {
    own: bool,
    tier: u8,
    simple_len: usize,
    name: &'g str,
    id: DeclarationId,
}

/// The user's own code first, then match quality, then the shorter name.
///
/// Putting the workspace ahead of match quality is the decision that makes this feature usable
/// rather than merely correct. A project has a few thousand declarations and its bundle has a
/// hundred and fifty thousand, so *any* ordering that ranks them together fills the picker with
/// gems: measured on a Rails app, "user" put `URI::Generic#user` and `Warden::Proxy#user` — both
/// exact matches, both useless — above the project's own `Users`. rust-analyzer reaches the same
/// conclusion from the other side and searches only the workspace unless asked; keeping the gems
/// in, below the fold, costs nothing and keeps the moat.
fn rank(a: &Ranked<'_>, b: &Ranked<'_>) -> std::cmp::Ordering {
    b.own
        .cmp(&a.own)
        .then(b.tier.cmp(&a.tier))
        .then(a.simple_len.cmp(&b.simple_len))
        .then(a.name.len().cmp(&b.name.len()))
        // Never by `id`: it is a hash, so ties would shuffle between runs.
        .then(a.name.cmp(b.name))
}

/// Which definition of a declaration the picker should jump to.
///
/// The user's own code wins when a name is defined in both: opening a Rails app and searching
/// for `ApplicationRecord` should land in `app/models`, not in whichever gem reopens it.
/// Otherwise it is the first in `definitions_of`'s stable order, so the answer never moves
/// between runs.
fn pick_definition<'g>(
    graph: &'g Graph,
    id: DeclarationId,
    own: &HashSet<UriId>,
) -> Option<&'g Definition> {
    let definitions = locator::definitions_of(graph, id);
    definitions
        .iter()
        .find(|definition| own.contains(definition.uri_id()))
        .or(definitions.first())
        .copied()
}

/// How well `query` matches `name`, from 4 (the name *is* the query) down to 0.
///
/// Zero is reachable only for a name rubydex matched by subsequence, which is the floor of what
/// it returns — so nothing here can promote a non-match.
fn tier(query: &str, name: &str) -> u8 {
    let simple = render::last_segment(name);
    if equal_ci(simple, query) {
        4
    } else if starts_with_ci(simple, query) {
        3
    } else if contains_ci(simple, query) {
        2
    } else if contains_ci(name, query) {
        // The query spelled a path — `Foo::Bar`, `Person#shout` — and this is it.
        1
    } else {
        0
    }
}

/// Whether any of a declaration's definitions is in the user's own code.
///
/// `any`, not "the first one": a class the project reopens is the project's, even when the gem
/// that first defined it sorts ahead of it.
fn declared_in(graph: &Graph, declaration: &Declaration, own: &HashSet<UriId>) -> bool {
    declaration.definitions().iter().any(|id| {
        graph
            .definitions()
            .get(id)
            .is_some_and(|definition| own.contains(definition.uri_id()))
    })
}

/// What belongs in a project-wide symbol list.
///
/// Names rubydex invented are excluded because nobody can search for them — a singleton class
/// is `<Person>` and an anonymous `Class.new` is `<uri>:<offset><anonymous>` — while the methods
/// *inside* a singleton stay, spelled `self.build`. Variables are excluded for the same reason
/// the outline excludes them: they exist for resolution, and `@name` would appear once per class
/// that has one.
fn is_listable(declaration: &Declaration) -> bool {
    if !render::is_nameable(declaration.name()) {
        return false;
    }
    match declaration {
        Declaration::Namespace(namespace) => {
            !matches!(namespace, Namespace::SingletonClass(_) | Namespace::Todo(_))
        }
        Declaration::Constant(_) | Declaration::ConstantAlias(_) | Declaration::Method(_) => true,
        Declaration::GlobalVariable(_)
        | Declaration::InstanceVariable(_)
        | Declaration::ClassVariable(_) => false,
    }
}

fn equal_ci(left: &str, right: &str) -> bool {
    let mut left = left.chars();
    let mut right = right.chars();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(a), Some(b)) if eq_ci(a, b) => {}
            _ => return false,
        }
    }
}

/// Case-insensitive `starts_with`, without allocating a lowercased copy of either side —
/// this runs once per candidate, and at gem scale that is six figures of allocations.
fn starts_with_ci(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|wanted| chars.next().is_some_and(|found| eq_ci(found, wanted)))
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .char_indices()
        .any(|(index, _)| starts_with_ci(&haystack[index..], needle))
}

/// Case folding for the alphabets Ruby identifiers actually use. ASCII is a single compare;
/// anything else falls back to Unicode's full lowercase mapping, which can be several chars.
fn eq_ci(left: char, right: char) -> bool {
    if left.is_ascii() || right.is_ascii() {
        return left.eq_ignore_ascii_case(&right);
    }
    left == right || left.to_lowercase().eq(right.to_lowercase())
}
