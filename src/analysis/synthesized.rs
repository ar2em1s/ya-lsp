//! Where a declaration that no file declares was really written.
//!
//! Every generator works the same way: read something the repository already states (a column in
//! `db/schema.rb`, a `belongs_to :user`), write RBS that says it, and hand that text to the two
//! functions that turn RBS into typed, navigable declarations. Neither [`indexer::index_source`]
//! nor [`Types::harvest`] knows where its text came from, which makes all generators one feature
//! instead of a dozen.
//!
//! That has one cost, and this module is it. Generated text is indexed under a URI with no file
//! behind it, at offsets into bytes nobody can open, while every navigation answer turns a
//! declaration into a place by asking the graph where its definitions are. So a generated
//! definition must be translated back to the line that implied it before it reaches an editor, and
//! where nothing recorded that line it must be **dropped**: a correct card above a jump into a file
//! the user does not have is worse than no type at all, because the card is checked once and the
//! jump is trusted forever.
//!
//! [`super::erb::ruby_view`] can blank a template in place because its output is the same length as
//! its input, so there is one coordinate system. Generated RBS has no such relationship to the Ruby
//! that implied it, so the relationship is recorded as the text is written.
//!
//! # One kind of member's place is in a file no generator read
//!
//! Every span above comes from the document the generator had open: a column's place is the
//! `t.string "title"` it was reading. ActiveRecord's query interface has no such line, because no
//! project file declares `Story.where`, and a *Resolved* card with nowhere to go is the tier
//! breaking its promise. It does have a definition, one directory further out: `where` is a `def`
//! in the activerecord gem, which is already indexed.
//!
//! So those members carry a **name** instead of a span ([`Generated::named`], written by
//! [`Facts::render`](crate::generated::Facts::render) for anything tagged
//! [`Source::Query`](crate::generated::Source::Query)), and the place is *looked up*. It cannot be
//! looked up while the generator runs: the generator writes the text rubydex is about to link, so
//! the graph knows nothing about any gem yet.
//! [`Analysis::place_generated_members`](super::Analysis::place_generated_members) asks one step
//! later, after the resolve, and writes the answers into [`Generated::placed`].
//!
//! **This does not soften the rule.** A name is looked up, never matched: it is found on the class
//! Rails installs it on or not at all, and not found is the same nothing as before.
//!
//! # Keyed by definition, not declaration
//!
//! A declaration merges every definition of a name: `Story#title()` generated from
//! `t.string "title"` and `Story#title()` written as a `def` in `app/models/story.rb` are one
//! declaration with two definitions. That is the ordinary case, and why the schema is a *derived*
//! answer. Keying on the declaration would rewrite its location and drop the user's own `def`. So
//! the key is the definition's document and offset, the real definition stays where it is, and only
//! the generated one is translated.
//!
//! # One generated document per source file **and body**, by naming
//!
//! [`generated_uri`] derives the generated document's URI from the source file's and the body it
//! holds (`ya-lsp-generated:file:///…/db/schema.rb#class:Story`), so recording twice for one body
//! cannot leave two answers: rubydex replaces a document indexed under a URI it already holds, and
//! this table is keyed by that same URI. The *source* is recoverable from the name alone, by the
//! prefix [`generated_prefix`] writes, which [`super::environment`], `completion::Locality` and
//! [`super::hints`] read instead of asking this table.
//!
//! **Why a body, not a file.** [`Synthesized::record`] pays per declaration it re-indexes, so the
//! document is the unit of invalidation. One document per file means a column that changed type
//! re-indexes every column in the schema, which dominated keystroke latency on a large schema; one
//! document per body re-indexes one table.
//!
//! **The one piece of bookkeeping, and the failure it prevents.** A source that wrote three bodies
//! and now writes two must *forget* the third, or a dropped table answers forever. So
//! [`Synthesized::record`] takes **every** part of one source at once and prunes whatever is
//! missing: a source's set of documents changes atomically, and there is no second list anyone must
//! remember to prune.
//!
//! # The URI is deliberately not a file URI
//!
//! rubydex happily indexes under `ya-lsp-generated:file:///…` (it files its own built-ins as
//! `rubydex:built-in`), and [`DocUri::from_graph_uri`] refuses it, because `Url::to_file_path`
//! does. That is the backstop under everything above: even with an empty table and a bug here, a
//! generated document cannot become a `Location`, a symbol row or a diagnostic. It is not under the
//! workspace prefix either, so [`super::Analysis::is_own_code`] already says no. The table decides
//! which generated definitions become a *useful* place; the scheme guarantees none becomes a wrong
//! one.

