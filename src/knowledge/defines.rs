//! Ruby's `define_method`, as a body of knowledge: a class or module body that makes a method by a
//! literal name has that method.
//!
//! The reading is `workspace::defines`'. Not switchable, like `Singleton`: it is how Ruby itself
//! behaves, not a framework's convention.

use std::collections::{BTreeSet, HashMap};

use super::{Counted, Declared, Declaring, Fresh, ListId, Sources, Wants};
use crate::workspace::{DocUri, Features, defines};

/// Documents that call `define_method` or `define_singleton_method` with no receiver.
pub const DEFINES: ListId = ListId("defines.defines");

static WANTS: [Wants; 1] = [Wants {
    list: DEFINES,
    calls: &defines::DEFINERS,
    constants: &[],
    modules: &[],
    defines: &[],
    path: None,
    spells: &[],
    tags: false,
    inherits: false,
    // A class in an engine's `app/` is one a reader can name, as a struct's is.
    engines: true,
    gems: false,
    reads_only: false,
    buffers: false,
}];

/// Ruby's `define_method`.
#[derive(Debug, Default)]
pub struct Defines {
    /// What the reader found in each listed file, by URI: a parse, which depends on the text
    /// alone ([`Knowledge::refresh`](super::Knowledge::refresh)).
    sources: HashMap<String, (Fresh, Vec<defines::Defined>)>,
    /// How many files it has read.
    pub reads: u64,
}

impl super::Knowledge for Defines {
    fn name(&self) -> &'static str {
        "defines"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn wants(&self) -> &'static [Wants] {
        &WANTS
    }

    fn wanted(&self, list: ListId, _features: Features) -> bool {
        list == DEFINES
    }

    /// Each method a body makes by name, on a class or module whose name can be spelled.
    fn declare(&mut self, declaring: &Declaring<'_>, into: &mut Declared) -> Counted {
        let namespaces = &declaring.context.namespaces;
        let mut declared = 0;
        // A listed file with nothing held could not be read (it is gone).
        for ((_, held), uri) in declaring
            .context
            .documents(DEFINES)
            .iter()
            .filter_map(|key| self.sources.get(key).zip(DocUri::from_graph_uri(key)))
        {
            let found: Vec<defines::Defined> = held
                .iter()
                .filter(|found| namespaces.spellable(found.owner.name()))
                .cloned()
                .collect();
            declared += found.len();
            super::add(
                into,
                &uri,
                defines::facts(&found, &(declaring.caption)(&uri)),
            );
        }
        vec![("defined methods", declared)]
    }

    fn refresh(&mut self, sources: &Sources<'_>) {
        let listed: BTreeSet<&str> = sources
            .context
            .documents(DEFINES)
            .iter()
            .map(String::as_str)
            .collect();
        // A file that has left the list keeps nothing here, as in the annotations module.
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
                .insert(key.to_owned(), (fresh, defines::read_defines(&text)));
        }
    }
}
