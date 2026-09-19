//! A Sorbet `sig` and a YARD `@return`, as a body of knowledge.
//!
//! The reader is [`analysis::annotations`](crate::analysis::annotations), which ends at a
//! [`Facts`](crate::generated::Facts) directly, not at a syntax type of its own. It is also not
//! about Rails, which made it the natural first fit for this registry.

use std::collections::{BTreeSet, HashMap};

use super::{Counted, Declared, Declaring, Fresh, ListId, Sources, Wants};
use crate::analysis::annotations;
use crate::generated::Facts;
use crate::workspace::{DocUri, Features};

/// Documents holding a `sig` call, or a `@return`/`@param` tag above a `def`.
pub const ANNOTATED: ListId = ListId("annotations.annotated");

static WANTS: [Wants; 1] = [Wants {
    list: ANNOTATED,
    calls: &["sig"],
    constants: &[],
    modules: &[],
    defines: &[],
    path: None,
    tags: true,
    inherits: false,
    // a `@return` an engine's author wrote is about the engine's own method
    engines: true,
    gems: false,
    reads_only: false,
}];

/// What somebody wrote down by hand.
#[derive(Debug, Default)]
pub struct Annotations {
    /// What the reader made of each listed file, by URI.
    ///
    /// **This reader ends at [`Facts`] directly, not at a syntax type**, so the memo holds facts
    /// here where Rails' holds a parse. Cloning them is far cheaper than re-parsing the files on
    /// every pass.
    sources: HashMap<String, (Fresh, Facts)>,
    /// How many files it has read.
    pub reads: u64,
}

impl Annotations {
    /// What the reader last made of one document, or nothing.
    #[must_use]
    pub fn facts(&self, uri: &str) -> Option<&Facts> {
        self.sources.get(uri).map(|(_, facts)| facts)
    }
}

impl super::Knowledge for Annotations {
    fn name(&self) -> &'static str {
        "annotations"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn wants(&self) -> &'static [Wants] {
        &WANTS
    }

    fn wanted(&self, list: ListId, features: Features) -> bool {
        list == ANNOTATED && features.annotations
    }

    /// What each annotated file's reader already made of it, handed over as is.
    ///
    /// The whole generator: this module's memo holds [`Facts`], not a parse, so declaring is a
    /// clone and a merge.
    fn declare(&mut self, declaring: &Declaring<'_>, into: &mut Declared) -> Counted {
        let mut typed = 0;
        for key in declaring.context.documents(ANNOTATED) {
            let Some(facts) = self.facts(key).cloned() else {
                continue;
            };
            let Some(uri) = DocUri::from_graph_uri(key) else {
                continue;
            };
            typed += facts.len();
            super::add(into, &uri, facts);
        }
        vec![("annotated methods", typed)]
    }

    fn refresh(&mut self, sources: &Sources<'_>) {
        let listed: BTreeSet<&str> = sources
            .context
            .documents(ANNOTATED)
            .iter()
            .map(String::as_str)
            .collect();
        // A file that has left the list keeps nothing here: the memo is keyed by URI, and a file
        // deleted and written again is a different file.
        self.sources.retain(|uri, _| listed.contains(uri.as_str()));
        for key in listed {
            let Some(uri) = DocUri::from_graph_uri(key) else {
                continue;
            };
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
            let name = (sources.caption)(&uri);
            self.sources
                .insert(key.to_owned(), (fresh, annotations::read(&text, &name)));
        }
    }
}