use std::collections::{BTreeMap, HashMap};

use rubydex::{
    indexing::LanguageId,
    model::{graph::Graph, ids::UriId},
};

use super::{indexer, locator::Site, types::Types};
use crate::generated::Runs;
use crate::workspace::DocUri;

/// The scheme generated documents are filed under.
///
/// The source's full URI follows it, so the key is unique by construction and a log line says which
/// file the declarations came from. A prefix on a non-`file` scheme, not a path: a path under the
/// workspace root could be matched by an `index.include` glob, and a path outside it would still be
/// a `Location` an editor would try to open.
pub const GENERATED_SCHEME: &str = "ya-lsp-generated:";

/// Everything ya-lsp wrote itself, and where each piece of it was really declared.
#[derive(Debug, Default)]
pub struct Synthesized {
    /// Generated document -> what was written into it, and the spans that point somewhere.
    ///
    /// A document with no mappings is still *in* the map, and that is the point:
    /// present-with-no-mapping answers [`Origin::Unknown`] and is not a jump target, while absent
    /// answers [`Origin::OnDisk`] and is left alone.
    documents: HashMap<UriId, Generated>,
    /// Which generated documents each source file owns, so a body it stops writing is dropped.
    ///
    /// Derived from the names in [`Self::documents`], never a second source of truth: written only
    /// in [`Synthesized::record`] and [`Self::forget`], and both write one source's whole list at
    /// once. A source with nothing live is not in it.
    sources: HashMap<UriId, Vec<String>>,
    /// How many times a generated document has really been handed to the graph.
    ///
    /// The third instrument beside [`Analysis::passes`](super::Analysis) and
    /// [`Analysis::walks`](super::Analysis), for their reason: the claim is that the graph is left
    /// alone, and a graph rebuilt into the shape it already had looks the same as an untouched one.
    /// Only a counter tells them apart.
    ///
    /// Monotonic, and deliberately not reset by [`Self::clear`]: a test asserts it did **not move**
    /// across an edit, which a counter that restarts cannot show.
    indexed: u64,
    /// Generated documents whose **text** has changed since somebody last asked.
    ///
    /// The one thing a client reading a generated document cannot work out itself: it shows a
    /// buffer with no file behind it, so nothing it watches will ever fire. Written where the text
    /// is written (the insert below, and [`Self::drop`], because a document going away is a change
    /// too) and drained by [`Analysis::refresh_generated`](super::Analysis::refresh_generated) once
    /// per settle, so it holds at most one pass's worth.
    ///
    /// Deliberately not filtered by who is looking. This table knows which text changed but not
    /// which documents a client has open; the caller knows the second but not the first, which is
    /// why the two halves are separate.
    changed: Vec<String>,
}

/// One generated document, as it was handed over.
///
/// The text is kept so [`Synthesized::record`] can tell a regeneration that changed something from
/// one that did not; see there for what that is worth. It is tens of kilobytes for a real schema,
/// against megabytes of Ruby in the graph.
#[derive(Debug)]
struct Generated {
    rbs: String,
    mappings: Vec<Mapping>,
    /// The members whose place is a *name*, not a span, as the generator wrote them.
    ///
    /// Kept, not resolved on the spot, because while a generator runs the graph holds no
    /// declarations: the pass writes the text rubydex is about to link, so "where does Rails write
    /// `where`" has no answer until after linking. See [`Self::placed`].
    named: Vec<crate::generated::Named>,
    /// And the answers, once the graph could give them.
    ///
    /// A second list, not more [`Self::mappings`], so the two stay separable: one is what the
    /// generator said (a property of the source file), the other what the graph said (a property of
    /// the bundle, discarded and re-asked every time the graph is linked). Read alongside
    /// `mappings`, never instead.
    placed: Vec<Mapping>,
    /// The blocks a class this document declares is the `self` of: where each call starts in the
    /// source, and the class, or `None` for a block whose `self` must not be answered.
    ///
    /// Taken with the mappings on every record, text changed or not, and for their reason: it is in
    /// the source's coordinates, and a line added above a call moves it without changing a byte of
    /// the RBS.
    ran: BTreeMap<u32, Runs>,
}

