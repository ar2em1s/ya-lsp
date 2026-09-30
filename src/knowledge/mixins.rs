//! Ruby's `include` and `prepend` called on a class from outside its body, as a body of knowledge:
//! the module joins the class's ancestors, as it does in Ruby.
//!
//! The reading is `workspace::mixins`'. Not switchable, like `Singleton` and `define_method`: it is
//! how Ruby itself behaves, not a framework's convention.

use std::collections::{BTreeSet, HashMap};

use super::{Counted, Declared, Declaring, Fresh, ListId, Sources, Wants};
use crate::workspace::{DocUri, Features, mixins};

/// Documents whose text calls `include` or `prepend` on something: rubydex records no reference for
/// such a call, so the text is what lists them ([`Wants::spells`]), and the reader keeps only a
/// call made on a constant.
pub const MIXINS: ListId = ListId("mixins.mixins");

static WANTS: [Wants; 1] = [Wants {
    list: MIXINS,
    calls: &[],
    constants: &[],
    modules: &[],
    defines: &[],
    path: None,
    spells: &mixins::SPELLED,
    tags: false,
    inherits: false,
    // An engine patching the application's classes is the common case (an engine's `app/patches/`).
    engines: true,
    gems: false,
    reads_only: false,
    buffers: false,
}];

/// Ruby's `include` and `prepend`, called from outside.
#[derive(Debug, Default)]
pub struct Mixins {
    /// What the reader found in each listed file, by URI: a parse, which depends on the text
    /// alone ([`Knowledge::refresh`](super::Knowledge::refresh)).
    sources: HashMap<String, (Fresh, Vec<mixins::Mixed>)>,
    /// How many files it has read.
    pub reads: u64,
}

impl super::Knowledge for Mixins {
    fn name(&self) -> &'static str {
        "mixins"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn wants(&self) -> &'static [Wants] {
        &WANTS
    }

    fn wanted(&self, list: ListId, _features: Features) -> bool {
        list == MIXINS
    }

    /// Each call whose two names mean something, in a file the application loads.
    ///
    /// **Only a file the application loads**: a spec's `ActiveRecord::Base.include(Helpers)` runs
    /// in the suite, and written onto the class it would hand every cursor in the application the
    /// suite's helpers as the class's own members.
    fn declare(&mut self, declaring: &Declaring<'_>, into: &mut Declared) -> Counted {
        let loaded: Vec<(DocUri, &Vec<mixins::Mixed>)> = declaring
            .context
            .documents(MIXINS)
            .iter()
            .filter_map(|key| {
                let (_, found) = self.sources.get(key)?;
                let uri = DocUri::from_graph_uri(key)?;
                (!found.is_empty() && (declaring.loaded)(&uri)).then_some((uri, found))
            })
            .collect();
        let asked: BTreeSet<String> = loaded
            .iter()
            .flat_map(|(_, found)| mixins::asked(found))
            .collect();
        let kinds = (declaring.kinds)(&asked);
        let kind = |name: &str| kinds.get(name).copied();
        let mut declared = 0;
        for (uri, found) in loaded {
            let resolved: Vec<mixins::Resolved> = found
                .iter()
                .filter_map(|mixed| mixins::resolve(mixed, &kind))
                .collect();
            declared += resolved.len();
            super::add(into, &uri, mixins::facts(&resolved));
        }
        vec![("mixins", declared)]
    }

    fn refresh(&mut self, sources: &Sources<'_>) {
        let listed: BTreeSet<&str> = sources
            .context
            .documents(MIXINS)
            .iter()
            .map(String::as_str)
            .collect();
        // A file that has left the list keeps nothing here, as in the `define_method` module.
        self.sources.retain(|uri, _| listed.contains(uri.as_str()));
        for (key, uri) in listed
            .into_iter()
            .filter_map(|key| DocUri::from_graph_uri(key).map(|uri| (key, uri)))
        {
            let fresh = (sources.fresh)(&uri);
            if self
                .sources
                .get(key)
                .is_some_and(|(held, _)| *held == fresh)
            {
                continue;
            }
            let Some(text) = (sources.text)(&uri) else {
                self.sources.remove(key);
                continue;
            };
            self.reads += 1;
            self.sources
                .insert(key.to_owned(), (fresh, mixins::read_mixins(&text)));
        }
    }
}
