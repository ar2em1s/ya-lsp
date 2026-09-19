//! `textDocument/references`: every place a name is used.
//!
//! # Two mechanisms, one request
//!
//! **Constants are exact.** rubydex's resolver links every constant reference to the declaration it
//! resolves to and files it there, so answering is a lookup, not a search, and the result is the
//! truth. `Foo` inside `module Bar` and `Bar::Foo` at the top level are the same reference, and a
//! `Foo` that means something else is not in the set.
//!
//! **Methods are matched by name.** rubydex records method references but never links them to a
//! declaration. This request stays on `locator::resolve` **on purpose**: the resolution path with
//! no document text, which derives no receiver. A work list is a list of places to edit, and a
//! derived receiver is the one entry that could be wrong. So the question answered is "what is
//! spelled this way": useful for an unusual name, close to useless for `call`, `id` or `name`, and
//! said plainly, not dressed up as an index.
//!
//! # Scope
//!
//! Both mechanisms are confined to the user's own code.
//! - Methods: noise. A Rails bundle spells `name` tens of thousands of times, and none of those is
//!   an answer.
//! - Constants: the result is a work list, and nobody is going to edit a gem.
//!
//! # An alias is a use of the name, not a call
//!
//! `alias reject! destroy!` names `destroy!` and calls nothing. Both halves matter:
//! - the line belongs in a work list, because a rename that skipped it leaves an alias pointing at
//!   a method that no longer exists;
//! - it does not belong in a tree of callers, because nothing is called there.
//!
//! [`Spellings`] is where the two part: `find` asks for both spellings, [`calls_to`] for the bare
//! one.

use std::{cmp::Reverse, collections::HashSet};

use rubydex::model::{
    declaration::Declaration,
    graph::Graph,
    ids::{DeclarationId, StringId, UriId},
};

use super::{
    locator::{self, Located, Resolution, Target},
    synthesized::Synthesized,
};

/// One place a name appears.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reference {
    /// The graph's own spelling of the document URI.
    pub uri: String,
    pub start: u32,
    pub end: u32,
    /// `true` where this is the place the name is *declared*, not used.
    ///
    /// `textDocument/references` has no use for the distinction (a work list is a work list), but
    /// `documentHighlight` draws a write differently from a read. Where a name is written down is
    /// already decided here, so it is not decided twice.
    pub write: bool,
}

/// Every reference to whatever the cursor is on, inside `scope`.
///
/// `scope` is the set of documents that count as the user's own code. The caller owns that
/// definition, because a vendored bundle sits inside the workspace root and must not count.
#[must_use]
pub fn find(
    graph: &Graph,
    synthesized: &Synthesized,
    located: &Located<'_>,
    resolution: &Resolution,
    scope: &HashSet<UriId>,
    include_declaration: bool,
) -> Vec<Reference> {
    let mut found = match located.target {
        // A call, whatever it did or did not resolve to. Deliberately not routed through the
        // resolution: an unresolved call (a method that only exists after some metaprogramming ran)
        // still has call sites, and they are exactly what was asked for. That is also why the
        // second spelling comes from the cursor's own word, not from a declaration as in
        // `method_names`: there may be no declaration, and a redirected one is spelled something
        // else entirely.
        Target::Call(reference) => by_name(graph, &call_spellings(graph, reference.str()), scope),
        Target::Constant(_) => by_declaration(graph, &resolution.declarations, scope),
        // The definition itself. Which mechanism applies depends on what was defined, and only the
        // resolution knows: `def` and `attr_reader` are both methods, `class` and `=` are both
        // constants.
        Target::Definition(_) => {
            let names = method_names(graph, &resolution.declarations, Spellings::Written);
            if names.is_empty() {
                by_declaration(graph, &resolution.declarations, scope)
            } else {
                by_name(graph, &names, scope)
            }
        }
    };

    // A redirected resolution is deliberately not the declaration of the name under the cursor
    // (`Foo.new` answers with `Foo#initialize`), so it has no declaration site to add.
    if include_declaration && !resolution.redirected {
        found.extend(declaration_sites(
            graph,
            synthesized,
            &resolution.declarations,
            scope,
        ));
    }

    ordered(found)
}