/// One body's worth of generated text, on its way in.
///
/// What [`Facts::split`](crate::generated::Facts::split) produced and
/// [`Facts::render`](crate::generated::Facts::render) spelled, plus the name it is filed under. A
/// struct, not four arguments, because [`Synthesized::record`] takes a whole source's worth at
/// once.
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
    /// The blocks a generated class is the `self` of, in the source's coordinates
    /// ([`Facts::runs`](crate::generated::Facts::runs)).
    pub ran: Vec<(u32, Runs)>,
}

/// One span of generated text, and the line that implied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    /// The byte range in the generated document this covers, end exclusive.
    ///
    /// Mappings may nest (the class a `create_table` implies contains the methods its columns
    /// imply), and the narrowest one containing a definition wins, as for the spans under the
    /// cursor in [`super::locator::locate`].
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
    /// Reached by everything the generators invent that nobody else wrote down: a `class Story` the
    /// schema hangs columns off, a callback registrar, the fixed half of a `Struct`. The query
    /// interface is the exception: its place is a name, as the module header explains.
    Unknown,
}

impl Synthesized {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Index everything `source` implies, replacing whatever it implied before.
    ///
    /// Both halves of the mechanism live in one function so they cannot drift: the graph learns the
    /// declarations, and [`Types`] learns what they return. Both overwrite instead of accumulating
    /// (rubydex drops the old document with this URI, and a harvest overwrites per method), so
    /// re-reading a source after an edit leaves no stale answer.
    ///
    /// **Every part of one source at once, by design.** One source writes one document per body, so
    /// the only way to notice a body has gone is to receive the whole list. A per-part `record`
    /// would need a second list someone kept pruned, and its failure (a dropped table still
    /// answering) is silent.
    ///
    /// **Two questions, not one.** The graph holds the *text*; the side table holds where each line
    /// came from. A regeneration can change one without the other, and the commonest Rails edit
    /// changes only the second: a provenance comment names a file and macro but never a line
    /// number, so pressing return above a `has_many` regenerates **byte-identical** RBS whose
    /// mappings have all shifted a line. Handing that to rubydex would make it drop the document,
    /// invalidate every declaration it touched, cascade to members, singletons and descendants,
    /// unresolve every dependent name, and relink everything to reach the graph it already had.
    ///
    /// So the text decides whether the **graph** is touched, and the mappings update either way.
    /// The saving grows with the *graph*, not the document: on a large workspace one keystroke's
    /// re-index costs a large fraction of a second, against a fraction of a millisecond to compare
    /// mappings. It is invisible on a small workspace, and it is why splitting into bodies pays: on
    /// an ordinary keystroke nearly every document takes this branch, so smaller documents make the
    /// few that do not smaller too.
    ///
    /// It does **not** mark the graph dirty; the caller does, as for
    /// [`super::Analysis::index_buffer`] and the signature files on the gem batch. Nor does it
    /// blank `interface` blocks like that path must: this text was written here, and that rule is
    /// for signature files somebody else wrote.
    ///
    /// A method that disappears from the regenerated text leaves its entry in [`Types`], and that
    /// entry is unreachable for the reason [`Types::harvest`] gives: a lookup only happens after
    /// rubydex has found the member, and rubydex no longer has one.
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
                // The graph already holds exactly this text, so it has nothing to learn and
                // everything to lose by being told again. The mappings are this table's own and are
                // taken whether or not they moved: comparing costs a scan of every span and
                // assigning costs a pointer, and a stale mapping is a jump landing on a line the
                // declaration has moved off.
                held.mappings = part.mappings;
                // `placed` is deliberately **not** cleared here. The text did not change, so
                // neither did the names, and the answers are re-asked after every resolve anyway;
                // dropping them would leave one settle's worth of requests with no place for a
                // member that has one.
                held.named = part.named;
                held.ran = ran_of(part.ran);
                written.push(uri);
                continue;
            }
            // Keyed by the generated document's own URI, the spelling it is indexed and served
            // under, so regenerating a body replaces what it declared instead of accumulating
            // beside it. See `Types::harvest`.
            //
            // A generator writing RBS the parser rejects would otherwise index text nothing can
            // read and type nothing, *silently*: the two consumers fail independently and neither
            // says so. **One body's worth of declarations** is the right blast radius for a
            // generator bug, and a warning naming the file and the body makes it findable.
            if !types.harvest(uri.as_str(), &part.rbs) {
                tracing::warn!(
                    "generated RBS from {source} does not parse; {} declares nothing",
                    part.body
                );
                self.drop(graph, &uri);
                continue;
            }
            // The bulkhead reaches this route too, with the recovery the branch above already
            // wrote: a generator whose RBS crashes the indexer declares one body's worth of nothing
            // instead of taking the session down. There is no skip list to join: a generated
            // document has no file, so nothing re-reads it, and the next edit to the *source* asks
            // the generator again.
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
                    ran: ran_of(part.ran),
                },
            );
            // Only the branch that changed the text reaches here, which is exactly what a reader of
            // this document needs to hear about. The branch above (identical RBS, moved mappings)
            // deliberately says nothing: it moved where a jump lands, and the bytes on screen are
            // the same.
            self.changed.push(uri.clone());
            written.push(uri);
        }
        // The pruning the signature exists for. It also covers the two failures above: a part that
        // declined to index is not in `written`, so whatever it left behind goes too.
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

    /// Take one generated document out of the table and the graph, and say whether there was one.
    ///
    /// The one place a generated document is deleted, so the three halves cannot separate: a
    /// document dropped from the table but left in the graph answers with nowhere to go, and one
    /// dropped without notice leaves a reader holding text that no longer describes anything.
    fn drop(&mut self, graph: &mut Graph, uri: &str) -> bool {
        if self.documents.remove(&UriId::from(uri)).is_none() {
            return false;
        }
        graph.delete_document(uri);
        // A document going away is the largest change its text can undergo, and the reader is the
        // one client that would otherwise show the old bytes forever: nothing else will ever tell
        // it, because there was never a file.
        self.changed.push(uri.to_owned());
        true
    }

    /// Drop everything `source` implied, from the table and the graph, and say whether there was
    /// anything.
    ///
    /// Called wherever a document leaves the index, so a deleted `db/schema.rb` takes its columns
    /// with it instead of leaving declarations nothing can reach or refresh. Every body it wrote,
    /// because a source owns all of them.
    pub fn forget(&mut self, graph: &mut Graph, source: &DocUri) -> bool {
        let Some(held) = self.sources.remove(&UriId::from(source.as_str())) else {
            return false;
        };
        let mut dropped = false;
        for uri in held {
            dropped |= self.drop(graph, &uri);
        }
        dropped
    }

    /// The RBS `source` last declared, or `None` where it declared nothing.
    ///
    /// The text, not its effects. Half of what a generator gets wrong is visible only in what it
    /// wrote (an optionality, an arity, which of two annotations won), and asserting those through
    /// the graph and the type table asserts three things at once.
    ///
    /// Every body it wrote, joined in filing order, because callers ask about the *file*; the split
    /// into documents is this module's business.
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

    /// What the block passed to the call starting at `call` in `source` runs as, where a generator
    /// said ([`Facts::runs`](crate::generated::Facts::runs)).
    ///
    /// `None` where nothing said. `call` is in the coordinates the source was generated from,
    /// which are the graph's.
    #[must_use]
    pub fn ran(&self, source: &str, call: u32) -> Option<&Runs> {
        self.sources
            .get(&UriId::from(source))?
            .iter()
            .filter_map(|uri| self.documents.get(&UriId::from(uri.as_str())))
            .find_map(|generated| generated.ran.get(&call))
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

    /// Every declaration `source` implied whose place selects `selection`: the generated document,
    /// and the declaration's span in it. The way back from a line in a file to what was written
    /// about it.
    #[must_use]
    pub fn generated_at(&self, source: &str, selection: (u32, u32)) -> Vec<(UriId, (u32, u32))> {
        self.sources
            .get(&UriId::from(source))
            .into_iter()
            .flatten()
            .filter_map(|uri| {
                let id = UriId::from(uri.as_str());
                Some((id, self.documents.get(&id)?))
            })
            .flat_map(|(id, generated)| {
                generated
                    .mappings
                    .iter()
                    // Every mapping of a source's documents is into that source.
                    .filter(|mapping| mapping.declared.selection == selection)
                    .map(move |mapping| (id, mapping.generated))
            })
            .collect()
    }

    /// Every declaration `source` implied whose place's whole construct holds `offset`: the
    /// generated document, the declaration's span in it, and where the construct starts. The way
    /// back from a lambda to the `scope` call that declared a member from it.
    #[must_use]
    pub fn generated_around(&self, source: &str, offset: u32) -> Vec<(UriId, (u32, u32), u32)> {
        self.sources
            .get(&UriId::from(source))
            .into_iter()
            .flatten()
            .filter_map(|uri| {
                let id = UriId::from(uri.as_str());
                Some((id, self.documents.get(&id)?))
            })
            .flat_map(|(id, generated)| {
                generated
                    .mappings
                    .iter()
                    .filter(|mapping| {
                        mapping.declared.full.0 <= offset && offset < mapping.declared.full.1
                    })
                    .map(move |mapping| (id, mapping.generated, mapping.declared.full.0))
            })
            .collect()
    }

    /// Every generated document that wrote a member whose place is a name, and those names.
    ///
    /// The read half of the two-step: a generator states the names while the graph is empty, and
    /// the caller answers them once it is linked.
    pub(super) fn named(&self) -> impl Iterator<Item = (UriId, &[crate::generated::Named])> {
        self.documents
            .iter()
            .filter(|(_, generated)| !generated.named.is_empty())
            .map(|(id, generated)| (*id, generated.named.as_slice()))
    }

    /// What the graph said about them, replacing whatever it said last time.
    ///
    /// **Replacing, not appending**, the rule the whole module is built on: a bundle that changed
    /// under the workspace must not leave a jump pointing into the version that went away.
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

    /// The RBS in one generated document, by the URI a client asks about it under.
    ///
    /// The whole of `workspace/textDocumentContent`, and the only way the text this crate writes is
    /// ever *read* by the person it was written for. Everything else here turns a generated span
    /// back into a line in a file the user has; this hands over the document itself.
    ///
    /// **Two lookups, because the URI that comes back is not always the one that went out.** The
    /// server names the document in a `window/showDocument` request, and the client asks for its
    /// content under whatever its own URI type makes of that. In VS Code that is a percent-encoded
    /// rewrite of the same string (`Uri.parse` decodes every component and `toString` escapes
    /// everything outside the unreserved set plus `/`). So the exact key is tried first, for a
    /// client that echoes what it got, and a scan comparing unescaped forms answers one that does
    /// not. The scan is linear over every generated document, which is fine here: this request
    /// comes when a person opens one document, not on a keystroke.
    ///
    /// The scheme test is not redundant with the map. It makes the refusal a **rule**, not an
    /// accident of the table's contents: this is the first request whose argument is a URI the
    /// client did not get from a `Location`, and a reader that returned a file's contents for a
    /// `file:` URI would be an unwanted file server.
    ///
    /// It returns the table's own spelling of the URI beside the text, because the caller needs
    /// both and only this function can tell them apart: the client is answered in its own spelling,
    /// a later `workspace/textDocumentContent/refresh` must name the same document, and everything
    /// on this side of the wire is keyed by the table's spelling.
    #[must_use]
    pub fn content<'a>(&'a self, uri: &'a str) -> Option<(&'a str, &'a str)> {
        if !uri.starts_with(GENERATED_SCHEME) {
            return None;
        }
        if let Some(generated) = self.documents.get(&UriId::from(uri)) {
            return Some((uri, &generated.rbs));
        }
        let held = self
            .sources
            .values()
            .flatten()
            .find(|held| crate::workspace::uri::same_uri(held, uri))?;
        self.documents
            .get(&UriId::from(held.as_str()))
            .map(|generated| (held.as_str(), generated.rbs.as_str()))
    }

    /// Every generated document whose text changed since this was last asked, emptying the list.
    ///
    /// Draining, not reading, is what bounds it: the caller is the settle, so the list holds one
    /// pass's worth, and a cold index does not pile up a document per body until somebody looks.
    pub(super) fn take_changed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.changed)
    }

    /// How many documents ya-lsp has generated. For the log line and the tests.
    ///
    /// Documents, not source files: one file writes one per body. [`Self::sources`] is the other
    /// number, for a caller asking "how many files had something to say".
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
    /// The field it reads explains why this is counted: a graph rebuilt into its existing shape
    /// looks the same as one left alone.
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
    /// For the one test that compares two settles byte for byte: a pass over an unchanged workspace
    /// writes what the previous pass wrote, and the only way to say so is to look at all of it, not
    /// only the documents a fixture remembers.
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

