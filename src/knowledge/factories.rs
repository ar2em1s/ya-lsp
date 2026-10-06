//! FactoryBot's factories: which class `create(:user)` builds.
//!
//! The reading is `workspace::factories`'; this finds the definition files, parses each once per
//! version of its text, and hosts the rows on the file that declares the library's own module, so
//! every arm of one method is one document's and is tried in the order it is written.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::{Counted, Declared, Declaring, Fresh, ListId, Sources, Wants};
use crate::generated::At;
use crate::workspace::{DocUri, Features, factories};

/// Files that name `FactoryBot`: where factories are defined.
pub const DEFINITIONS: ListId = ListId("factories.definitions");

static WANTS: [Wants; 1] = [Wants {
    list: DEFINITIONS,
    calls: &[],
    constants: &["FactoryBot"],
    modules: &[],
    defines: &[],
    path: None,
    spells: &[],
    tags: false,
    inherits: false,
    engines: false,
    // A gem's `lib/` ships factories a project may load (`spree/testing_support/factories`); the
    // project's own win a name both write ([`factories::classes`]).
    gems: true,
    // Not `reads_only`: the rows are declared from these, though hosted on the library's module.
    reads_only: false,
    buffers: false,
}];

/// Every definition file, read.
#[derive(Debug, Default)]
pub struct Factories {
    sources: BTreeMap<String, (Fresh, Arc<factories::Read>, DocUri)>,
    /// Where each factory is written, as the last pass found them ([`factories::Classes::places`]).
    places: BTreeMap<String, (String, At)>,
    /// How many files were parsed, for a test that asserts an unchanged one is not.
    pub reads: usize,
}

impl super::Knowledge for Factories {
    fn name(&self) -> &'static str {
        "factories"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn wants(&self) -> &'static [Wants] {
        &WANTS
    }

    fn wanted(&self, list: ListId, features: Features) -> bool {
        list == DEFINITIONS && features.factories
    }

    /// The `factory` call a strategy's first argument names: every member this module writes is
    /// a strategy on [`factories::SYNTAX`], and the gem's own members there have places.
    fn literal_place(&self, owner: &str, _method: &str, literal: &str) -> Option<(String, At)> {
        (owner == factories::SYNTAX)
            .then(|| self.places.get(literal).cloned())
            .flatten()
    }

    fn spellable_names(&self) -> Vec<&'static str> {
        factories::CONSTANTS.to_vec()
    }

    fn refresh(&mut self, sources: &Sources<'_>) {
        let wanted: BTreeSet<&String> = sources.context.documents(DEFINITIONS).iter().collect();
        self.sources.retain(|key, _| wanted.contains(key));
        for key in wanted {
            let Some(uri) = DocUri::from_graph_uri(key) else {
                continue;
            };
            let fresh = (sources.fresh)(&uri);
            if self
                .sources
                .get(key)
                .is_some_and(|(held, _, _)| *held == fresh)
            {
                continue;
            }
            let Some(text) = (sources.text)(&uri) else {
                self.sources.remove(key);
                continue;
            };
            self.reads += 1;
            let mut read = factories::read_factories(&text);
            read.uri.clone_from(key);
            self.sources
                .insert(key.clone(), (fresh, Arc::new(read), uri));
        }
    }

    /// Every definition's class, resolved against what the project and the bundle declare, then
    /// the rows on the library's own module. With `[types] factories` off nothing is held:
    /// [`DEFINITIONS`] is not wanted, and `refresh` drops what it no longer lists.
    fn declare(&mut self, declaring: &Declaring<'_>, into: &mut Declared) -> Counted {
        if self.sources.is_empty() {
            self.places.clear();
            return Counted::new();
        }
        let reads: Vec<factories::Read> = self
            .sources
            .values()
            .map(|(_, read, _)| factories::Read {
                gem: !(declaring.own)(&read.uri),
                ..(**read).clone()
            })
            .collect();
        let namespaces = &declaring.context.namespaces;
        // One question to the graph for every name a definition may mean and every host.
        let mut wanted = factories::wanted_names(&reads);
        wanted.extend(factories::CONSTANTS.iter().map(|name| (*name).to_owned()));
        let hosts = (declaring.declares)(&wanted);
        let classes = factories::classes(&reads, &|nesting, written| {
            factories::candidates_of(nesting, written)
                .into_iter()
                .find(|name| hosts.contains_key(name) || namespaces.declares(name))
        });
        let built = classes
            .factories
            .values()
            .filter(|class| class.is_some())
            .count();
        for (owner, facts) in factories::rows(&classes, namespaces) {
            if let Some(uri) = hosts.get(owner) {
                super::add(into, uri, facts);
            }
        }
        // What each factory's blocks run as, written beside the file that defines it: a block's
        // `self` is looked up where its call is written.
        let uris: Vec<&DocUri> = self.sources.values().map(|(_, _, uri)| uri).collect();
        let mut defined = 0;
        for (index, facts) in factories::proxies(&reads, &classes, namespaces) {
            defined += 1;
            super::add(into, uris[index], facts);
        }
        self.places = classes.places;
        vec![("factories", built), ("definition files", defined)]
    }
}
