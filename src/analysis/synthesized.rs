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
//! # One kind of member's place is in a file no generator read
//!
//! Every span above comes from the document the generator had open: a column's place is the
//! `t.string "title"` it was reading. ActiveRecord's query interface has no such line, because no
//! file in the project declares `Story.where` — and a *Resolved* card with nowhere to go is the
//! tier making a promise it cannot keep. It does have a definition, one directory further out:
//! `where` is a `def` in the activerecord gem, which is already indexed.
//!
//! So those members carry a **name** instead of a span — [`Generated::named`], written by
//! [`Facts::render`](crate::generated::Facts::render) for anything tagged
//! [`Source::Query`](crate::generated::Source::Query) — and the place is *found*. It cannot be
//! found where it is stated: a generator writes the text rubydex is about to link, so while one
//! runs the graph holds four declarations and knows nothing about any gem.
//! [`Analysis::place_generated_members`](super::Analysis::place_generated_members) asks one step
//! later, after the resolve, and writes the answers into [`Generated::placed`].
//!
//! **This does not soften the rule below it.** A name is looked up, never matched: it is found on
//! the class Rails installs it on or it is not found, and not found is the same nothing as before.
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
//! # One generated document per source file **and body**, by naming rather than by bookkeeping
//!
//! [`generated_uri`] derives the generated document's URI from the source file's and the body it
//! holds — `ya-lsp-generated:file:///…/db/schema.rb#class:Story` — so recording twice for one
//! body cannot leave two answers anywhere: rubydex replaces a document indexed under a URI it
//! already holds, and the table is a map keyed by that same URI. The *source* is still
//! recoverable from the name alone, by the prefix [`generated_prefix`] writes, which is what
//! [`super::environment`], `completion::Locality` and [`super::hints`] read rather than asking
//! this table.
//!
//! **Why a body and not a file.** [`Synthesized::record`] is charged per declaration it re-indexes, so
//! the document is the unit of invalidation: one document per file means a column that changed
//! type re-indexes every column in the schema — 2.2 seconds of a 2.49 second keystroke on
//! discourse — and one document per body re-indexes one table. `synthesized.md` has the
//! measurement and the migration distribution that decided it.
//!
//! **The one thing that is bookkeeping, and the failure it is against.** A source that wrote
//! three bodies and now writes two has to *forget* the third, or a dropped table answers
//! forever. So [`Synthesized::record`] takes **every** part of one source at once and prunes whatever
//! is no longer in the list: the set of documents a source owns moves atomically, and there is
//! no second list anybody has to remember to prune.
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
    /// Which generated documents each source file owns, so a body it stops writing is dropped.
    ///
    /// Derived from the names in [`Self::documents`] and never a second source of truth: it is
    /// written in [`Synthesized::record`] and [`Self::forget`] and nowhere else, and both of them write
    /// the whole of one source's list at once. A source with nothing live is not in it.
    sources: HashMap<UriId, Vec<String>>,
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
    /// The members whose place is a *name* rather than a span, as the generator wrote them.
    ///
    /// Kept rather than resolved on the spot, because at the moment a generator runs the graph
    /// holds no declarations at all: the pass writes the text rubydex is about to link, so the
    /// question "where does Rails write `where`" has no answer until after it has linked. See
    /// [`Self::placed`].
    named: Vec<crate::generated::Named>,
    /// And the answers, once the graph could give them.
    ///
    /// A second list rather than more [`Self::mappings`] so that the two stay separable: one is
    /// what the generator said, which is a property of the source file, and this is what the
    /// graph said, which is a property of the bundle and is thrown away and asked again every
    /// time the graph is linked. Read alongside `mappings` and never instead of them.
    placed: Vec<Mapping>,
}