/// [`Part::ran`] keyed by the call, the question [`Synthesized::ran`] asks. Two facts about one call
/// are the same block said twice; the first is kept.
fn ran_of(ran: Vec<(u32, Runs)>) -> BTreeMap<u32, Runs> {
    let mut keyed = BTreeMap::new();
    for (call, runs) in ran {
        keyed.entry(call).or_insert(runs);
    }
    keyed
}

/// The URI one body of the declarations implied by `source` is indexed under.
///
/// A pure function of the source URI and the body's name, which makes "one generated document per
/// source file and body" a property of naming, not a list someone keeps pruned. Total, on purpose:
/// a fallible derivation would add a silent "generated nothing" arm on exactly the path where
/// silence is the failure being designed against.
///
/// The body is [`Owner::body`](crate::generated::Owner::body) (`class:Story`, `module:Storyish`),
/// the key [`Facts::render`](crate::generated::Facts::render) itself uses, so a part never holds
/// half a body and two parts never claim one.
#[must_use]
pub fn generated_uri(source: &DocUri, body: &str) -> String {
    format!("{}{body}", generated_prefix(source.as_str()))
}

/// What every generated document from one source begins with, and nothing else does.
///
/// The read half of the naming: a caller holding a source that wants its generated documents tests
/// this prefix instead of asking this table, which keeps `completion::Locality` and
/// [`super::hints`] independent of it. The trailing `#` makes it exact: a source URI is
/// percent-encoded and so holds no `#`, so one source's prefix is never another's.
#[must_use]
pub(super) fn generated_prefix(source: &str) -> String {
    format!("{GENERATED_SCHEME}{source}#")
}

