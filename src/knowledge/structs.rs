//! `Struct.new` and `Data.define`, as a body of knowledge.
//!
//! The reader is [`analysis::structs`](crate::analysis::structs), and it is plain Ruby, not a
//! framework: which spelling names a class, what each installs, and the `def` in the block that
//! keeps its member.

use std::collections::{BTreeSet, HashMap};

use super::{Counted, Declared, Declaring, Fresh, ListId, Sources, Wants};
use crate::analysis::structs;
use crate::generated::Facts;
use crate::workspace::{DocUri, Features};

/// Documents that reference the constant `Struct` or `Data`.
///
/// The one list filled by a **constant** reference, not by a call, a path or a superclass, and it
/// must be: what puts a document here is `Struct.new`, whose *method* name is `new`, a filter
/// almost no file fails. The constant is rare, and rubydex records it at index time exactly as it
/// records a method reference.
pub const STRUCTS: ListId = ListId("structs.structs");

static WANTS: [Wants; 1] = [Wants {
    list: STRUCTS,
    calls: &[],
    constants: &["Struct", "Data"],
    modules: &[],
    defines: &[],
    path: None,
    spells: &[],
    tags: false,
    inherits: false,
    // `Point = Struct.new(:x)` in a gem's `app/` declares `Point#x`: the engine rule read straight.
    // The members are on a class the reader can name, and nothing about the call is scoped to an
    // application
    engines: true,
    gems: false,
    reads_only: false,
    buffers: false,
}];

/// The two constructors the language ships.
#[derive(Debug, Default)]
pub struct Structs {
    /// What the reader made of each listed file, by URI ([`Held`]).
    sources: HashMap<String, Held>,
    /// How many files it has read.
    pub reads: u64,
}

/// One file's facts, and what they were read against.
///
/// **Facts, not a parse**, against [`Knowledge::refresh`](super::Knowledge::refresh)'s advice,
/// because the reader asks the projection one question while it parses: whether a class's name
/// can be spelled ([`structs::read_asking`]). So the facts are held with every answer they rest
/// on, and read again when the text moved or any answer did.
#[derive(Debug)]
struct Held {
    fresh: Fresh,
    asked: Vec<(String, bool)>,
    facts: Facts,
}

impl super::Knowledge for Structs {
    fn name(&self) -> &'static str {
        "structs"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn wants(&self) -> &'static [Wants] {
        &WANTS
    }

    fn wanted(&self, list: ListId, features: Features) -> bool {
        list == STRUCTS && features.structs
    }

    /// What a `Struct.new` or a `Data.define` installs on the constant it is assigned.
    ///
    /// The same shape as the annotations module, for the same reason: a reader that needs nothing
    /// but the text and the file's name needs nothing from the graph either. [`structs::read`]
    /// decides which documents on this list really write one; a file that mentions `Struct` and
    /// never calls it says nothing and is not recorded.
    ///
    /// Held between passes ([`Held`]): re-reading every listed file each settle was a tenth of a
    /// large app's pass.
    fn declare(&mut self, declaring: &Declaring<'_>, into: &mut Declared) -> Counted {
        let mut members = 0;
        // A listed file with nothing held could not be read (it is gone).
        for (held, uri) in declaring
            .context
            .documents(STRUCTS)
            .iter()
            .filter_map(|key| self.sources.get(key).zip(DocUri::from_graph_uri(key)))
        {
            members += held.facts.len();
            super::add(into, &uri, held.facts.clone());
        }
        vec![("struct members", members)]
    }

    fn refresh(&mut self, sources: &Sources<'_>) {
        let listed: BTreeSet<&str> = sources
            .context
            .documents(STRUCTS)
            .iter()
            .map(String::as_str)
            .collect();
        // A file that has left the list keeps nothing here, as in the annotations module.
        self.sources.retain(|uri, _| listed.contains(uri.as_str()));
        let namespaces = &sources.context.namespaces;
        for (key, uri) in listed
            .into_iter()
            .filter_map(|key| DocUri::from_graph_uri(key).map(|uri| (key, uri)))
        {
            let fresh = (sources.fresh)(&uri);
            if self.sources.get(key).is_some_and(|held| {
                held.fresh == fresh
                    && held
                        .asked
                        .iter()
                        .all(|(name, was)| namespaces.spellable(name) == *was)
            }) {
                continue;
            }
            let Some(text) = (sources.text)(&uri) else {
                self.sources.remove(key);
                continue;
            };
            self.reads += 1;
            let (facts, asked) = structs::read_asking(&text, &(sources.caption)(&uri), namespaces);
            self.sources.insert(
                key.to_owned(),
                Held {
                    fresh,
                    asked,
                    facts,
                },
            );
        }
    }
}