/// Every place a **member** is named, for a cursor the graph holds no target for.
///
/// [`find`] starts from a [`Located`]: the graph saying what the cursor is on. A macro's `:symbol`
/// has no such target (rubydex records the call, not its arguments), so the name comes from the
/// buffer instead. After that the mechanism is the one a method under the cursor gets: matched by
/// name, declaration included.
///
/// `name` is the bare word, and both spellings are searched, for [`Spellings`]' reason.
/// [`method_names`] derives them from a declaration's name and is deliberately not used here: it
/// splits on `#`, and a `scope :recent` declares `Story.recent()`.
#[must_use]
pub fn to_member(
    graph: &Graph,
    synthesized: &Synthesized,
    name: &str,
    declarations: &[DeclarationId],
    scope: &HashSet<UriId>,
) -> Vec<Reference> {
    let mut found = by_name(graph, &spellings(name), scope);
    found.extend(declaration_sites(graph, synthesized, declarations, scope));
    ordered(found)
}

/// Every call of one method, matched by name, in the user's own code.
///
/// The mechanism [`find`] gives a method under the cursor, addressed by declaration instead:
/// `callHierarchy/incomingCalls` arrives holding the method it wants the callers of, and there is
/// no position to locate. It differs from [`find`] at the same method in two ways:
/// - **The declaration is not in the result.** A `def` is not a call of itself.
/// - **One spelling, not two.** An `alias` writes the name down without calling it, so its line is
///   a reference, not a caller. [`Spellings`] carries the argument.
///
/// **Found by name is found by name.** Every caller of `call` or `name` here is a caller of
/// *something* spelled that way, exactly as in `textDocument/references`, and a tree makes that
/// look more precise than a flat list does. Saying so is the caller's job, and it is not optional.
#[must_use]
pub fn calls_to(
    graph: &Graph,
    declaration: DeclarationId,
    scope: &HashSet<UriId>,
) -> Vec<Reference> {
    let names = method_names(graph, &[declaration], Spellings::Called);
    if names.is_empty() {
        return Vec::new();
    }
    ordered(by_name(graph, &names, scope))
}