/// One body's worth of generated text, on its way in.
///
/// What [`Facts::split`](crate::generated::Facts::split) produced and
/// [`Facts::render`](crate::generated::Facts::render) spelled, plus the name the pair is filed
/// under. A struct rather than four positional arguments because [`Synthesized::record`] takes a
/// whole source's worth of them at once.
#[derive(Debug)]
pub struct Part {
    /// Which body this is, as [`Owner::body`](crate::generated::Owner::body) spells it.
    pub body: String,
    /// The RBS itself.
    pub rbs: String,
    /// Where each declaration in it was really written.
    pub mappings: Vec<Mapping>,
    /// And the ones whose place is a name in a gem rather than a span in the source.
    pub named: Vec<crate::generated::Named>,
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
    ///
    /// Still reached by everything the generators invent and nobody else wrote down — a
    /// `class Story` the schema hangs columns off, a callback registrar, the fixed half of a
    /// `Struct`. What left it is the query interface, whose place is a name the module header
    /// explains rather than an absence.
    Unknown,
}

impl Synthesized {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Index everything `source` implies, replacing whatever it implied before.
    ///
    /// Both halves of the mechanism live in one function so they cannot drift: the graph learns
    /// the declarations and [`Types`] learns what they return. Both are keyed in a way that
    /// overwrites rather than accumulates — rubydex drops the old document with this URI, and a
    /// harvest overwrites per method — so a source re-read after an edit leaves no stale answer.
    ///
    /// **Every part of one source at once, and that is the signature rather than a convenience.**
    /// One source writes one document per body, so the only way to notice that a body has gone
    /// away is to be handed the whole list; a per-part `record` would need a second list somebody
    /// kept pruned, and the failure it would leak — a dropped table still answering — is silent.
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
    /// of a millisecond to compare every mapping. That is why it is invisible on a small one —
    /// and it is the reason the split into bodies pays: 975 of discourse's 977 documents take
    /// this branch on an ordinary keystroke, so making them smaller makes the two that do not
    /// smaller too.
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
        parts: Vec<Part>,
    ) -> Vec<String> {
        let mut written: Vec<String> = Vec::with_capacity(parts.len());
        for part in parts {
            let uri = generated_uri(source, &part.body);
            let id = UriId::from(uri.as_str());
            if let Some(held) = self.documents.get_mut(&id)
                && held.rbs == part.rbs
            {
                // The graph already holds exactly this text, so there is nothing for it to learn
                // and everything for it to lose by being told again. The mappings are this
                // table's own and are taken whether they moved or not: comparing them costs a
                // scan of every span and assigning them costs a pointer, and a mapping left
                // stale is a jump that lands on a line the declaration has moved off.
                held.mappings = part.mappings;
                // `placed` is deliberately **not** cleared here. The text did not change, so the
                // names did not either, and the answers are re-asked after every resolve anyway —
                // dropping them would leave one settle's worth of requests with no place for a
                // member that has one.
                held.named = part.named;
                written.push(uri);
                continue;
            }
            // A generator that writes RBS the parser rejects would otherwise index text nothing
            // can read and type nothing at all, *silently* — the two consumers fail independently
            // and neither says so. **One body's worth of declarations** is the right blast radius
            // for a bug in a generator, and a warning naming the file and the body is what makes
            // it findable.
            if !types.harvest(&part.rbs) {
                tracing::warn!(
                    "generated RBS from {source} does not parse; {} declares nothing",
                    part.body
                );
                self.drop(graph, &uri);
                continue;
            }
            // The bulkhead reaches this route too, and the recovery here is the one
            // the branch above already wrote: a generator whose RBS crashes the indexer declares
            // one body's worth of nothing, rather than taking the session with it. There is no
            // skip list to join — a generated document has no file behind it, so nothing re-reads
            // it and the next edit to the *source* asks the generator again.
            if !indexer::index_source(graph, &uri, &part.rbs, &LanguageId::Rbs) {
                tracing::warn!(
                    "indexing the RBS generated from {source} crashed; {} declares nothing",
                    part.body
                );
                self.drop(graph, &uri);
                continue;
            }
            self.indexed += 1;
            self.documents.insert(
                id,
                Generated {
                    rbs: part.rbs,
                    mappings: part.mappings,
                    named: part.named,
                    placed: Vec::new(),
                },
            );
            written.push(uri);
        }
        // The pruning the signature exists for, and it covers the two failures above as well as a
        // body the generators stopped writing: a part that declined to index is not in `written`,
        // so whatever it left behind goes with the rest.
        let held = self
            .sources
            .insert(UriId::from(source.as_str()), written.clone())
            .unwrap_or_default();
        for gone in held.iter().filter(|uri| !written.contains(uri)) {
            self.drop(graph, gone);
        }
        if written.is_empty() {
            self.sources.remove(&UriId::from(source.as_str()));
        }
        written
    }

    /// Take one generated document out of the table and out of the graph.
    ///
    /// The one place a generated document is deleted, so the two halves cannot come apart: a
    /// document dropped from the table and left in the graph answers with nowhere to go.
    fn drop(&mut self, graph: &mut Graph, uri: &str) {
        if self.documents.remove(&UriId::from(uri)).is_some() {
            graph.delete_document(uri);
        }
    }

    /// Drop everything `source` implied, from the table and from the graph, and say whether
    /// there was anything to drop.
    ///
    /// Called wherever a document leaves the index, so that a deleted `db/schema.rb` takes its
    /// columns with it rather than leaving declarations nothing can reach and nothing will ever
    /// refresh. Every body it wrote, because a source owns all of them.
    pub fn forget(&mut self, graph: &mut Graph, source: &DocUri) -> bool {
        let Some(held) = self.sources.remove(&UriId::from(source.as_str())) else {
            return false;
        };
        let mut dropped = false;
        for uri in held {
            dropped |= self.documents.remove(&UriId::from(uri.as_str())).is_some();
            graph.delete_document(&uri);
        }
        dropped
    }

    /// The RBS `source` last declared, or `None` where it has declared nothing.
    ///
    /// The text rather than its effects. Half of what a generator gets wrong is visible only in
    /// what it wrote — an optionality, an arity, which of two annotations won — and asserting
    /// those through the graph and the type table asserts three things at once.
    ///
    /// Every body it wrote, joined in the order they are filed, because what a caller is asking
    /// about is the *file* and the split into documents is this module's business rather than
    /// theirs.
    #[must_use]
    pub fn text(&self, source: &DocUri) -> Option<String> {
        let held = self.sources.get(&UriId::from(source.as_str()))?;
        let rbs: String = held
            .iter()
            .filter_map(|uri| self.documents.get(&UriId::from(uri.as_str())))
            .map(|generated| generated.rbs.as_str())
            .collect();
        (!rbs.is_empty()).then_some(rbs)
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
            .chain(&generated.placed)
            .filter(|mapping| mapping.generated.0 <= offset && offset < mapping.generated.1)
            .min_by_key(|mapping| mapping.generated.1 - mapping.generated.0)
            .map_or(Origin::Unknown, |mapping| {
                Origin::Declared(&mapping.declared)
            })
    }

    /// Every generated document that wrote a member whose place is a name, and those names.
    ///
    /// The read half of the two-step: a generator states the names while the graph is empty,
    /// and the caller answers them once it is linked.
    pub(super) fn named(&self) -> impl Iterator<Item = (UriId, &[crate::generated::Named])> {
        self.documents
            .iter()
            .filter(|(_, generated)| !generated.named.is_empty())
            .map(|(id, generated)| (*id, generated.named.as_slice()))
    }

    /// What the graph said about them, replacing whatever it said last time.
    ///
    /// **Replacing and not appending**, which is the same rule the whole module is built on: a
    /// bundle that changed under the workspace must not leave a jump pointing into the version
    /// that went away.
    pub(super) fn place(&mut self, document: UriId, placed: Vec<Mapping>) {
        if let Some(generated) = self.documents.get_mut(&document) {
            generated.placed = placed;
        }
    }

    /// Whether ya-lsp wrote this document itself.
    #[must_use]
    pub fn is_generated(&self, document: &UriId) -> bool {
        self.documents.contains_key(document)
    }

    /// How many documents ya-lsp has generated. For the log line, and for the tests.
    ///
    /// Documents and not source files: one file writes one per body. [`Self::sources`] is the
    /// other number, and a caller that means "how many files had something to say" wants it.
    #[must_use]
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// How many source files declared anything at all.
    #[must_use]
    pub fn sources(&self) -> usize {
        self.sources.len()
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
        self.sources.clear();
    }
}

