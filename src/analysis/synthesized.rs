//! Where a declaration that no file declares was really written.
//!
//! Every generator works the same way: read something the repository already states — a column in
//! `db/schema.rb`, a `belongs_to :user` — write RBS that says it, and hand that text to the two
//! functions that already turn RBS into typed, navigable declarations. Neither
//! [`indexer::index_source`] nor [`Types::harvest`] knows where its text came from, which is what
//! makes all of them one feature rather than a dozen.
//!
//! Not caring has one cost, and this module is it. Generated text is indexed under a URI with no
//! file behind it, at offsets into bytes nobody can open, while every navigational answer in the
//! crate turns a declaration into a place by asking the graph where its definitions are. So a
//! generated definition has to be translated back to the line that implied it before it reaches
//! an editor, and where nothing recorded that line it has to be **dropped**: a card that is right
//! above a jump into a file the user does not have is worse than not typing the receiver at all,
//! because the card is checked once and the jump is trusted forever.
//!
//! [`super::erb::ruby_view`] can blank a template in place because its output is the same length
//! as its input, so there is only ever one coordinate system. Generated RBS has no such
//! relationship to the Ruby that implied it, so the relationship is written down as the text is.
//!
//! # Keyed by definition, not by declaration
//!
//! A declaration is the *merge* of every definition of a name: `Story#title()` generated from
//! `t.string "title"` and `Story#title()` written as a `def` in `app/models/story.rb` are one
//! declaration with two definitions — the ordinary case, and the reason the schema is a *derived*
//! answer rather than a resolved one. Keying on the declaration would rewrite its location and
//! take the user's own `def` off the map. So the key is the definition's document and offset, the
//! real definition is left where it is, and only the generated one is translated.
//!
//! # One generated document per source file, by naming rather than by bookkeeping
//!
//! [`generated_uri`] derives the generated document's URI from the source file's, so recording
//! twice for one source cannot leave two answers anywhere: rubydex replaces a document indexed
//! under a URI it already holds, and the table is a map keyed by that same URI. There is no
//! second key to leak and no list to prune — which matters because the failure that would cause,
//! an edit to `db/schema.rb` leaving the old column behind, is silent.
//!
//! # The URI is deliberately not a file URI
//!
//! `ya-lsp-generated:file:///…` is a URI rubydex is happy to index under (it files its own
//! built-ins as `rubydex:built-in`) and that [`DocUri::from_uri_str`] refuses, because
//! `Url::to_file_path` refuses it. That is the backstop under everything above: even with an
//! empty table and a bug in this module, a generated document cannot become a `Location`, a
//! symbol row or a diagnostic — and it is not under the workspace prefix either, so
//! [`super::Analysis::is_own_code`] already says no. The table decides which generated
//! definitions become a *useful* place; the scheme decides that none can become a wrong one.

use std::collections::HashMap;

use rubydex::{
    indexing::LanguageId,
    model::{graph::Graph, ids::UriId},
};

use super::{indexer, locator::Site, types::Types};
use crate::workspace::DocUri;

/// The scheme generated documents are filed under.
///
/// The source's own URI follows it whole, so the key is unique by construction and a log line
/// says which file the declarations came from. Kept as a prefix on a non-`file` scheme rather
/// than as a path — a path under the workspace root would be indexable by an `index.include`
/// glob, and a path outside it would still be a `Location` an editor would try to open.
pub(super) const GENERATED_SCHEME: &str = "ya-lsp-generated:";

/// Everything ya-lsp wrote itself, and where each piece of it was really declared.
#[derive(Debug, Default)]
pub struct Synthesized {
    /// Generated document -> what was written into it, and the spans that point somewhere.
    ///
    /// A document with no mappings is still *in* the map, and the difference is the whole
    /// point: present-with-no-mapping answers [`Origin::Unknown`] and is not a jump target,
    /// while absent answers [`Origin::OnDisk`] and is left alone.
    documents: HashMap<UriId, Generated>,
    /// How many times a generated document has actually been handed to the graph.
    ///
    /// The third instrument beside [`Analysis::passes`](super::Analysis) and
    /// [`Analysis::walks`](super::Analysis), and needed for their reason: the claim is that the
    /// graph is left alone, and a graph rebuilt into exactly the shape it already had is
    /// indistinguishable from one that was not touched. Only a counter says which.
    ///
    /// Monotonic, and deliberately not reset by [`Self::clear`]: what a test asserts is that
    /// it did **not move** across an edit, which a counter that starts again cannot say.
    indexed: u64,
}

