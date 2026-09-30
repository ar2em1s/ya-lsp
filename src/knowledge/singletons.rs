//! Ruby's `Singleton`, as a body of knowledge: a class that writes `include Singleton` answers
//! `instance`.
//!
//! The reading is `workspace::singletons`'. Not switchable, like the inflector: it is how Ruby's own
//! library behaves, not a framework's convention.

use super::{Counted, Declared, Declaring, ListId, Wants};
use crate::generated::candidates;
use crate::workspace::{DocUri, Features, singletons};

/// Documents that name the constant `Singleton`.
pub const SINGLETONS: ListId = ListId("singletons.singletons");

static WANTS: [Wants; 1] = [Wants {
    list: SINGLETONS,
    calls: &[],
    constants: &[singletons::SINGLETON],
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

/// Ruby's `Singleton`.
#[derive(Debug, Default)]
pub struct Singletons;

impl super::Knowledge for Singletons {
    fn name(&self) -> &'static str {
        "singletons"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn wants(&self) -> &'static [Wants] {
        &WANTS
    }

    fn wanted(&self, list: ListId, _features: Features) -> bool {
        list == SINGLETONS
    }

    /// `instance` on every class whose `include Singleton` names Ruby's module: not where the
    /// project declares a `Singleton` of its own nearer the class.
    fn declare(&mut self, declaring: &Declaring<'_>, into: &mut Declared) -> Counted {
        let classes = &declaring.context.classes;
        let mut declared = 0;
        for uri in declaring
            .context
            .documents(SINGLETONS)
            .iter()
            .filter_map(|uri| DocUri::from_graph_uri(uri))
        {
            let Some(source) = (declaring.text)(&uri) else {
                continue;
            };
            let found: Vec<singletons::Included> = singletons::read_singletons(&source)
                .into_iter()
                .filter(|found| {
                    candidates(&found.class, singletons::SINGLETON)
                        .iter()
                        .take_while(|name| name.as_str() != singletons::SINGLETON)
                        .all(|name| !classes.contains(name))
                })
                .collect();
            declared += found.len();
            super::add(
                into,
                &uri,
                singletons::facts(&found, &(declaring.caption)(&uri)),
            );
        }
        vec![("singletons", declared)]
    }
}