/// The URI one body of the declarations implied by `source` is indexed under.
///
/// A pure function of the source URI and the body's own name, which is what makes "one generated
/// document per source file and body" a property of the naming instead of a list somebody has to
/// keep pruned. Total, and deliberately: a fallible derivation would put a silent "generated
/// nothing" arm on the one path where being silent is the failure being designed against.
///
/// The body is [`Owner::body`](crate::generated::Owner::body) — `class:Story`, `module:Storyish`
/// — which is [`Facts::render`](crate::generated::Facts::render)'s own key, so a part can never
/// hold half a body and two parts can never both claim one.
#[must_use]
pub fn generated_uri(source: &DocUri, body: &str) -> String {
    format!("{}{body}", generated_prefix(source.as_str()))
}

/// What every generated document one source wrote begins with, and nothing else does.
///
/// The read half of the naming: a caller that holds a source and wants its generated documents
/// tests this prefix rather than asking this table, which is what keeps `completion::Locality`
/// and [`super::hints`] free of it. The trailing `#` is what makes it exact — a source URI is
/// percent-encoded, so it can hold no `#` of its own, and one source's prefix is therefore never
/// another's.
#[must_use]
pub(super) fn generated_prefix(source: &str) -> String {
    format!("{GENERATED_SCHEME}{source}#")
}

