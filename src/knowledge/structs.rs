//! `Struct.new` and `Data.define`, as a body of knowledge.
//!
//! The reader is [`analysis::structs`](crate::analysis::structs) and it is plain Ruby rather than
//! a framework: which spelling names a class, what each installs, the `def` in the block that
//! keeps its member.

use super::{Counted, Declared, Declaring, ListId, Wants};
use crate::analysis::structs;
use crate::workspace::{DocUri, Features};

/// Documents that reference the constant `Struct` or `Data`.
///
/// The one list filled by a **constant** reference rather than by a call, a path or a superclass,
/// and it has to be: what puts a document here is `Struct.new`, whose *method* name is `new` — a
/// filter no file in any corpus would fail. The constant is the rare half (136 of discourse's
/// 11,875 `.rb` files mention either name) and rubydex records it at index time exactly as it
/// records a method reference.
pub const STRUCTS: ListId = ListId("structs.structs");

static WANTS: [Wants; 1] = [Wants {
    list: STRUCTS,
    calls: &[],
    constants: &["Struct", "Data"],
    modules: &[],
    defines: &[],
    path: None,
    tags: false,
    inherits: false,
    // `Point = Struct.new(:x)` in a gem's `app/` declares `Point#x`, which is the engine rule read
    // straight: the members are on a class the reader can name, and nothing about the call is
    // scoped to an application
    engines: true,
    gems: false,
    reads_only: false,
}];

/// The two constructors the language ships.
#[derive(Debug, Default)]
pub struct Structs;

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
    /// The same shape as the annotations module and for the same reason — a reader that needs
    /// nothing but the text and the file's name needs nothing from the graph either.
    /// [`structs::read`] decides which of the documents this list holds really writes one; a file
    /// that mentions `Struct` and never calls it says nothing and is not recorded.
    ///
    /// No memo, and that is the one difference: this reader ends at [`generated::Facts`](crate::generated::Facts) like the
    /// annotations one, and unlike it there is nothing to re-read — 136 of discourse's 11,875
    /// files mention either constant, so the list is short enough that a parse per settle is
    /// cheaper than a memo to keep fresh.
    fn declare(&mut self, declaring: &Declaring<'_>, into: &mut Declared) -> Counted {
        let mut members = 0;
        for uri in declaring
            .context
            .documents(STRUCTS)
            .iter()
            .filter_map(|uri| DocUri::from_uri_str(uri))
        {
            let Some(source) = (declaring.text)(&uri) else {
                continue;
            };
            let facts = structs::read(
                &source,
                &(declaring.caption)(&uri),
                &declaring.context.namespaces,
            );
            members += facts.len();
            super::add(into, &uri, facts);
        }
        vec![("struct members", members)]
    }
}