/// One generated document, as it was handed over.
///
/// The text is kept so that [`Synthesized::record`] can tell a regeneration that changed
/// something from one that did not — see there for what that is worth. It is a few tens of
/// kilobytes for a real schema, against a graph that holds megabytes of Ruby.
#[derive(Debug)]
struct Generated {
    rbs: String,
    mappings: Vec<Mapping>,
}

/// One span of generated text, and the line that implied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    /// The byte range in the generated document this covers, end exclusive.
    ///
    /// Mappings may nest — the class a `create_table` implies contains the methods its columns
    /// imply — and the narrowest one containing a definition wins, exactly as it does for the
    /// spans under the cursor in [`super::locator::locate`].
    pub generated: (u32, u32),
    /// Where the editor goes instead: a real file, and the two spans a link needs.
    pub declared: Site,
}

/// What this table knows about the document a definition sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin<'s> {
    /// Nothing generated this document. The graph's own answer is the right one.
    OnDisk,
    /// Generated, and this is the file and the spans it was really declared at.
    Declared(&'s Site),
    /// Generated, and nothing said where from. Not a place, and must not be offered as one.
    Unknown,
}

impl Synthesized {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Index `rbs` as the declarations `source` implies, replacing whatever it implied before.
    ///
    /// Both halves of the mechanism live in one function so they cannot drift: the graph learns
    /// the declarations and [`Types`] learns what they return. Both are keyed in a way that
    /// overwrites rather than accumulates — rubydex drops the old document with this URI, and a
    /// harvest overwrites per method — so a source re-read after an edit leaves no stale answer.
    ///
    /// **Two questions and not one.** The graph holds the *text*, and the side table holds where
    /// each line of it came from. A regeneration can move one without the other, and the
    /// commonest edit in Rails moves only the second: a provenance comment names a file and the
    /// macro it read — never a line number — so pressing return above a `has_many` re-derives RBS
    /// that is **byte-identical** with mappings that have all shifted by one line. Handing that to
    /// rubydex makes it drop the generated document, invalidate every declaration the old one
    /// touched, cascade to their members, singletons and descendants, unresolve every dependent
    /// name and link it all again — to arrive at the graph it already had.
    ///
    /// So the text decides whether the **graph** is touched, and the mappings are updated either
    /// way. The saving is large and scales with the *graph* rather than the document: on a big
    /// workspace one keystroke costs the better part of a second to re-index, against a fraction
    /// of a millisecond to compare every mapping. That is why it is invisible on a small one.
    ///
    /// It does **not** mark the graph dirty; the caller does, exactly as for
    /// [`super::Analysis::index_buffer`] and for the signature files on the gem batch. Nor does it
    /// blank `interface` blocks the way that path has to: this text was written here, and that
    /// rule exists for signature files somebody else wrote.
    ///
    /// A method that disappears from the regenerated text leaves its entry in [`Types`], and that
    /// entry is unreachable for the reason [`Types::harvest`] already gives: a lookup only happens
    /// after rubydex has found the member, and rubydex no longer has one.
    pub fn record(
        &mut self,
        graph: &mut Graph,
        types: &mut Types,
        source: &DocUri,
        rbs: &str,
        mappings: Vec<Mapping>,
    ) -> String {
        let uri = generated_uri(source);
        let id = UriId::from(uri.as_str());
        if let Some(held) = self.documents.get_mut(&id)
            && held.rbs == rbs
        {
            // The graph already holds exactly this text, so there is nothing for it to learn
            // and everything for it to lose by being told again. The mappings are this table's
            // own and are taken whether they moved or not: comparing them costs a scan of every
            // span and assigning them costs a pointer, and a mapping left stale is a jump that
            // lands on a line the declaration has moved off.
            held.mappings = mappings;
            return uri;
        }
        // A generator that writes RBS the parser rejects would otherwise index text nothing
        // can read and type nothing at all, *silently* — the two consumers fail independently
        // and neither says so. One file's worth of declarations is the right blast radius for
        // a bug in a generator, and a warning naming the file is what makes it findable.
        if !types.harvest(rbs) {
            tracing::warn!("generated RBS from {source} does not parse; it declares nothing");
            self.forget(graph, source);
            return uri;
        }
        // The bulkhead reaches this route too, and the recovery here is the one
        // the branch above already wrote: a generator whose RBS crashes the indexer declares
        // one file's worth of nothing, rather than taking the session with it. There is no
        // skip list to join — a generated document has no file behind it, so nothing re-reads
        // it and the next edit to the *source* asks the generator again.
        if !indexer::index_source(graph, &uri, rbs, &LanguageId::Rbs) {
            tracing::warn!("indexing the RBS generated from {source} crashed; it declares nothing");
            self.forget(graph, source);
            return uri;
        }
        self.indexed += 1;
        self.documents.insert(
            id,
            Generated {
                rbs: rbs.to_owned(),
                mappings,
            },
        );
        uri
    }