/// The file a document came out of: itself, unless ya-lsp generated it.
///
/// The inverse of the naming above, and the one place the `#` is cut. Every **prefix** rule in
/// [`super::environment`] is asked of this rather than of the raw URI: a generated document is
/// filed under a scheme and not under a path, so left in, a prefix test reads it as being nowhere
/// at all — outside the workspace, under no load path. Left in, that said a `Data.define` written
/// in a project's own spec file was not the suite's, which is exactly what it is and lobsters
/// writes one. The **segment** rules never noticed, because the source's directories are still in
/// there to be split on, which is why this is applied where the prefixes are read and not at the
/// top of every rule. Same lesson as `file:` squashing to `file` in `locator::named_after`: a
/// scheme in front of a uri is not a directory.
#[must_use]
pub(super) fn source_of(uri: &str) -> &str {
    match uri.strip_prefix(GENERATED_SCHEME) {
        Some(rest) => rest.split_once('#').map_or(rest, |(source, _)| source),
        None => uri,
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::testing::*;
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

    /// One body's worth of RBS, which is what most of these tests are about.
    ///
    /// The production signature takes every part of a source at once, because that is what the
    /// pruning needs. A test whose subject is one document says so here rather than spelling a
    /// `Part` out at nine call sites, and the body name is the one `db/schema.rb` would really
    /// produce.
    fn record_one(
        table: &mut Synthesized,
        graph: &mut Graph,
        types: &mut Types,
        source: &DocUri,
        rbs: &str,
        mappings: Vec<Mapping>,
        named: Vec<crate::generated::Named>,
    ) -> String {
        table.record(
            graph,
            types,
            source,
            vec![Part {
                body: "class:Story".to_owned(),
                rbs: rbs.to_owned(),
                mappings,
                named,
            }],
        );
        generated_uri(source, "class:Story")
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
        let uri = generated_uri(&source(), "class:Story");
        assert_eq!(
            uri,
            "ya-lsp-generated:file:///project/db/schema.rb#class:Story"
        );
        // And the source comes back out of it, which is what every prefix rule in
        // `environment` and the two locality tables read rather than asking this table.
        assert_eq!(source_of(&uri), "file:///project/db/schema.rb");
        assert_eq!(
            source_of("file:///project/db/schema.rb"),
            "file:///project/db/schema.rb"
        );
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
        let uri = record_one(
            &mut table,
            &mut graph,
            &mut types,
            &source(),
            "class Story\nend\n",
            Vec::new(),
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
                named: Vec::new(),
                placed: Vec::new(),
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

        let uri = record_one(
            &mut table,
            &mut graph,
            &mut types,
            &source,
            "class Story\n  def title: () -> String\n  def byline: () -> String\nend\n",
            vec![
                mapping((14, 38), site(&source, (60, 80))),
                mapping((41, 66), site(&source, (90, 110))),
            ],
            Vec::new(),
        );
        assert_eq!(indexed(&graph, &uri), 3);
        assert_eq!(table.indexed(), 1);

        record_one(
            &mut table,
            &mut graph,
            &mut types,
            &source,
            "class Story\n  def byline: () -> String\nend\n",
            vec![mapping((14, 39), site(&source, (90, 110)))],
            Vec::new(),
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

    /// The one piece of bookkeeping the split adds, and its failure is silent.
    #[test]
    fn a_body_a_source_stops_writing_is_taken_out_of_the_graph_with_it() {
        let (mut graph, mut types) = (Graph::new(), Types::new());
        let mut table = Synthesized::new();
        let source = source();
        let parts = |bodies: &[&str]| -> Vec<Part> {
            bodies
                .iter()
                .map(|body| Part {
                    body: (*body).to_owned(),
                    rbs: format!("class {}\nend\n", body.trim_start_matches("class:")),
                    mappings: Vec::new(),
                    named: Vec::new(),
                })
                .collect()
        };

        table.record(
            &mut graph,
            &mut types,
            &source,
            parts(&["class:Story", "class:Widget"]),
        );
        assert_eq!(table.len(), 2, "one document per body");
        assert_eq!(table.sources(), 1, "out of one file");
        let widget = generated_uri(&source, "class:Widget");
        assert_eq!(indexed(&graph, &widget), 1);

        // The table was dropped from the schema. Nothing re-reads a body no generator writes any
        // more, so left behind it would answer forever — which is the failure the whole-source
        // signature of `record` exists to make impossible.
        table.record(&mut graph, &mut types, &source, parts(&["class:Story"]));

        assert_eq!(table.len(), 1);
        assert_eq!(indexed(&graph, &widget), 0);
        assert!(
            !graph
                .documents()
                .contains_key(&UriId::from(widget.as_str()))
        );
        assert_eq!(
            table.origin(&UriId::from(widget.as_str()), 0),
            Origin::OnDisk
        );
        // And the one that stayed was not handed over a second time for the other's sake.
        assert_eq!(table.indexed(), 2);

        // Forgetting the source takes every body it still owns, in one call.
        assert!(table.forget(&mut graph, &source));
        assert!(table.is_empty());
        assert_eq!(table.sources(), 0);
        assert_eq!(indexed(&graph, &generated_uri(&source, "class:Story")), 0);
    }

    #[test]
    fn recording_the_same_text_again_does_not_hand_it_over_again() {
        let (mut graph, mut types) = (Graph::new(), Types::new());
        let mut table = Synthesized::new();
        let source = source();
        let rbs = "class Story\n  def title: () -> String\nend\n";
        let mappings = vec![mapping((14, 38), site(&source, (60, 80)))];

        let uri = record_one(
            &mut table,
            &mut graph,
            &mut types,
            &source,
            rbs,
            mappings.clone(),
            Vec::new(),
        );
        assert_eq!(indexed(&graph, &uri), 2);
        assert_eq!(table.indexed(), 1);

        // Taken out from under it, so that a second `record` doing any work at all would put
        // it back. Nothing else in the crate deletes a document it means to keep; it is the
        // only way to watch for a call that is deliberately not made.
        graph.delete_document(&uri);
        record_one(
            &mut table,
            &mut graph,
            &mut types,
            &source,
            rbs,
            mappings,
            Vec::new(),
        );
        assert_eq!(indexed(&graph, &uri), 0);
        assert_eq!(table.indexed(), 1);

        // **The text is the only thing the graph holds.** A mapping that moved is not a change
        // to it — the same column, one line further down `db/schema.rb` — so the table takes the
        // new spans and the graph is still not told, which is what the document deleted out from
        // under it says: it stays deleted. Re-indexing here costs 940 ms in front of a keystroke
        // on discourse.
        record_one(
            &mut table,
            &mut graph,
            &mut types,
            &source,
            rbs,
            vec![mapping((14, 38), site(&source, (90, 110)))],
            Vec::new(),
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

        record_one(
            &mut table,
            &mut graph,
            &mut types,
            &source,
            "class Story\n  def title: () -> String\nend\n",
            vec![mapping((14, 38), site(&source, (60, 80)))],
            Vec::new(),
        );

        let uri = generated_uri(&source, "class:Story");
        assert_eq!(indexed(&graph, &uri), 2);

        assert!(table.forget(&mut graph, &source));
        assert!(table.is_empty());
        assert_eq!(indexed(&graph, &uri), 0);
        assert!(!graph.documents().contains_key(&UriId::from(uri.as_str())));
        assert_eq!(
            table.origin(
                &UriId::from(generated_uri(&source, "class:Story").as_str()),
                20
            ),
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
        record_one(
            &mut table,
            &mut graph,
            &mut types,
            &source(),
            "class Story\nend\n",
            Vec::new(),
            Vec::new(),
        );

        table.clear();

        assert!(table.is_empty());
        assert!(!table.is_generated(&UriId::from(
            generated_uri(&source(), "class:Story").as_str()
        )));
    }

    #[test]
    fn a_generated_declaration_jumps_to_the_line_that_declared_it() {
        // The first thing the side table has to do, and the reason it exists at all: the
        // declaration is real, its offsets are into bytes nobody can open, and the place the
        // user has to land is a line in a completely different file.
        let source = "Story.new.title.upcase\n";
        let (mut harness, schema, uri) = synthetic_project(source);
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));

        assert!(
            harness.has("Story#title()"),
            "the generated RBS was not indexed"
        );

        let definition = harness.definition_at(&uri, source, "title");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(schema.as_str()),
            "{definition}"
        );
        // The two spans a link carries, and the mapping decides both: the whole `t.string` call
        // to reveal, the string literal to select.
        assert_eq!(
            definition[0]["targetRange"]["start"]["line"], 2,
            "{definition}"
        );
        assert_eq!(
            definition[0]["targetRange"]["start"]["character"], 4,
            "{definition}"
        );
        assert_eq!(
            definition[0]["targetSelectionRange"]["start"]["character"], 13,
            "{definition}"
        );

        // The other half of the boundary, in the same call: the graph learned the declaration and
        // the type table learned what it returns. `upcase` resolves at all only because
        // `-> String` was harvested out of text this crate wrote itself.
        let markdown = card(&mut harness, &uri, source, "upcase");
        assert!(markdown.contains("String#upcase"), "{markdown}");
        assert!(
            harness
                .declarations_at(&uri, "Story.new.title.~\n")
                .contains(&"upcase".to_owned())
        );
    }

    #[test]
    fn a_generated_declaration_with_no_mapping_is_not_a_jump_target() {
        // The second, and the one the table exists for. `byline` is in the graph, it
        // types its receiver, and it has nowhere to go — so it answers everything *except* the
        // question whose wrong answer opens a file that is not there.
        let source = "Story.new.byline.upcase\n";
        let (mut harness, schema, uri) = synthetic_project(source);
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));

        assert!(harness.has("Story#byline()"));
        let markdown = card(&mut harness, &uri, source, "byline");
        assert!(markdown.contains("Story#byline"), "{markdown}");

        let definition = harness.definition_at(&uri, source, "byline");
        assert!(
            definition.is_null(),
            "an unmapped declaration is not a place: {definition}"
        );

        // Not by luck: the generated document's URI is not one a client could be sent, and no
        // answer anywhere names it.
        assert!(
            DocUri::from_uri_str(&synthesized::generated_uri(&schema, "class:Story")).is_none()
        );
    }

    #[test]
    fn the_class_a_generated_document_reopens_still_jumps_to_the_users_own_file() {
        // A generated document defines `class Story` because RBS has no other way to hang a
        // method off it, so the declaration has two definitions: one the user wrote and one
        // this crate did. Keying the table by *declaration* would have redirected both and
        // taken the model file off the map; keying it by definition leaves the real one alone.
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = synthetic_project(source);
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));

        let definition = harness.definition_at(&uri, source, "Story");
        let targets = definition.as_array().expect("a link");
        assert_eq!(targets.len(), 1, "{definition}");
        assert!(
            targets[0]["targetUri"]
                .as_str()
                .is_some_and(|target| target.ends_with("app/models/story.rb")),
            "{definition}"
        );
    }

    #[test]
    fn regenerating_a_source_leaves_one_answer_rather_than_two() {
        // The third. An edit to `db/schema.rb` re-runs whatever read it, and the
        // failure being designed out is silent: a column that was renamed answering from both
        // its old name and its new one, forever, with nothing on screen to say so.
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = synthetic_project(source);
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));
        assert!(harness.has("Story#title()"));

        let renamed = "class Story\n  def headline: () -> String\nend\n";
        harness.synthesize(
            &schema,
            renamed,
            vec![synthesized::Mapping {
                generated: span(renamed, "  def headline: () -> String\n"),
                declared: Site {
                    uri: schema.as_str().to_owned(),
                    full: span(SCHEMA, "t.string \"byline\""),
                    selection: span(SCHEMA, "\"byline\""),
                },
            }],
        );

        assert!(harness.has("Story#headline()"));
        assert!(
            !harness.has("Story#title()"),
            "the column that went away is still answering"
        );
        assert!(
            harness.definition_at(&uri, source, "title").is_null(),
            "and still jumping"
        );
    }

    #[test]
    fn a_generated_declaration_whose_source_is_deleted_stops_being_reachable() {
        // The fourth, through the path a `git checkout` takes: the watcher says the
        // file is gone. Nothing will ever re-read a file that is not there, so a generated
        // declaration left behind by that would outlive every chance to correct it.
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = synthetic_project(source);
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));
        assert!(harness.has("Story#title()"));

        std::fs::remove_file(schema.to_path().unwrap()).unwrap();
        harness.watch(&[&schema]);

        assert!(!harness.has("Story#title()"));
        assert!(harness.analysis.synthesized.is_empty());
        assert!(harness.definition_at(&uri, source, "title").is_null());
    }

    #[test]
    fn a_generated_document_is_not_the_users_code_and_says_nothing_on_screen() {
        // A generated document is not under the workspace root — it is not under any root — so
        // the test that keeps a vendored bundle's diagnostics off the screen already covers it.
        // Worth pinning rather than assuming: a generated document is a document like any other
        // to rubydex, and every rule that keeps somebody else's files off the screen has to hold
        // for text that has no file at all.
        let (mut harness, schema, _uri) = synthetic_project("Story.new\n");
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));

        let generated = synthesized::generated_uri(&schema, "class:Story");
        assert!(!harness.analysis.is_own_code(&generated));
        assert!(
            harness
                .analysis
                .synthesized
                .is_generated(&UriId::from(generated.as_str()))
        );
        assert!(
            harness
                .published()
                .into_iter()
                .all(|(uri, _)| uri != generated),
            "a document with no file behind it must not publish diagnostics"
        );
    }

    #[test]
    fn rbs_this_crate_wrote_that_does_not_parse_declares_nothing_at_all() {
        // The two consumers of generated text fail *independently* and neither says so:
        // `Types::harvest` returns on a parse error and rubydex indexes what it can. A generator
        // with a bug would then declare a partial class nobody can explain, silently. Refusing
        // the body is the right blast radius, and the warning names the file **and the body** —
        // which is what a reader needs once one file writes several.
        let (mut harness, schema, _uri) = synthetic_project("Story.new\n");
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));
        assert!(harness.has("Story#title()"));

        let (_, logged) = crate::testing::captured_logs(tracing::Level::WARN, || {
            harness.synthesize(&schema, "class Story\n  def title: () ->\n", Vec::new());
        });

        assert!(!harness.has("Story#title()"));
        assert!(
            harness.analysis.synthesized.is_empty(),
            "text that does not parse must take what the source declared before with it"
        );
        assert!(
            logged.contains("does not parse; class:Story declares nothing"),
            "{logged}"
        );
    }

    #[test]
    fn rbs_this_crate_wrote_that_crashes_the_indexer_declares_nothing_at_all() {
        // The bulkhead's seventh route. The recovery is the one the branch above already wrote,
        // because the two failures have one blast radius: this generator's declarations, and
        // nobody else's. What is new is that the analysis thread is still here to have them —
        // `Synthesized::record` runs inline on it, on the settle path, which is every keystroke.
        //
        // Armed rather than provoked, and `indexer::SOURCE_INDEXES_TO_CRASH` says why: forty-
        // three RBS shapes were run through rubydex's RBS indexer looking for one that panics
        // and none does. The two routes a *user's* file takes have real Ruby fixtures.
        let (mut harness, schema, _uri) = synthetic_project("Story.new\n");
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));
        assert!(harness.has("Story#title()"));

        indexer::SOURCE_INDEXES_TO_CRASH.set(1);
        let (_, logged) = crate::testing::captured_logs(tracing::Level::WARN, || {
            harness.synthesize(
                &schema,
                "class Story\n  def title: () -> String\nend\n",
                Vec::new(),
            );
        });
        assert_eq!(
            indexer::SOURCE_INDEXES_TO_CRASH.replace(0),
            0,
            "the crash was armed and taken"
        );

        assert!(!harness.has("Story#title()"));
        assert!(
            harness.analysis.synthesized.is_empty(),
            "text that cannot be indexed takes what the source declared before with it"
        );
        assert!(
            logged.contains("crashed; class:Story declares nothing"),
            "{logged}"
        );
    }

    #[test]
    fn a_generated_declaration_in_the_symbol_picker_points_at_the_real_file() {
        // `workspace/symbol` reaches `locator::site` by a different route from goto-definition,
        // and a row an editor cannot open is worse than a row that is not there — which is the
        // sentence `hierarchy` already carries about `rubydex:built-in`. Both halves here, over
        // one fixture where the two columns differ in nothing but their mapping: the mapped one
        // is offered and points at the schema, the unmapped one is not offered at all.
        let (mut harness, schema, _uri) = synthetic_project("Story.new\n");
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));

        let mut rows = |query: &str| -> Vec<(String, String)> {
            let found = harness.ask("workspace/symbol", serde_json::json!({ "query": query }));
            found
                .as_array()
                .map(|symbols| {
                    symbols
                        .iter()
                        .filter_map(|symbol| {
                            Some((
                                symbol["name"].as_str()?.to_owned(),
                                symbol["location"]["uri"].as_str()?.to_owned(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };

        assert_eq!(
            rows("title"),
            vec![("title".to_owned(), schema.as_str().to_owned())]
        );
        assert!(
            rows("byline").is_empty(),
            "an unmapped row has nowhere to open"
        );
    }

    /// The two halves of placing a member run a resolve apart, and a document can leave the
    /// index between them.
    #[test]
    fn placing_a_member_of_a_document_that_is_gone_does_nothing_rather_than_inventing_one() {
        let (mut graph, mut types) = (Graph::new(), Types::new());
        let mut table = Synthesized::new();
        let source = source();
        let uri = record_one(
            &mut table,
            &mut graph,
            &mut types,
            &source,
            "class Story\n  def where: (*untyped) -> untyped\nend\n",
            Vec::new(),
            vec![crate::generated::Named {
                generated: (14, 46),
                singleton: false,
                name: "where".to_owned(),
            }],
        );
        assert_eq!(
            table.named().map(|(_, names)| names.len()).sum::<usize>(),
            1
        );

        table.forget(&mut graph, &source);
        table.place(
            UriId::from(uri.as_str()),
            vec![mapping((14, 46), site(&source, (0, 10)))],
        );

        // Not "the place is wrong" but "there is nothing to put a place on": the document is
        // gone, so the offset belongs to nobody and `origin` says what it said before the
        // generator ever ran.
        assert_eq!(table.len(), 0);
        assert_eq!(table.origin(&UriId::from(uri.as_str()), 20), Origin::OnDisk);
    }
}
