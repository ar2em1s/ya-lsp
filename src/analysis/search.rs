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
//! beats a substring beats a subsequence, and a name the application loads beats one only the
//! suite does. See [`rank`] for why the workspace outranks the match rather than the other way
//! round — it is not the obvious order, and the obvious one measured badly — and
//! [`environment`](super::environment) for why this surface ranks the test trees down rather
//! than dropping them the way a completion list does.

use std::collections::HashSet;

use lsp_types::{SymbolKind, SymbolTag};
use rubydex::{
    model::{
        declaration::{Declaration, Namespace},
        graph::Graph,
        ids::{DeclarationId, UriId},
    },
    query::{self, MatchMode},
};

use super::{
    environment::{self, Trees},
    locator::{self, Site},
    render, symbols,
    synthesized::Synthesized,
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
pub fn search(
    graph: &Graph,
    synthesized: &Synthesized,
    query: &str,
    limit: usize,
    own: &HashSet<UriId>,
    names: environment::Names<'_>,
) -> Vec<Hit> {
    if limit == 0 {
        return Vec::new();
    }

    // Once per request, not once per candidate: `declaration_search` hands back six figures of
    // ids on a short query, and the alternative is that many path splits.
    let trees = Trees::of(graph, own, names);

    let mut ranked: Vec<Ranked> = query::declaration_search(graph, &[query], &MatchMode::Fuzzy)
        .into_iter()
        .filter_map(|id| {
            let declaration = graph.declarations().get(&id)?;
            // A placeholder the resolver invented for a namespace it never saw — `Foo::Bar`
            // mentioned by a reference to a `Foo` that does not exist — is filed as
            // `Namespace::Todo`, so `is_listable` is what actually turns those away and the
            // second test has never fired. It stays because the two ask different questions:
            // `Todo` is how *today's* rubydex spells "invented", while "nowhere to jump" is the
            // property the picker actually needs, for any declaration that ends up with no
            // definitions behind it.
            if !is_listable(declaration) || declaration.has_no_definitions() {
                return None;
            }
            let name = declaration.name();
            let placement = environment::placement(graph, declaration, own, &trees);
            Some(Ranked {
                own: placement.own,
                loadable: placement.loadable,
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
    //
    // **A generated row a declaration in the file itself already covers is dropped.** ActiveRecord's
    // query interface is declared once per relation class and once per base, and each of those now
    // carries the place Rails really wrote — so a query for `annotate` offered
    // `ActiveRecordRelation#annotate`, `ActiveRecord::Base.annotate` and `Story.annotate` above
    // `ActiveRecord::QueryMethods#annotate`: four rows opening one line, with the real declaration
    // the one pushed out of the top ten. Measured over 360 queries on six corpora, repeated files
    // inside a top ten went **489 to 708** without this clause.
    //
    // **Only where the file's own declaration is in the list**, which is narrower than "already
    // seen" and the difference is a defect the first version shipped for ten minutes. A `scope` is
    // declared twice on purpose — `Story.recent` and `Story::Relation#recent`, the same
    // `scope :recent` line, two different receivers — and rubydex records no declaration at a
    // `scope` call, so nothing else claims that span and **both copies are the answer**. Dropping
    // by first-seen took 44 real rows over the six corpora, every one of them a scope.
    //
    // The key carries the **bare name** as well as the span, because one line declares more than
    // one member: `attr_accessor :x` is `x` and `x=` at the same offsets.
    let rows: Vec<Offered> = ranked
        .into_iter()
        .filter_map(|entry| {
            let definition = locator::preferred_definition(graph, entry.id, own, names)?;
            let (name, container) = render::split_qualified(graph, entry.name);
            let site = locator::site(graph, synthesized, definition)?;
            let key = (
                site.uri.clone(),
                site.full.0,
                site.full.1,
                name.trim_start_matches("self.").to_owned(),
            );
            Some(Offered {
                stands_in: synthesized.is_generated(definition.uri_id()),
                key,
                hit: Hit {
                    name,
                    container,
                    kind: symbols::kind_of(definition),
                    tags: definition
                        .is_deprecated()
                        .then(|| vec![SymbolTag::DEPRECATED]),
                    site,
                },
            })
        })
        .collect();
    let written: HashSet<Opened> = rows
        .iter()
        .filter(|row| !row.stands_in)
        .map(|row| row.key.clone())
        .collect();
    rows.into_iter()
        .filter(|row| !row.stands_in || !written.contains(&row.key))
        .map(|row| row.hit)
        .collect()
}

/// What a row opens: the file, the construct's span in it, and the member's own name.
///
/// The name is there because one line declares more than one member — `attr_accessor :x` is `x`
/// and `x=` at the same offsets — and `self.` is stripped, because a class-side stand-in and the
/// instance `def` it points at are the same method to a reader.
type Opened = (String, u32, u32, String);

/// A row, with what it opens and whether this crate invented the declaration behind it.
struct Offered {
    hit: Hit,
    stands_in: bool,
    key: Opened,
}

/// A candidate, with everything the sort needs and nothing that costs a lookup.
struct Ranked<'g> {
    own: bool,
    tier: u8,
    /// Whether the application loads it at all, or only the suite does.
    loadable: bool,
    simple_len: usize,
    name: &'g str,
    id: DeclarationId,
}

/// The user's own code first, then match quality, then the environment, then the shorter name.
///
/// Putting the workspace ahead of match quality is the decision that makes this feature usable
/// rather than merely correct. A project has a few thousand declarations and its bundle has a
/// hundred and fifty thousand, so *any* ordering that ranks them together fills the picker with
/// gems: measured on a Rails app, "user" put `URI::Generic#user` and `Warden::Proxy#user` — both
/// exact matches, both useless — above the project's own `Users`. rust-analyzer reaches the same
/// conclusion from the other side and searches only the workspace unless asked; keeping the gems
/// in, below the fold, costs nothing and keeps the moat.
///
/// **The test trees go below match quality and not above it**, which is the opposite of where
/// the same fact sits in a completion list. The volume argument is the same — a Rails project's
/// spec tree is a third of its files and the `def`s inside a `describe` block all land on
/// `Object` — but the failure it would cause is not: a picker is how a name is *looked up*, so
/// burying an exact match under a substring match because the exact one is a spec would make the
/// only way of finding a spec helper stop working. Below the tier, the ordering only rearranges
/// candidates the query matched equally well, which is where every leak measured was.
fn rank(a: &Ranked<'_>, b: &Ranked<'_>) -> std::cmp::Ordering {
    b.own
        .cmp(&a.own)
        .then(b.tier.cmp(&a.tier))
        .then(b.loadable.cmp(&a.loadable))
        .then(a.simple_len.cmp(&b.simple_len))
        .then(a.name.len().cmp(&b.name.len()))
        // Never by `id`: it is a hash, so ties would shuffle between runs.
        .then(a.name.cmp(b.name))
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

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::MAX_WORKSPACE_SYMBOLS;
    use crate::analysis::testing::*;

    #[test]
    fn matching_is_case_insensitive_without_allocating_a_lowercased_copy() {
        // This runs once per candidate and a bundle has ~150k of them, so the folding is done
        // character by character rather than by lowercasing both sides. These are the cases
        // where that hand-rolled comparison could differ from the obvious one.
        assert!(equal_ci("Person", "person"));
        assert!(!equal_ci("Person", "persona"), "a prefix is not equality");
        assert!(!equal_ci("persona", "Person"), "nor is a suffix");
        assert!(!equal_ci("Person", "Persan"));

        assert!(starts_with_ci("PersonName", "person"));
        assert!(!starts_with_ci("Per", "person"), "the haystack runs out");

        assert!(contains_ci("ApplicationRecord", "record"));
        assert!(!contains_ci("ApplicationRecord", "reccord"));
        // An empty query matches everything, which is what makes an empty picker list the
        // workspace rather than nothing at all.
        assert!(contains_ci("anything", ""));
        assert!(contains_ci("", ""));
    }

    #[test]
    fn non_ascii_identifiers_fold_by_unicode_rather_than_by_byte() {
        // Ruby allows them and a UTF-8 workspace has them. ASCII on either side takes the fast
        // path; two non-ASCII characters need the full lowercase mapping, which is the only
        // route through `eq_ci`'s second line.
        assert!(equal_ci("Ünicorn", "ünicorn"));
        assert!(equal_ci("ПРИВЕТ", "привет"));
        assert!(!equal_ci("Ünicorn", "unicorn"), "not a transliteration");
        assert!(!equal_ci("привет", "приват"));
        // Mixed: one side ASCII means the ASCII rule decides, and `ü` is not `u`.
        assert!(!equal_ci("ü", "u"));
    }

    #[test]
    fn a_query_is_tiered_by_how_much_of_the_name_it_accounts_for() {
        // The ordering `search::rank` reads. Exact beats prefix beats substring beats a match
        // that only appears once the namespace is included.
        assert_eq!(tier("shout", "Person#shout()"), 4);
        assert_eq!(tier("SHOUT", "Person#shout()"), 4, "and case-insensitively");
        assert_eq!(tier("sho", "Person#shout()"), 3);
        assert_eq!(tier("hou", "Person#shout()"), 2);
        assert_eq!(
            tier("person#sh", "Person#shout()"),
            1,
            "the query is a path"
        );
        assert_eq!(tier("widget", "Person#shout()"), 0);
    }

    #[test]
    fn two_names_the_query_matches_alike_are_split_by_which_one_the_application_loads() {
        // Both are the user's own code, both are a prefix match, and both simple names are
        // eleven characters — so every field of `rank` above this one ties and the list used to
        // come back alphabetical, which put the double first. See `environment` for why this
        // surface sinks a test tree rather than dropping it.
        let mut harness = Harness::new();
        harness.write("app/models/store_finder.rb", "class StoreFinder\nend\n");
        harness.write("spec/support/store_double.rb", "class StoreDouble\nend\n");
        harness.index();

        assert_eq!(
            harness.symbol_names("store"),
            ["StoreFinder", "StoreDouble"]
        );
    }

    #[test]
    fn a_gem_s_generator_template_sinks_the_same_way_a_spec_does() {
        // The one tag `Trees` reads of the whole graph rather than of `own`: a tree
        // `rails generate` copies out of is unloadable wherever it ships from, and a gem's is
        // the common case — pundit's `application_policy.rb`, jbuilder's `api_controller.rb`,
        // active_model_serializers' `serializer.rb`. It sinks rather than drops for the row's
        // own reason: the picker is how a name is looked up, and a template is a real file
        // somebody edits.
        //
        // Measured over 247 picker queries on the six corpora, 6 lists changed: 5 template rows
        // sank, 2 fell past the 256-row cap and 2 real rows came in behind them, and no real
        // row was lost.
        //
        // Both rows are the project's own and both are a prefix match at the same tier, and
        // both simple names are eight characters — so every field of `rank` above this one
        // ties and the list came back alphabetical, which put the template first. `Trees`
        // reads the template tag off the **whole graph**, so a gem's sinks by the same term;
        // the project's own is what makes `own` tie, which is the only way to watch this one
        // field decide anything.
        let mut harness = Harness::new();
        harness.write("app/policies/store_zed.rb", "class StoreZed\nend\n");
        harness.write(
            "lib/generators/shouty/install/templates/store_abc.rb",
            "class StoreAbc\nend\n",
        );
        harness.index();

        assert_eq!(harness.symbol_names("store"), ["StoreZed", "StoreAbc"]);
    }

    #[test]
    fn an_exact_match_in_a_spec_still_beats_a_prefix_match_in_the_application() {
        // The ordering decision, and the reason the term sits *below* the tier rather than
        // above it: a picker is how a name is looked up, so burying an exact match because it
        // is a spec would break the only way there is of finding a spec helper. Below the tier
        // it only rearranges candidates the query matched equally well.
        let mut harness = Harness::new();
        harness.write(
            "app/models/store_double_writer.rb",
            "class StoreDoubleWriter\nend\n",
        );
        harness.write("spec/support/store_double.rb", "class StoreDouble\nend\n");
        harness.index();

        assert_eq!(
            harness.symbol_names("storedouble"),
            ["StoreDouble", "StoreDoubleWriter"]
        );
    }

    #[test]
    fn a_row_points_at_the_model_and_not_at_the_migration_that_reopened_it() {
        // The tie-break's third tree, and **the fixture has to get past two sorts to reach it**,
        // which is the honest size of this case. A file named after the constant wins outright,
        // and among the rest the one nearest the top of the tree wins — a migration is neither,
        // so it only sorts first where the application's own copy is both unnamed and deep:
        // one file declaring several classes, four directories down, reopened at the top of a
        // migration to backfill against the schema of the day. A tie-break and not a fence,
        // exactly as the spec case: the migration keeps its own rows, it just stops speaking
        // for the class.
        let mut harness = Harness::new();
        harness.write(
            "lib/a/b/c/models.rb",
            "class Story\n  def title\n  end\nend\n\nclass Tag\nend\n",
        );
        harness.write(
            "db/migrate/20180101000000_backfill_stories.rb",
            "class Story < ActiveRecord::Base\nend\n",
        );
        harness.index();

        let own = harness.analysis.own_documents();
        let graph = &harness.analysis.graph;
        let (id, _) = graph
            .declarations()
            .iter()
            .find(|(_, declaration)| declaration.name() == "Story")
            .expect("the class");
        let places: Vec<&str> = locator::definitions_of(graph, *id)
            .into_iter()
            .filter_map(|definition| graph.documents().get(definition.uri_id()))
            .map(rubydex::model::document::Document::uri)
            .collect();
        assert!(
            places
                .first()
                .is_some_and(|uri| uri.contains("db/migrate/")),
            "the fixture only means something while the migration sorts first: {places:?}"
        );

        let pointed =
            locator::preferred_definition(graph, *id, &own, environment::Names::default())
                .and_then(|definition| graph.documents().get(definition.uri_id()))
                .map(rubydex::model::document::Document::uri);
        assert!(
            pointed.is_some_and(|uri| uri.ends_with("lib/a/b/c/models.rb")),
            "{pointed:?}"
        );
    }

    #[test]
    fn a_row_points_at_the_copy_the_application_loads() {
        // An engine monorepo, which is the shape where this can go wrong: `definitions_of`
        // orders by path, `admin/` sorts before `core/`, and both files are named after the
        // constant — so the spec copy is genuinely first and the row would jump into it.
        // `preferred_definition` breaks that tie towards the application, which is a tie-break
        // and not a fence: a class written *only* in a spec still gets a row, pointing at the
        // spec.
        let mut harness = Harness::new();
        harness.write(
            "core/app/models/store.rb",
            "class Store\n  def ship\n  end\nend\n",
        );
        harness.write(
            "admin/spec/support/store.rb",
            "class Store\n  def stub_ship\n  end\nend\n",
        );
        harness.index();

        let own = harness.analysis.own_documents();
        let graph = &harness.analysis.graph;
        let (id, _) = graph
            .declarations()
            .iter()
            .find(|(_, declaration)| declaration.name() == "Store")
            .expect("the class");
        let places: Vec<&str> = locator::definitions_of(graph, *id)
            .into_iter()
            .filter_map(|definition| graph.documents().get(definition.uri_id()))
            .map(rubydex::model::document::Document::uri)
            .collect();
        assert!(
            places
                .first()
                .is_some_and(|uri| uri.ends_with("admin/spec/support/store.rb")),
            "the fixture only means something while the spec sorts first: {places:?}"
        );

        let pointed =
            locator::preferred_definition(graph, *id, &own, environment::Names::default())
                .and_then(|definition| graph.documents().get(definition.uri_id()))
                .map(rubydex::model::document::Document::uri);
        assert!(
            pointed.is_some_and(|uri| uri.ends_with("core/app/models/store.rb")),
            "{pointed:?} out of {places:?}"
        );
    }

    #[test]
    fn the_symbol_picker_answers_nothing_when_asked_for_nothing() {
        // `MAX_WORKSPACE_SYMBOLS` is a latency control — a subsequence match on one character
        // hits most of a bundle on every keystroke — so a zero limit has to stop before the
        // search runs at all rather than after it.
        let mut harness = Harness::new();
        harness.write("app/person.rb", "class Person\nend\n");
        harness.index();

        let own = harness.analysis.own_documents();
        assert!(
            super::search(
                &harness.analysis.graph,
                &harness.analysis.synthesized,
                "Person",
                1,
                &own,
                environment::Names::default()
            )
            .len()
                == 1,
            "the fixture has to match at all for a zero limit to mean anything"
        );
        assert!(
            super::search(
                &harness.analysis.graph,
                &harness.analysis.synthesized,
                "Person",
                0,
                &own,
                environment::Names::default()
            )
            .is_empty()
        );
    }

    #[test]
    fn a_symbol_search_ranks_the_name_the_user_typed_first() {
        // rubydex's fuzzy match is a subsequence test, so all four of these match `user` and it
        // scores them identically. Without a ranking of our own the picker's first row is
        // whichever one the hash map happened to yield.
        let mut harness = Harness::new();
        harness.write(
            "app/models.rb",
            "class UserSerializer\nend\n\nclass User\nend\n\nclass SuperUserPolicy\nend\n\nclass Ultra\n  def send_error(u)\n  end\nend\n",
        );
        harness.index();

        let found = harness.symbol_names("user");
        assert_eq!(
            found,
            vec![
                "User",
                "UserSerializer",
                "SuperUserPolicy",
                "Ultra#send_error"
            ],
            "{found:?}"
        );
    }

    const PICKER: &str = "\
class User
end

class UserSerializer
end

class SuperUserPolicy
end

module Admin
  class User
  end
end

class Ultra
  def send_error(u)
  end
end

class Account
  USER_LIMIT = 10

  def user
  end

  def user_name
  end
end
";

    /// The picker's rows the way it draws them: the name, the container the client shows beside
    /// it, and the file it would jump to.
    ///
    /// The file is here because `own` — the user's code before a gem's — is the first field the
    /// ranking sorts on and the decision the feature stands on, and it is invisible in a list of
    /// names.
    fn picker_rows(harness: &mut Harness, query: &str) -> Vec<String> {
        let found = harness.symbol_search(query);
        let Some(symbols) = found.as_array() else {
            return Vec::new();
        };
        symbols
            .iter()
            .map(|symbol| {
                let file = symbol["location"]["uri"]
                    .as_str()
                    .unwrap_or_default()
                    .rsplit('/')
                    .next()
                    .unwrap_or_default();
                format!(
                    "{}  {}  {file}",
                    symbol["name"].as_str().unwrap_or_default(),
                    symbol["containerName"].as_str().unwrap_or("-"),
                )
            })
            .collect()
    }

    /// A project and a gem, opened by `picker` below.
    ///
    /// Every name here matches `user`, and they are chosen so that each is the *only* one that
    /// separates two of the ranking's fields: `User` and `Admin::User` differ only in qualified
    /// length, `Account#user` and `#user_name` only in simple length, `USER_LIMIT` only in case,
    /// `SuperUserPolicy` only in where the match falls, and `Ultra#send_error` matches nothing
    /// but a subsequence. The gem's `UserAgent` is an exact match on a name the project does not
    /// have, so its position is the whole of what `own` decides.
    /// The gem home comes back with the harness because dropping it deletes the gem, and the
    /// harness holds only the project's own directory.
    fn picker() -> (Harness, tempfile::TempDir) {
        let (dir, gem_home, env) =
            project_with_gem("class User\nend\n\nmodule Shouty\n  class UserAgent\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/accounts.rb", PICKER);
        harness.index();
        harness.index_gems();
        (harness, gem_home)
    }

    #[test]
    fn the_rows_of_a_symbol_search_in_the_order_the_picker_draws_them() {
        // `ANCESTRY`'s treatment for the other ranked list in the crate. Every field of `rank`
        // is separated by exactly one adjacent pair here, so the whole list is the ordering
        // stated once: `own` before everything (the gem's exact `UserAgent` is last, under a
        // subsequence match in the project), then match quality, then the shorter simple name,
        // then the shorter qualified one, then alphabetical.
        //
        // Four of these rows were asserted nowhere before — the constant, the nested class, the
        // second method and the gem — and a ranking is not a set of rows, it is their order.
        let (mut harness, _gem_home) = picker();

        assert_eq!(
            picker_rows(&mut harness, "user"),
            [
                "User  -  accounts.rb",
                "User  Admin  accounts.rb",
                "user  Account  accounts.rb",
                "user_name  Account  accounts.rb",
                "USER_LIMIT  Account  accounts.rb",
                "UserSerializer  -  accounts.rb",
                "SuperUserPolicy  -  accounts.rb",
                "send_error  Ultra  accounts.rb",
                "UserAgent  Shouty  shouty.rb",
            ]
        );
    }

    #[test]
    fn a_one_letter_query_ranks_rather_than_gives_up() {
        // The query a picker actually receives first, and the one every ordering decision was
        // made for: on a Rails bundle a single letter subsequence-matches most of a hundred and
        // fifty thousand declarations. Here it adds `Ultra`, `Account` and `Shouty` to the list
        // above — and puts none of them above a name the letter actually starts.
        let (mut harness, _gem_home) = picker();

        assert_eq!(
            picker_rows(&mut harness, "u"),
            [
                "User  -  accounts.rb",
                "User  Admin  accounts.rb",
                "user  Account  accounts.rb",
                "Ultra  -  accounts.rb",
                "user_name  Account  accounts.rb",
                "USER_LIMIT  Account  accounts.rb",
                "UserSerializer  -  accounts.rb",
                "Account  -  accounts.rb",
                "SuperUserPolicy  -  accounts.rb",
                "send_error  Ultra  accounts.rb",
                "UserAgent  Shouty  shouty.rb",
                "Shouty  -  shouty.rb",
            ]
        );
    }

    #[test]
    fn a_query_that_spells_a_path_is_matched_on_the_path() {
        // `rank`'s tier 1: the query matched nothing in the simple name and everything in the
        // qualified one, which is what somebody typing `Account#user` means and the only tier
        // that cannot be reached by typing a name.
        let (mut harness, _gem_home) = picker();

        assert_eq!(
            picker_rows(&mut harness, "Account#user"),
            [
                "user  Account  accounts.rb",
                "user_name  Account  accounts.rb",
            ]
        );
    }

    #[test]
    fn a_symbol_search_answers_the_same_list_however_the_query_is_cased() {
        // Every comparison in `tier` is case-insensitive, and a picker that reordered itself
        // when the user pressed shift would be worse than one that did not rank at all.
        let (mut harness, _gem_home) = picker();

        let typed = picker_rows(&mut harness, "user");
        assert_eq!(picker_rows(&mut harness, "User"), typed);
        assert_eq!(picker_rows(&mut harness, "USER"), typed);
    }

    #[test]
    fn a_symbol_search_prefers_the_users_own_code_to_a_gem() {
        // A gem reopening a class the project also defines is one declaration with definitions
        // in both, so there is one row and it has to point somewhere. It points at the file the
        // user can edit — and at the same place goto-definition would have taken them.
        let (dir, _gem_home, env) = project_with_gem("class Megaphone\n  def blare\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        harness.write("app/megaphone.rb", "class Megaphone\nend\n");
        harness.index();
        harness.index_gems();

        let found = harness.symbol_search("Megaphone");
        let symbols = found.as_array().expect("an array");
        assert!(
            symbols[0]["location"]["uri"]
                .as_str()
                .unwrap_or_default()
                .ends_with("app/megaphone.rb"),
            "{found}"
        );
        // The gem's method is in the answer too — `Megaphone#blare` contains the query as a
        // subsequence — but a name that *is* the query outranks a name that merely contains it.
        assert_eq!(
            harness.symbol_names("Megaphone"),
            vec!["Megaphone", "Megaphone#blare"]
        );
    }

    #[test]
    fn a_symbol_search_reaches_into_the_gems() {
        // The moat again: a class that exists nowhere in the project is still findable.
        let (dir, _gem_home, env) =
            project_with_gem("module Shouty\n  class Megaphone\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        harness.write("app/main.rb", "1\n");
        harness.index();
        assert!(harness.symbol_search("Megaphone").is_null());

        harness.index_gems();
        assert_eq!(harness.symbol_names("Megaphone"), vec!["Shouty#Megaphone"]);
    }

    #[test]
    fn symbols_are_spelled_the_way_the_outline_spells_them() {
        let mut harness = Harness::new();
        harness.write(
            "app/person.rb",
            "class Person\n  MAX_AGE = 120\n  attr_reader :name\n\n  class << self\n    def build\n    end\n  end\nend\n\nmaker = Class.new do\n  def weld\n  end\nend\n",
        );
        harness.index();

        assert_eq!(harness.symbol_names("build"), vec!["Person#self.build"]);
        // A `Class.new` nothing bound to a constant is not a row — nobody can search for a
        // document id and an offset — but its methods are, and the container beside one is
        // where the key used to be printed.
        assert_eq!(harness.symbol_names("weld"), vec!["Class.new#weld"]);
        assert_eq!(harness.symbol_names("MAX_AGE"), vec!["Person#MAX_AGE"]);
        assert_eq!(harness.symbol_names("name"), vec!["Person#name"]);
        // The singleton class itself has no name a person would ever search for: rubydex calls
        // it `Person::<Person>`, and it is not a thing you can jump to.
        assert!(
            !harness
                .symbol_names("Person")
                .iter()
                .any(|name| name.contains('<')),
            "{:?}",
            harness.symbol_names("Person")
        );
    }

    #[test]
    fn a_symbol_search_is_capped() {
        // At gem scale an unbounded answer is the failure mode: a two-letter query subsequence-
        // matches a large fraction of a bundle, on every keystroke.
        let mut harness = Harness::new();
        let classes: String = (0..MAX_WORKSPACE_SYMBOLS + 50)
            .map(|index| format!("class Widget{index}\nend\n"))
            .collect();
        harness.write("app/widgets.rb", &classes);
        harness.index();

        let found = harness.symbol_search("Widget");
        assert_eq!(
            found.as_array().map(Vec::len),
            Some(MAX_WORKSPACE_SYMBOLS),
            "the cap is not being applied"
        );
    }

    /// The picker's half of the row that gave the query interface a place: one `def`, one row.
    #[test]
    fn a_generated_stand_in_for_a_def_the_list_already_holds_is_not_a_second_row() {
        let mut harness = Harness::new();
        harness.write(
            "lib/active_record/relation/query_methods.rb",
            "module ActiveRecord\n  module QueryMethods\n    def annotate(*args)\n    end\n  end\nend\n",
        );
        harness.write(
            "lib/active_record/relation.rb",
            "module ActiveRecord\n  class Relation\n    include QueryMethods\n  end\nend\n",
        );
        harness.write(
            "lib/active_record/base.rb",
            "module ActiveRecord\n  class Base\n  end\nend\n",
        );
        harness.write(
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base\nend\n",
        );
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\nend\n",
        );
        harness.index();

        // `ActiveRecordRelation#annotate` and `ActiveRecord::Base.annotate` are this crate's own
        // declarations and both now point at the `def` below them. The row a file actually wrote
        // is the one a reader wants, and the other two open the same line.
        assert_eq!(
            harness.symbol_names("annotate"),
            ["ActiveRecord::QueryMethods#annotate"]
        );
    }

    /// And the clause that keeps it from taking real rows: a `scope` is two declarations at one
    /// line **on purpose**, and nothing else in the graph claims that line.
    #[test]
    fn both_halves_of_a_scope_are_still_offered_although_they_share_a_line() {
        let mut harness = Harness::new();
        harness.write(
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base\nend\n",
        );
        harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  scope :recent, -> { order(id: :desc) }\nend\n",
        );
        harness.index();

        // `Story.recent` starts a chain and `Story::Relation#recent` continues one. Two
        // receivers, one `scope :recent` line, and both are the answer.
        let mut found = harness.symbol_names("recent");
        found.sort();
        assert_eq!(found, ["Story#self.recent", "Story::Relation#recent"]);
    }
}