    /// Drop everything `source` implied, from the table and from the graph, and say whether
    /// there was anything to drop.
    ///
    /// Called wherever a document leaves the index, so that a deleted `db/schema.rb` takes its
    /// columns with it rather than leaving declarations nothing can reach and nothing will ever
    /// refresh.
    pub fn forget(&mut self, graph: &mut Graph, source: &DocUri) -> bool {
        let uri = generated_uri(source);
        if self.documents.remove(&UriId::from(uri.as_str())).is_none() {
            return false;
        }
        graph.delete_document(&uri);
        true
    }

    /// The RBS `source` last declared, or `None` where it has declared nothing.
    ///
    /// The text rather than its effects. Half of what a generator gets wrong is visible only in
    /// what it wrote — an optionality, an arity, which of two annotations won — and asserting
    /// those through the graph and the type table asserts three things at once.
    #[must_use]
    pub fn text(&self, source: &DocUri) -> Option<&str> {
        self.documents
            .get(&UriId::from(generated_uri(source).as_str()))
            .map(|generated| generated.rbs.as_str())
    }

    /// Where a definition at `offset` of `document` was really written.
    #[must_use]
    pub fn origin(&self, document: &UriId, offset: u32) -> Origin<'_> {
        let Some(generated) = self.documents.get(document) else {
            return Origin::OnDisk;
        };
        generated
            .mappings
            .iter()
            .filter(|mapping| mapping.generated.0 <= offset && offset < mapping.generated.1)
            .min_by_key(|mapping| mapping.generated.1 - mapping.generated.0)
            .map_or(Origin::Unknown, |mapping| {
                Origin::Declared(&mapping.declared)
            })
    }

    /// Whether ya-lsp wrote this document itself.
    #[must_use]
    pub fn is_generated(&self, document: &UriId) -> bool {
        self.documents.contains_key(document)
    }

    /// How many documents ya-lsp has generated. For the log line, and for the tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// How many times a generated document has been handed to the graph.
    ///
    /// The field this reads carries the argument for counting at all: a graph rebuilt into
    /// exactly the shape it already had is indistinguishable from one that was left alone.
    #[must_use]
    pub fn indexed(&self) -> u64 {
        self.indexed
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Every generated document's text, keyed by the `UriId` it is filed under.
    ///
    /// For the one test that has to compare two settles byte for byte: a pass over an unchanged
    /// workspace writes what the pass before it wrote, and the only way
    /// to say that is to look at all of it rather than at the documents a fixture remembers.
    #[cfg(test)]
    pub(super) fn every_document(&self) -> std::collections::BTreeMap<UriId, &str> {
        self.documents
            .iter()
            .map(|(uri, generated)| (*uri, generated.rbs.as_str()))
            .collect()
    }

    /// Throw the table away, for a graph that is being built again from nothing.
    pub fn clear(&mut self) {
        self.documents.clear();
    }
}