/// The file a document came from: itself, unless ya-lsp generated it.
///
/// The inverse of the naming above, and the one place the `#` is cut. Every **prefix** rule in
/// [`super::environment`] asks this, not the raw URI: a generated document is filed under a scheme,
/// not a path, so a prefix test on the raw URI reads it as nowhere at all (outside the workspace,
/// under no load path), and a `Data.define` in a project's own spec file would stop being the
/// suite's. The **segment** rules are unaffected, because the source's directories are still there
/// to split on, which is why this is applied where prefixes are read and not at the top of every
/// rule. A scheme in front of a URI is not a directory.
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
    /// The production signature takes every part of a source at once, because the pruning needs
    /// that. A test about one document says so here instead of spelling out a `Part` at nine call
    /// sites, and the body name is the one `db/schema.rb` would really produce.
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
                ran: Vec::new(),
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
    /// Definitions, not declarations, on purpose: a declaration is what the *resolver* merges
    /// definitions into, and `Resolver::resolve` has exactly one call site in this crate
    /// (`analysis::resolve`, where its panic-after-delete is contained). Indexing alone produces
    /// definitions, which are enough to tell replacing from appending. The declaration side is
    /// asked end to end, through the server, in `analysis::tests`.
    fn indexed(graph: &Graph, uri: &str) -> usize {
        graph
            .documents()
            .get(&UriId::from(uri))
            .map_or(0, |document| document.definitions().len())
    }

    /// Which class a block runs as is kept per source, and taken on every record, text changed or
    /// not: a line added above the call moves it without changing a byte of the RBS.
    #[test]
    fn which_class_a_block_runs_as_is_taken_with_the_mappings() {
        let mut graph = Graph::new();
        let mut types = Types::default();
        let mut table = Synthesized::new();
        let spec = DocUri::from_path(Path::new("/project/spec/story_spec.rb")).expect("a file uri");
        let part = |ran: Vec<(u32, Runs)>| Part {
            body: "whole".to_owned(),
            rbs: "class Story\nend\n".to_owned(),
            mappings: Vec::new(),
            named: Vec::new(),
            ran,
        };
        assert_eq!(
            table.ran(spec.as_str(), 0),
            None,
            "nothing recorded for the source"
        );
        table.record(
            &mut graph,
            &mut types,
            &spec,
            vec![part(vec![
                (0, Runs::Made("Story".to_owned())),
                (0, Runs::Refused),
                (9, Runs::Refused),
            ])],
        );
        assert_eq!(
            table.ran(spec.as_str(), 0),
            Some(&Runs::Made("Story".to_owned())),
            "the first said is kept"
        );
        assert_eq!(table.ran(spec.as_str(), 9), Some(&Runs::Refused));
        assert_eq!(table.ran(spec.as_str(), 5), None);
        // The same text, the call moved down a line.
        table.record(
            &mut graph,
            &mut types,
            &spec,
            vec![part(vec![(4, Runs::Made("Story".to_owned()))])],
        );
        assert_eq!(table.ran(spec.as_str(), 0), None);
        assert_eq!(
            table.ran(spec.as_str(), 4),
            Some(&Runs::Made("Story".to_owned()))
        );
    }

    #[test]
    fn a_generated_document_is_not_a_file_and_cannot_be_turned_into_one() {
        let uri = generated_uri(&source(), "class:Story");
        assert_eq!(
            uri,
            "ya-lsp-generated:file:///project/db/schema.rb#class:Story"
        );
        // And the source comes back out, which is what every prefix rule in `environment` and the
        // two locality tables read instead of asking this table.
        assert_eq!(source_of(&uri), "file:///project/db/schema.rb");
        assert_eq!(
            source_of("file:///project/db/schema.rb"),
            "file:///project/db/schema.rb"
        );
        // The backstop the module documents: whatever this table says, nothing that turns a graph
        // URI into something an editor opens accepts one of these.
        assert!(DocUri::from_graph_uri(&uri).is_none());

        // **Still refused now that the URI travels on the wire.** A code action carries one as a
        // command argument, and a client hands it straight back in `workspace/textDocumentContent`,
        // so the door `from_lsp` guards really is knocked on. A `Location`, a symbol row and a
        // diagnostic all reach the client through it, and none may ever name a document with no
        // file behind it.
        let wire: lsp_types::Uri = uri.parse().expect("a valid uri");
        assert!(DocUri::from_lsp(&wire).is_none());
    }

    #[test]
    fn the_text_is_readable_by_uri_however_the_client_spells_that_uri() {
        let (mut graph, mut types) = (Graph::new(), Types::new());
        let mut table = Synthesized::new();
        let uri = record_one(
            &mut table,
            &mut graph,
            &mut types,
            &source(),
            "class Story\n  def title: () -> String\nend\n",
            Vec::new(),
            Vec::new(),
        );

        // The spelling the server handed out, which is what a client that echoes it sends.
        let (spelled, rbs) = table.content(&uri).expect("the document");
        assert_eq!(spelled, uri);
        assert!(rbs.contains("def title: () -> String"));

        // And VS Code's, which decodes every component on the way in and escapes everything outside
        // the unreserved set on the way out, so both colons come back as `%3A`.
        let rewritten = "ya-lsp-generated:file%3A///project/db/schema.rb#class%3AStory";
        assert_ne!(rewritten, uri);
        let (spelled, same) = table.content(rewritten).expect("the same document");
        // The table's own spelling comes back beside the text, because the refresh that follows
        // must name the document in the spelling the *client* used, and the caller is the only one
        // holding both.
        assert_eq!(spelled, uri);
        assert_eq!(same, rbs);

        // Nothing else is served, and the scheme is the rule, not the table being incomplete: a
        // reader answering for a `file:` URI would be a file server.
        assert!(table.content(source().as_str()).is_none());
        assert!(
            table
                .content("ya-lsp-generated:file:///project/db/schema.rb#class:Nobody")
                .is_none()
        );
        assert!(table.content("").is_none());
    }

    #[test]
    fn only_text_that_actually_moved_is_worth_telling_a_reader_about() {
        let (mut graph, mut types) = (Graph::new(), Types::new());
        let mut table = Synthesized::new();
        let uri = record_one(
            &mut table,
            &mut graph,
            &mut types,
            &source(),
            "class Story\n  def title: () -> String\nend\n",
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(table.take_changed(), vec![uri.clone()]);
        // Draining bounds the list: a cold index generates every body in the workspace, and nothing
        // should pile up until somebody opens one.
        assert!(table.take_changed().is_empty());

        // The same RBS again changes nothing a reader can see. Nearly every document takes this
        // branch on an ordinary keystroke (identical text, mappings shifted a line), and a refresh
        // there would redraw a window for nothing.
        record_one(
            &mut table,
            &mut graph,
            &mut types,
            &source(),
            "class Story\n  def title: () -> String\nend\n",
            vec![mapping((12, 20), site(&source(), (40, 55)))],
            Vec::new(),
        );
        assert!(table.take_changed().is_empty());

        // A body going away is the largest change its text can undergo: nothing else will tell the
        // reader, because there was never a file.
        table.forget(&mut graph, &source());
        assert_eq!(table.take_changed(), vec![uri]);
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
        // In the table, so not left alone, and with nothing to point at, so not a place.
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
                ran: BTreeMap::new(),
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

        // One document holding two constructs instead of five, one set of mappings instead of
        // three, and the removed column answers nothing.
        assert_eq!(table.len(), 1);
        // And the graph really was told, the other half of the gate: changed text is handed over,
        // whatever it costs.
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

    /// The one piece of bookkeeping the split adds, whose failure would be silent.
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
                    ran: Vec::new(),
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
        // more, so left behind it would answer forever, which is the failure `record`'s
        // whole-source signature exists to prevent.
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
        // And the one that stayed was not re-handed to the graph for the other's sake.
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

        // Deleted from under it, so that a second `record` doing any work at all would put it back.
        // Nothing else in the crate deletes a document it means to keep; this is the only way to
        // detect a call that is deliberately not made.
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

        // **The graph holds only the text.** A moved mapping is not a change to it (the same
        // column, one line further down `db/schema.rb`), so the table takes the new spans and the
        // graph is still not told, as the document deleted from under it shows: it stays deleted.
        // Re-indexing here would cost most of a second per keystroke on a large workspace.
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

        // Nothing to forget the second time, and saying so keeps a caller from deleting a document
        // it never created.
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
        // The first job of the side table, and the reason it exists: the declaration is real, its
        // offsets are into bytes nobody can open, and the user must land on a line in a different
        // file entirely.
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
        // The two spans a link carries, and the mapping decides both: the whole `t.string` call to
        // reveal, the string literal to select.
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
        // the type table learned its return type. `upcase` resolves only because `-> String` was
        // harvested from text this crate wrote itself.
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
        // The second job, and the one the table exists for. `byline` is in the graph and types its
        // receiver, but has nowhere to go, so it answers everything *except* the question whose
        // wrong answer opens a nonexistent file.
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
            DocUri::from_graph_uri(&synthesized::generated_uri(&schema, "class:Story")).is_none()
        );
    }

    #[test]
    fn the_class_a_generated_document_reopens_still_jumps_to_the_users_own_file() {
        // A generated document defines `class Story` because RBS has no other way to hang a method
        // off it, so the declaration has two definitions: the user's and this crate's. Keying the
        // table by *declaration* would redirect both and drop the model file; keying by definition
        // leaves the real one alone.
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
        // The third. An edit to `db/schema.rb` re-runs whatever read it, and the failure designed
        // out is silent: a renamed column answering under both its old and new names, forever, with
        // nothing on screen to say so.
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
        // The fourth, through the path a `git checkout` takes: the watcher reports the file gone.
        // Nothing will re-read a missing file, so a generated declaration left behind would outlive
        // every chance to correct it.
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = synthetic_project(source);
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));
        assert!(harness.has("Story#title()"));

        std::fs::remove_file(schema.to_file_path().unwrap()).unwrap();
        harness.watch(&[&schema]);

        assert!(!harness.has("Story#title()"));
        assert!(harness.analysis.synthesized.is_empty());
        assert!(harness.definition_at(&uri, source, "title").is_null());
    }

    #[test]
    fn a_generated_document_is_not_the_users_code_and_says_nothing_on_screen() {
        // A generated document is not under the workspace root (or any root), so the test that
        // keeps a vendored bundle's diagnostics off the screen already covers it. Worth pinning,
        // not assuming: to rubydex a generated document is a document like any other, and every
        // rule keeping other people's files off the screen must also hold for text with no file at
        // all.
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
        // The two consumers of generated text fail *independently*, and neither says so:
        // `Types::harvest` returns on a parse error, and rubydex indexes what it can. A buggy
        // generator would then declare a partial class nobody can explain. Refusing the body is the
        // right blast radius, and the warning names the file **and the body**, which a reader needs
        // once one file writes several.
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
        // Another bulkhead route. The recovery is the branch above's, because both failures have
        // one blast radius: this generator's declarations and nobody else's. The analysis thread
        // must survive to handle them, because `Synthesized::record` runs inline on it, on the
        // settle path, which is every keystroke.
        //
        // Armed, not provoked; `indexer::SOURCE_INDEXES_TO_CRASH` explains why: no RBS shape tried
        // makes rubydex's RBS indexer panic. The two routes a *user's* file takes have real Ruby
        // fixtures.
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
        // `workspace/symbol` reaches `locator::site` by a different route from goto-definition, and
        // a row an editor cannot open is worse than no row (the sentence `hierarchy` already
        // carries about `rubydex:built-in`). Both halves here, on one fixture where the two columns
        // differ only in their mapping: the mapped one is offered and points at the schema, the
        // unmapped one is not offered.
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

    /// The two halves of placing a member run a resolve apart, and a document can leave the index
    /// in between.
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

        // Not "the place is wrong" but "there is nothing to put a place on": the document is gone,
        // so the offset belongs to nobody, and `origin` says what it said before the generator ever
        // ran.
        assert_eq!(table.len(), 0);
        assert_eq!(table.origin(&UriId::from(uri.as_str()), 20), Origin::OnDisk);
    }
}