/// One list, read down the file, with each place in it once.
///
/// References arrive per declaration and per document, in hash order. Sorting makes the list read
/// down the file, and adjacent duplicates (the same span reached through two declarations of one
/// reopened class) collapse.
///
/// `write` is sorted on but deliberately not compared by the dedup: a span reached both as a
/// declaration and as a reference is one place, not two. Comparing it would emit the same location
/// twice for every caller, `textDocument/references` included.
fn ordered(mut found: Vec<Reference>) -> Vec<Reference> {
    found.sort_unstable_by(|left, right| {
        (&left.uri, left.start, left.end, Reverse(left.write)).cmp(&(
            &right.uri,
            right.start,
            right.end,
            Reverse(right.write),
        ))
    });
    // Compared as one tuple, not a chain of `&&`: the same comparison without three short-circuit
    // arms, in a file held at 100% of branches.
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
    // references, the same nothing as an id the graph does not hold, and neither needs handling
    // here.
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
            // rubydex fabricates a reference to `<Foo>` for every call with a constant or implicit
            // receiver, and for an implicit one it spans the whole call. They are attached to the
            // singleton class, not `Foo`, so they should be unreachable from here. But a
            // `class << self` under the cursor resolves to exactly that declaration, and bytes the
            // user never wrote must never be listed.
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
/// Walks the scope's documents, not the graph's whole reference table. With a bundle indexed the
/// two differ by an order of magnitude, and every reference outside the scope would be discarded
/// anyway.
fn by_name(graph: &Graph, names: &[StringId], scope: &HashSet<UriId>) -> Vec<Reference> {
    let mut found = Vec::new();
    // `filter_map` to match the inner loop: `scope` is built from the graph's own documents, so a
    // miss is a lookup that yields nothing, not a case to handle.
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
    synthesized: &Synthesized,
    declarations: &[DeclarationId],
    scope: &HashSet<UriId>,
) -> Vec<Reference> {
    declarations
        .iter()
        .flat_map(|id| locator::sites(graph, synthesized, *id))
        .filter(|site| scope.contains(&UriId::from(site.uri.as_str())))
        .map(|site| Reference {
            uri: site.uri,
            start: site.selection.0,
            end: site.selection.1,
            write: true,
        })
        .collect()
}

/// Which of a method name's two spellings a name-based search matches.
///
/// **rubydex writes one method name into its reference table two ways**, and the parenthesis is not
/// an interning detail: it is the only thing in the graph that tells an alias from a call.
/// - A call records the bare `shout`.
/// - The *old name* of an `alias` or `alias_method` records the parenthesised `shout()`, and
///   nothing else does. `def`, `attr_reader` and `private :shout` carry that spelling on the
///   **declaration** instead, where no search meets it.
///
/// So the choice is not about spelling. It is whether an alias line belongs in the answer, and the
/// two surfaces built on this file want opposite things.
#[derive(Clone, Copy)]
enum Spellings {
    /// Both: every place the name is written down. What a work list wants: `alias reject! destroy!`
    /// is a use of `destroy!`, and a rename taken off a list without it leaves the alias naming a
    /// method that is gone.
    Written,
    /// The bare one: the places that actually call the method. What a call hierarchy wants: an
    /// alias line calls nothing, and a tree of callers holding one says it does.
    Called,
}

impl Spellings {
    /// How many of [`spellings`]' two to use, which is why that function returns the bare one
    /// first.
    fn how_many(self) -> usize {
        match self {
            Self::Written => 2,
            Self::Called => 1,
        }
    }
}

/// The two ways one method name is recorded, `shout` and `shout()`, bare first.
///
/// `StringId` is a pure hash of the string, so these are built without touching the graph and
/// compared as integers. That is what makes scanning a workspace's references affordable.
fn spellings(name: &str) -> [StringId; 2] {
    let bare = name.strip_suffix("()").unwrap_or(name);
    [StringId::from(bare), StringId::from(&*format!("{bare}()"))]
}

/// Both spellings of the word a call cursor is on, read out of the graph's own string table.
///
/// [`method_names`] cannot serve here, for [`find`]'s reason: a call is answered without its
/// resolution, so there may be no declaration to take a second spelling from, and where there is
/// one it may be spelled differently (`Foo.new` resolves to `Foo#initialize`). The cursor's id *is*
/// one of the two, and the graph holding the reference interned it, so the lookup always finds its
/// text.
fn call_spellings(graph: &Graph, spelled: &StringId) -> Vec<StringId> {
    let mut names = vec![*spelled];
    names.extend(
        graph
            .strings()
            .get(spelled)
            .map(|text| other_spelling(text.as_str())),
    );
    names
}

/// The other of the two, whichever one the cursor's word is.
fn other_spelling(text: &str) -> StringId {
    match text.strip_suffix("()") {
        // On the old name of an `alias`, where the other spelling is every real call.
        Some(bare) => StringId::from(bare),
        // On a call, where the other spelling is every alias of it.
        None => StringId::from(&*format!("{text}()")),
    }
}

/// The interned call-site spellings of every method among `declarations`.
fn method_names(graph: &Graph, declarations: &[DeclarationId], want: Spellings) -> Vec<StringId> {
    let mut names = Vec::new();
    for declaration in declarations
        .iter()
        .filter_map(|id| graph.declarations().get(id))
    {
        if !matches!(declaration, Declaration::Method(_)) {
            continue;
        }
        let name = declaration.name();
        let bare = name.rsplit_once('#').map_or(name, |(_, method)| method);
        for spelling in &spellings(bare)[..want.how_many()] {
            if !names.contains(spelling) {
                names.push(*spelling);
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
    use crate::analysis::MAX_REFERENCES;
    use crate::analysis::testing::*;
    use rubydex::{indexing::LanguageId, resolution::Resolver};

    use super::super::indexer;

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
    fn a_declaration_that_names_no_method_has_no_call_sites() {
        // `calls_to` is reached from a call-hierarchy item whose `data` is whatever the client sent
        // back, and a class is the shape it takes when an item is stale or invented. With no method
        // among the declarations there is no name to match, and the guard matters: `by_name` with
        // an empty list walks every reference in the workspace to compare each against nothing.
        let mut graph = Graph::new();
        assert!(indexer::index_source(
            &mut graph,
            "file:///fixture/hr.rb",
            "class Person\n  def shout\n  end\nend\n\nPerson.new.shout\n",
            &LanguageId::Ruby
        ));
        Resolver::new(&mut graph).resolve();
        let scope: HashSet<UriId> = graph.documents().keys().copied().collect();

        let [person] = declarations_named(&graph, "Person")[..] else {
            panic!("one declaration named Person");
        };
        assert!(calls_to(&graph, person, &scope).is_empty());

        // The method beside it, so the empty answer above is about the class, not the fixture.
        let [shout] = declarations_named(&graph, "#shout()")[..] else {
            panic!("one declaration named shout");
        };
        assert_eq!(calls_to(&graph, shout, &scope).len(), 1);
    }

    #[test]
    fn two_declarations_spelled_the_same_contribute_one_pair_of_spellings() {
        // `names` is scanned against every method reference in the workspace, once per reference,
        // so a duplicate is not just untidy: it is a second string comparison per call site for an
        // answer already known. Two classes defining `shout` is the ordinary way a resolution holds
        // more than one declaration of one name.
        let mut graph = Graph::new();
        assert!(indexer::index_source(
            &mut graph,
            "file:///fixture/hr.rb",
            "class Person\n  def shout\n  end\nend\n\nclass Siren\n  def shout\n  end\nend\n",
            &LanguageId::Ruby
        ));
        Resolver::new(&mut graph).resolve();

        let shouts = declarations_named(&graph, "#shout()");
        assert_eq!(shouts.len(), 2, "two classes, two declarations");

        // Both spellings, because rubydex records a call as `shout` and an `alias` as `shout()`,
        // and each once, however many declarations share it.
        let names = method_names(&graph, &shouts, Spellings::Written);
        assert_eq!(
            names,
            vec![StringId::from("shout"), StringId::from("shout()")]
        );

        // The call hierarchy's half of the same answer: one spelling, because an alias line is not
        // a call. `Spellings` is the whole difference between the two surfaces.
        assert_eq!(
            method_names(&graph, &shouts, Spellings::Called),
            vec![StringId::from("shout")]
        );
    }

    #[test]
    fn find_all_references_on_new_lists_call_sites_and_not_the_constructor() {
        // The redirect is a navigation affordance, and `references` is the one caller that must not
        // take it. `def initialize` is not a declaration of `new`, and a work list of `.new` call
        // sites with the constructor in it is noise, for every class whose constructor is in the
        // user's own code.
        let mut harness = Harness::new();
        harness.write("lib/shop.rb", CONSTRUCTORS);
        let source = "Money.new(1)\nMoney.new(2)\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        assert_eq!(
            harness.reference_list(&caller, source, "new(1)", true),
            vec!["main.rb:0:6", "main.rb:1:6"]
        );
    }

    /// A call, an `alias` and an `alias_method`, which is every way this name is written down.
    const ALIASED: &str = "\
class Person
  def shout
    \"!\"
  end

  def announce
    shout
  end

  alias yell shout
  alias_method :holler, :shout
end
";

    #[test]
    fn an_alias_is_a_use_of_the_name_and_both_spellings_are_listed() {
        // rubydex records the old name of an `alias` as `shout()` and a call as `shout`, and a
        // cursor is only ever on one of the two. A list built from the cursor's own id alone would
        // answer a rename with the alias lines missing, and renaming off it would leave `yell` and
        // `holler` naming a method that is gone.
        let mut harness = Harness::new();
        let uri = harness.write("lib/hr.rb", ALIASED);
        harness.index();

        assert_eq!(
            harness.reference_list(&uri, ALIASED, "shout\n  end", false),
            vec!["hr.rb:6:4", "hr.rb:9:13", "hr.rb:10:25"]
        );
    }

    #[test]
    fn a_cursor_on_the_old_name_of_an_alias_lists_the_calls_too() {
        // The same invariant from the other side, and a separate failure, not a restatement: on
        // `shout` in the `alias` line the cursor's own id is the parenthesised spelling, so a
        // single-spelling answer would hold the two alias lines and not one real call.
        let mut harness = Harness::new();
        let uri = harness.write("lib/hr.rb", ALIASED);
        harness.index();

        assert_eq!(
            harness.reference_list(&uri, ALIASED, "shout\n  alias_method", false),
            vec!["hr.rb:6:4", "hr.rb:9:13", "hr.rb:10:25"]
        );
    }

    #[test]
    fn a_use_in_a_spec_is_a_use_and_this_list_is_never_fenced() {
        // Stated here because two other surfaces do the opposite: completion and the name rung of
        // goto-definition drop a name only the suite can call. This request must not, and neither
        // must `rename`, which is built on it. A work list that quietly omitted the suite is a
        // refactor that breaks the suite, and the user never sees what was left out. `environment`
        // holds the table of which surface does which; this is the half with teeth.
        let mut harness = Harness::new();
        let declaration = "class Store\n  def ship\n  end\nend\n";
        let store = harness.write("app/models/store.rb", declaration);
        harness.write(
            "spec/models/store_spec.rb",
            "describe Store do\n  it \"ships\" do\n    Store.new.ship\n  end\nend\n",
        );
        harness.write("app/jobs/ship_job.rb", "Store.new.ship\n");
        harness.index();

        assert_eq!(
            harness.reference_list(&store, declaration, "ship", false),
            vec!["ship_job.rb:0:10", "store_spec.rb:2:14"]
        );
    }

    #[test]
    fn a_use_in_a_migration_is_a_use_too() {
        // The spec rule, for migrations. A migration is real Ruby somebody edits, and `rename` is
        // built on this list: a work list that quietly omitted `db/migrate` is a rename that leaves
        // a migration calling a method that no longer exists, and it fails years later on somebody
        // else's machine.
        let mut harness = Harness::new();
        let declaration = "class Store\n  def ship\n  end\nend\n";
        let store = harness.write("app/models/store.rb", declaration);
        harness.write(
            "db/migrate/20180101000000_ship_everything.rb",
            "class ShipEverything\n  def change\n    Store.new.ship\n  end\nend\n",
        );
        harness.write("app/jobs/ship_job.rb", "Store.new.ship\n");
        harness.index();

        assert_eq!(
            harness.reference_list(&store, declaration, "ship", false),
            vec!["ship_job.rb:0:10", "20180101000000_ship_everything.rb:2:14"]
        );
    }

    #[test]
    fn references_from_a_method_definition_find_its_call_sites() {
        // The other half of `method_references_are_name_based`: the cursor on `def shout`, not on a
        // call. What was defined decides the mechanism: a method is matched by name, a constant
        // through the resolution.
        let mut harness = Harness::new();
        let declaration = "class Person\n  def shout\n  end\n  attr_reader :volume\nend\n";
        let person = harness.write("app/person.rb", declaration);
        let main = harness.write(
            "app/main.rb",
            "Person.new.shout\nPerson.new.volume\nother.shout\n",
        );
        harness.index();
        // Open, so the ranges come from the buffer, not a re-read of disk: an open file is the one
        // a find-references result most likely names.
        harness.open(&main, "Person.new.shout\nPerson.new.volume\nother.shout\n");

        // Name-based, so `other.shout` is in the answer too: stated, not hidden.
        assert_eq!(
            harness.reference_list(&person, declaration, "shout", false),
            vec!["main.rb:0:11", "main.rb:2:6"]
        );
        // `attr_reader :volume` defines a method as much as `def` does.
        assert_eq!(
            harness.reference_list(&person, declaration, "volume", false),
            vec!["main.rb:1:11"]
        );
    }

    #[test]
    fn constant_references_are_resolved_rather_than_matched_by_name() {
        // The point of answering from a resolved graph. `Person` inside `module HR` and
        // `HR::Person` at the top level are the same constant written two ways, and the top-level
        // `Person` is a different class that shares a name. Any grep gets all three wrong.
        let mut harness = Harness::new();
        harness.write(
            "app/hr.rb",
            "module HR\n  class Person\n  end\n\n  class Team\n    def lead\n      Person.new\n    end\n  end\nend\n",
        );
        harness.write("app/person.rb", "class Person\nend\n");
        let source = "HR::Person.new\nPerson.new\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        // The cursor is on the `Person` half of `HR::Person`, which is what the user points at.
        let found = harness.reference_list(&uri, source, "Person.new", false);
        assert_eq!(found, vec!["hr.rb:6:6", "main.rb:0:4"], "{found:?}");
        // Line 1's bare `Person` is a different class and is not in the list. Nothing matching on
        // text could tell the two apart in either direction.
        assert!(!found.contains(&"main.rb:1:0".to_owned()), "{found:?}");
    }

    #[test]
    fn a_reference_is_the_name_the_user_wrote_not_the_call_around_it() {
        // rubydex fabricates a constant reference for every call with a constant receiver, so
        // `Person.new` can resolve against `Person`'s singleton class. Listing those bytes would
        // show a second, wider hit over text the user never wrote.
        let mut harness = Harness::new();
        harness.write("app/person.rb", "class Person\nend\n");
        let source = "Person.new\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let found = harness.references_at(&uri, source, "Person", false);
        assert_eq!(found.as_array().map(Vec::len), Some(1), "{found}");
        assert_eq!(
            found[0]["range"],
            serde_json::json!({
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 6 },
            }),
            "{found}"
        );
    }

    #[test]
    fn references_can_be_asked_for_from_the_definition() {
        // The common gesture: the cursor is on `class Person`, not on a use of it.
        let mut harness = Harness::new();
        let declaration = "class Person\nend\n";
        let person = harness.write("app/person.rb", declaration);
        let source = "Person.new\n";
        harness.write("app/main.rb", source);
        harness.index();

        assert_eq!(
            harness.reference_list(&person, declaration, "Person", false),
            vec!["main.rb:0:0"]
        );
        // With the declaration included, its own name span joins the list.
        assert_eq!(
            harness.reference_list(&person, declaration, "Person", true),
            vec!["main.rb:0:0", "person.rb:0:6"]
        );
    }

    #[test]
    fn method_references_are_name_based_and_will_over_report() {
        // Stated, not hidden: with no type inference, `shout` is `shout` whoever the receiver is.
        // `Megaphone#shout` is a different method, and it is in the answer anyway.
        let mut harness = Harness::new();
        harness.write(
            "app/person.rb",
            "class Person\n  def shout\n  end\nend\n\nclass Megaphone\n  def shout\n  end\nend\n",
        );
        let source = "Person.new.shout\nMegaphone.new.shout\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();

        let found = harness.reference_list(&uri, source, "shout", false);
        assert_eq!(found, vec!["main.rb:0:11", "main.rb:1:14"], "{found:?}");
    }

    #[test]
    fn a_call_to_a_method_that_was_never_defined_still_finds_its_call_sites() {
        // `define_method` and friends mean a name can have call sites and no declaration at all.
        // Routing through the resolution would answer `null` for exactly the code where the
        // editor's own word search helps least.
        let mut harness = Harness::new();
        let source = "widget.summon\nother.summon\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        assert!(!harness.has("#summon()"));

        assert_eq!(
            harness.reference_list(&uri, source, "summon", true),
            vec!["main.rb:0:7", "main.rb:1:6"]
        );
    }

    #[test]
    fn references_never_leave_the_users_own_code() {
        // A gem using the same constant is not an answer: nobody is going to edit it, and for the
        // name-based half a Rails bundle would drown the real hits.
        let (dir, _gem_home, env) = project_with_gem("Shouty = 1\nShouty\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "Shouty\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();

        let found = harness.reference_list(&uri, source, "Shouty", true);
        assert!(
            found.iter().all(|hit| hit.starts_with("main.rb")),
            "{found:?}"
        );
    }

    #[test]
    fn too_many_references_are_truncated_and_the_user_is_told() {
        // The cap is a safety property, not a preference: without it a name-based match in a large
        // workspace hands the editor a multi-megabyte response (`.new` in a large tree finds tens
        // of thousands). Truncating silently would be a wrong answer that looks exactly like a
        // right one, so it is said out loud.
        //
        // **The sentence names the file the list stops in.** [`ordered`] sorts by URI before the
        // caller truncates, so the cap drops every file after one point, not a scatter, and a
        // reader told only "the first 10,000" reads it as a sample of the whole workspace. There is
        // one file here, so the boundary is that file; the assertion covers the whole sentence,
        // which pins the shape.
        let mut harness = Harness::new();
        let source = "widget.ping\n".repeat(MAX_REFERENCES + 1);
        let uri = harness.write("app/main.rb", &source);
        harness.index();

        let found = harness.references_at(&uri, &source, "ping", false);
        assert_eq!(found.as_array().map(Vec::len), Some(MAX_REFERENCES));

        let stops_at = uri.to_file_path().unwrap();
        assert_eq!(
            harness.messages(),
            vec![messages::references_truncated(
                MAX_REFERENCES + 1,
                MAX_REFERENCES,
                Some(&stops_at),
            )],
            "a truncated answer has to say so, say by how much, and say where it stopped"
        );
    }

    #[test]
    fn the_bytes_rubydex_invented_are_never_listed_as_references() {
        // rubydex fabricates a constant reference to `<Person>` for every call with a `Person`
        // receiver, so the singleton class can be resolved. Those references are attached to the
        // singleton class, which is exactly what a cursor on `class << self` resolves to. Without
        // the filter this returns `Person.new` in main.rb: a span the user never wrote and cannot
        // rename.
        let mut harness = Harness::new();
        let declaration = "class Person\n  class << self\n    def build\n    end\n  end\nend\n";
        let person = harness.write("app/person.rb", declaration);
        harness.write("app/main.rb", "Person.new\n");
        harness.index();

        assert!(
            harness
                .references_at(&person, declaration, "self\n", false)
                .is_null(),
            "a call is not a reference to the callee's singleton class"
        );
    }
}