/// The URI the declarations implied by `source` are indexed under.
///
/// A pure function of the source URI, which is what makes "one generated document per source
/// file" a property of the naming instead of a list somebody has to keep pruned. Total, and
/// deliberately: a fallible derivation would put a silent "generated nothing" arm on the one
/// path where being silent is the failure being designed against.
#[must_use]
pub fn generated_uri(source: &DocUri) -> String {
    format!("{GENERATED_SCHEME}{}", source.as_str())
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn source() -> DocUri {
        DocUri::from_path(Path::new("/project/db/schema.rb")).expect("a file uri")
    }

    fn site(uri: &DocUri, at: (u32, u32)) -> Site {
        Site {
            uri: uri.as_str().to_owned(),
            full: at,
            selection: at,
        }
    }

    fn mapping(generated: (u32, u32), declared: Site) -> Mapping {
        Mapping {
            generated,
            declared,
        }
    }

    /// How many constructs rubydex filed for a document.
    ///
    /// Definitions rather than declarations, deliberately: a declaration is what the *resolver*
    /// merges definitions into, and `Resolver::resolve` has exactly one call site in this crate
    /// (`analysis::resolve`, which is where its panic-after-delete is contained). What indexing
    /// alone produces is definitions, and they are enough to tell replacing from appending. The
    /// declaration side of the same question is asked end to end, through the server, in
    /// `analysis::tests`.
    fn indexed(graph: &Graph, uri: &str) -> usize {
        graph
            .documents()
            .get(&UriId::from(uri))
            .map_or(0, |document| document.definitions().len())
    }

    #[test]
    fn a_generated_document_is_not_a_file_and_cannot_be_turned_into_one() {
        let uri = generated_uri(&source());
        assert_eq!(uri, "ya-lsp-generated:file:///project/db/schema.rb");
        // The backstop the module documents: whatever this table says, nothing that turns a
        // graph URI into something an editor opens will accept one of these.
        assert!(DocUri::from_uri_str(&uri).is_none());
    }

    #[test]
    fn a_document_nobody_generated_is_left_exactly_where_it_is() {
        let table = Synthesized::new();
        assert!(table.is_empty());
        assert_eq!(
            table.origin(&UriId::from("file:///project/app/models/story.rb"), 12),
            Origin::OnDisk
        );
        assert!(!table.is_generated(&UriId::from("file:///project/app/models/story.rb")));
    }

    #[test]
    fn a_generated_offset_with_no_mapping_is_not_a_place() {
        let (mut graph, mut types) = (Graph::new(), Types::new());
        let mut table = Synthesized::new();
        let uri = table.record(
            &mut graph,
            &mut types,
            &source(),
            "class Story\nend\n",
            Vec::new(),
        );

        let id = UriId::from(uri.as_str());
        assert!(table.is_generated(&id));
        assert_eq!(table.len(), 1);
        assert_eq!(indexed(&graph, &uri), 1);
        // In the table, so not left alone — and with nothing to point at, so not a place.
        assert_eq!(table.origin(&id, 0), Origin::Unknown);
    }

    #[test]
    fn the_narrowest_mapping_containing_the_offset_wins() {
        let source = source();
        let (klass, column) = (site(&source, (10, 40)), site(&source, (20, 34)));
        let mut table = Synthesized::new();
        table.documents.insert(
            UriId::from("ya-lsp-generated:x"),
            Generated {
                rbs: String::new(),
                mappings: vec![
                    mapping((0, 100), klass.clone()),
                    mapping((12, 40), column.clone()),
                ],
            },
        );

        let id = UriId::from("ya-lsp-generated:x");
        // Inside both: the inner one is what wrote those bytes.
        assert_eq!(table.origin(&id, 20), Origin::Declared(&column));
        // Inside the outer one only.
        assert_eq!(table.origin(&id, 5), Origin::Declared(&klass));
        // The end is exclusive, so a mapping cannot claim the byte after its own text.
        assert_eq!(table.origin(&id, 40), Origin::Declared(&klass));
        assert_eq!(table.origin(&id, 100), Origin::Unknown);
    }

    #[test]
    fn recording_a_source_again_replaces_what_it_declared_before() {
        let (mut graph, mut types) = (Graph::new(), Types::new());
        let mut table = Synthesized::new();
        let source = source();

        let uri = table.record(
            &mut graph,
            &mut types,
            &source,
            "class Story\n  def title: () -> String\n  def byline: () -> String\nend\n",
            vec![
                mapping((14, 38), site(&source, (60, 80))),
                mapping((41, 66), site(&source, (90, 110))),
            ],
        );
        assert_eq!(indexed(&graph, &uri), 3);
        assert_eq!(table.indexed(), 1);

        table.record(
            &mut graph,
            &mut types,
            &source,
            "class Story\n  def byline: () -> String\nend\n",
            vec![mapping((14, 39), site(&source, (90, 110)))],
        );

        // One document holding two constructs rather than one holding five, one set of
        // mappings rather than three, and the column that went away answers nothing at all.
        assert_eq!(table.len(), 1);
        // And the graph really was told, which is the other half of the gate: text that changed
        // is handed over, whatever it costs.
        assert_eq!(table.indexed(), 2);
        assert_eq!(indexed(&graph, &uri), 2);
        assert_eq!(
            table.origin(&UriId::from(uri.as_str()), 20),
            Origin::Declared(&site(&source, (90, 110)))
        );
        assert_eq!(
            table.origin(&UriId::from(uri.as_str()), 50),
            Origin::Unknown
        );
    }

    #[test]
    fn recording_the_same_text_again_does_not_hand_it_over_again() {
        let (mut graph, mut types) = (Graph::new(), Types::new());
        let mut table = Synthesized::new();
        let source = source();
        let rbs = "class Story\n  def title: () -> String\nend\n";
        let mappings = vec![mapping((14, 38), site(&source, (60, 80)))];

        let uri = table.record(&mut graph, &mut types, &source, rbs, mappings.clone());
        assert_eq!(indexed(&graph, &uri), 2);
        assert_eq!(table.indexed(), 1);

        // Taken out from under it, so that a second `record` doing any work at all would put
        // it back. Nothing else in the crate deletes a document it means to keep; it is the
        // only way to watch for a call that is deliberately not made.
        graph.delete_document(&uri);
        table.record(&mut graph, &mut types, &source, rbs, mappings);
        assert_eq!(indexed(&graph, &uri), 0);
        assert_eq!(table.indexed(), 1);

        // **The text is the only thing the graph holds.** A mapping that moved is not a change
        // to it — the same column, one line further down `db/schema.rb` — so the table takes the
        // new spans and the graph is still not told, which is what the document deleted out from
        // under it says: it stays deleted. Re-indexing here costs 940 ms in front of a keystroke
        // on discourse.
        table.record(
            &mut graph,
            &mut types,
            &source,
            rbs,
            vec![mapping((14, 38), site(&source, (90, 110)))],
        );
        assert_eq!(indexed(&graph, &uri), 0);
        assert_eq!(table.indexed(), 1);
        assert_eq!(
            table.origin(&UriId::from(uri.as_str()), 20),
            Origin::Declared(&site(&source, (90, 110)))
        );
    }

    #[test]
    fn forgetting_a_source_takes_its_declarations_out_of_the_graph_too() {
        let (mut graph, mut types) = (Graph::new(), Types::new());
        let mut table = Synthesized::new();
        let source = source();

        table.record(
            &mut graph,
            &mut types,
            &source,
            "class Story\n  def title: () -> String\nend\n",
            vec![mapping((14, 38), site(&source, (60, 80)))],
        );

        let uri = generated_uri(&source);
        assert_eq!(indexed(&graph, &uri), 2);

        assert!(table.forget(&mut graph, &source));
        assert!(table.is_empty());
        assert_eq!(indexed(&graph, &uri), 0);
        assert!(!graph.documents().contains_key(&UriId::from(uri.as_str())));
        assert_eq!(
            table.origin(&UriId::from(generated_uri(&source).as_str()), 20),
            Origin::OnDisk
        );

        // Nothing to forget the second time, and saying so is what keeps a caller from
        // deleting a document it never created.
        assert!(!table.forget(&mut graph, &source));
    }

    #[test]
    fn clearing_leaves_nothing_generated() {
        let (mut graph, mut types) = (Graph::new(), Types::new());
        let mut table = Synthesized::new();
        table.record(
            &mut graph,
            &mut types,
            &source(),
            "class Story\nend\n",
            Vec::new(),
        );

        table.clear();

        assert!(table.is_empty());
        assert!(!table.is_generated(&UriId::from(generated_uri(&source()).as_str())));
    }
}
