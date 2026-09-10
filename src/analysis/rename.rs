//! `textDocument/prepareRename` and `textDocument/rename` — the one request that writes.
//!
//! # Why this module is mostly refusals
//!
//! Every other request ya-lsp answers is read-only: being wrong shows the user something
//! unhelpful and they look elsewhere. A rename edits their files, so being wrong here means code
//! that does not run, or — worse — code that runs and means something else, discovered whenever
//! the branch is next opened. The shape of this module is therefore not "how much can be renamed"
//! but "what can be renamed *exactly*", and everything else is declined out loud.
//!
//! Two things are exact:
//!
//! - **Locals and block parameters**, from [`scopes`](super::scopes). Prism resolves every
//!   local-variable node to the scope it belongs to, so "every place this variable appears" is a
//!   fact about the file rather than a text search.
//! - **Constants**, from the graph. rubydex's resolver links each constant reference to what it
//!   resolves to, so `Person` inside `module HR` and `HR::Person` at the top level are known to be
//!   one constant, and a `Person` in another namespace is known not to be.
//!
//! Two are declined. **Methods**, because `textDocument/references` finds a method's uses by name
//! alone, and a rename built on that would edit `call`, `id` and `name` in hundreds of unrelated
//! places while looking like it had worked. **Instance variables**, because `@name` is exact
//! inside one class body and stops being exact the moment a subclass or an included module writes
//! the same name.
//!
//! # The guard that makes it safe
//!
//! A plan is a list of byte spans, and *every span is checked against the bytes it is about to
//! replace* before a single edit is emitted. That check is not defensive padding; it fires on
//! ordinary Ruby. rubydex promotes `Error = Class.new(StandardError)` to a class whose name span
//! is the **entire assignment**, so a rename that trusted the span would replace
//! `Error = Class.new(StandardError)` with `Failure` and delete the class along with the name.
//! [`narrow`] turns that into a correct one-word edit, and refuses when it cannot.
//!
//! A refusal is always whole. No path here edits some of the places a name is written and not the
//! rest, because a half-applied rename is the one outcome worse than no rename at all.

use std::collections::HashSet;

use ruby_prism::{ConstantWriteNode, LocalVariableWriteNode, Location, Visit};
use rubydex::model::{declaration::Declaration, graph::Graph, ids::UriId};

use super::{
    locator::{self, Located, Target},
    references, render, scopes,
    synthesized::Synthesized,
};
use crate::messages;

/// One span a rename would replace, in bytes into the document `uri` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// The graph's own spelling of the document URI.
    pub uri: String,
    pub start: u32,
    pub end: u32,
}

/// What a rename at a position comes to.
#[derive(Debug)]
pub enum Plan {
    /// Every span spelling `name`, all of which have to change together.
    Edits {
        name: String,
        /// Which kind of name the replacement has to be: a constant, or a variable.
        constant: bool,
        edits: Vec<Edit>,
    },
    /// A position ya-lsp *could* have answered for and deliberately does not, with the sentence
    /// that says why.
    ///
    /// Distinct from [`Plan::Nothing`] because the two reach the user differently: a refusal is
    /// something they need to know, since they asked for this one on purpose by pressing a key.
    Refused(String),
    /// A position a rename has nothing to do with — a comment, a keyword, a string's insides.
    Nothing,
}

/// The rename at `offset`, or why there is not one.
///
/// `uri` is the document's URI as the graph spells it, which is what the edits for a local
/// carry: those never leave the file, so there is no lookup to do for them.
#[must_use]
pub fn plan(
    graph: &Graph,
    synthesized: &Synthesized,
    uri: &str,
    source: &str,
    offset: u32,
    own: &HashSet<UriId>,
) -> Plan {
    // The scope walk is asked first, for the reason `highlight` asks it first: it is the half
    // that can say no, claiming the cursor only when the cursor really is on a variable.
    if let Some((name, occurrences)) = scopes::variable(source, offset) {
        return variable(uri, source, &name, &occurrences);
    }
    constant(graph, synthesized, UriId::from(uri), offset, own)
}

/// A local, a parameter or an instance variable.
fn variable(uri: &str, source: &str, name: &str, occurrences: &[scopes::Occurrence]) -> Plan {
    // An instance variable's name carries its sigil, which is the whole test. The refusal is
    // the plan's, not a limit of the scope walk: `@name` is exact inside one class body and is
    // not exact once a subclass or an included module writes the same name, and the walk cannot
    // see either of those from one file.
    if name.starts_with('@') {
        return Plan::Refused(messages::rename_refuses_instance_variables());
    }
    // `it` and `_1` are read at every occurrence and written at none, because Ruby supplies
    // them rather than the file declaring them. There is nowhere to put the new name.
    if !occurrences.iter().any(|at| at.write) {
        return Plan::Refused(messages::rename_refuses_implicit_parameters(name));
    }
    if occurrences.iter().any(|at| is_shorthand(source, at.end)) {
        return Plan::Refused(messages::rename_refuses_shorthand(name));
    }
    Plan::Edits {
        name: name.to_owned(),
        constant: false,
        edits: occurrences
            .iter()
            .map(|at| Edit {
                uri: uri.to_owned(),
                start: at.start,
                end: at.end,
            })
            .collect(),
    }
}

/// Whether the name ending at `end` is written in a shorthand where it means more than itself.
///
/// Three ordinary Ruby spellings put a variable's name somewhere it is also something else, and
/// all three are followed by a colon:
///
/// - `def f(a:)` — a keyword parameter, whose name is part of the method's interface. Renaming
///   it changes what every caller has to write, and those callers are found by matching a
///   method name, which is the search this module refuses to build a rename on.
/// - `{ a:, b: }` — Ruby 3.1's hash shorthand, where the one word is the key *and* a read of
///   the local. Replacing the span renames the key too, which changes the hash rather than the
///   variable, and still parses.
/// - `f(a:)` — the same shorthand in an argument list, with the same consequence.
///
/// `::` is deliberately not one of them: `mod::CONST` is a constant looked up on a local, and
/// that colon belongs to the lookup. Without the second test the commonest legitimate spelling
/// of a local before a colon would be refused along with the three above.
fn is_shorthand(source: &str, end: u32) -> bool {
    let rest = source.as_bytes().get(end as usize..).unwrap_or_default();
    rest.first() == Some(&b':') && rest.get(1) != Some(&b':')
}

/// A constant, or something the graph resolves that is not one.
fn constant(
    graph: &Graph,
    synthesized: &Synthesized,
    uri_id: UriId,
    offset: u32,
    own: &HashSet<UriId>,
) -> Plan {
    // As in goto-definition and references: several targets can share the narrowest span, so
    // take the first that has something to say rather than the first that exists.
    locator::locate(graph, uri_id, offset)
        .into_iter()
        .find_map(|located| decide(graph, synthesized, &located, own))
        .unwrap_or(Plan::Nothing)
}

/// What one target comes to, or `None` to ask the next one that shares its span.
fn decide(
    graph: &Graph,
    synthesized: &Synthesized,
    located: &Located<'_>,
    own: &HashSet<UriId>,
) -> Option<Plan> {
    let resolution = locator::resolve(graph, located);
    if resolution.declarations.is_empty() {
        return None;
    }
    // A call is a method however it resolved — and `Target::Definition` is whichever of the two
    // was defined, which only the resolution knows: `def` and `attr_reader` are both methods,
    // `class` and `=` are both constants.
    if matches!(located.target, Target::Call(_)) || defines_a_method(graph, &resolution) {
        return Some(Plan::Refused(messages::rename_refuses_methods()));
    }

    let name = render::last_segment(
        graph
            .declarations()
            .get(&resolution.declarations[0])?
            .name(),
    );
    // Anything the graph fabricated rather than read: a singleton's `<Person>`, and every other
    // name no one could have typed. Silent, because the cursor is on `class << self` or the
    // like, and nobody meant to rename that.
    if !is_name(name, true) {
        return Some(Plan::Nothing);
    }
    let name = name.to_owned();

    // Every place it is written has to be somewhere ya-lsp is willing to edit, and a bundle is
    // not. One `class String` reopened in the user's own code is the case this is really about:
    // its other definition is in Ruby's own signatures, so renaming it would rename half of a
    // name and leave core Ruby calling the other half.
    let sites: Vec<&rubydex::model::definitions::Definition> = resolution
        .declarations
        .iter()
        .flat_map(|id| locator::definitions_of(graph, *id))
        .collect();
    if sites.is_empty() || !sites.iter().all(|site| own.contains(site.uri_id())) {
        return Some(Plan::Refused(messages::rename_refuses_foreign(&name)));
    }

    // `references` already answers exactly this question, filters the fabricated references out
    // of it, and confines it to the user's own code. `include_declaration` is not optional
    // here: a rename that changes every use and not the `class` line is broken code.
    let edits = references::find(graph, synthesized, located, &resolution, own, true)
        .into_iter()
        .map(|reference| Edit {
            uri: reference.uri,
            start: reference.start,
            end: reference.end,
        })
        .collect();
    Some(Plan::Edits {
        name,
        constant: true,
        edits,
    })
}

fn defines_a_method(graph: &Graph, resolution: &locator::Resolution) -> bool {
    resolution
        .declarations
        .iter()
        .filter_map(|id| graph.declarations().get(id))
        .any(|declaration| matches!(declaration, Declaration::Method(_)))
}

/// The part of `span` that spells `name`, as byte offsets into `span`.
///
/// Almost always the whole of it, and the exception is not hypothetical. rubydex promotes
/// `Error = Class.new(StandardError)` to a class, and the name span it records for that one is
/// the entire assignment — so a rename that replaced the span would delete the `Class.new` with
/// it. `Shim = Module.new` is the same shape. Narrowing to a *whole-word* occurrence inside the
/// span recovers those, and requiring it to be the **only** one is what keeps it honest:
/// `Registry = Class.new { include Registry }` has two, either of which could be the one being
/// declared, so it refuses rather than guess.
///
/// `None` also covers a span that does not contain the name at all, which is what a stale offset
/// looks like — a file edited between the plan and the check.
#[must_use]
pub fn narrow(span: &str, name: &str) -> Option<(u32, u32)> {
    if span == name {
        return Some((0, name.len() as u32));
    }
    let mut only = None;
    for (at, _) in span.match_indices(name) {
        if !is_whole_word(span, at, name.len()) {
            continue;
        }
        if only.is_some() {
            return None;
        }
        only = Some((at as u32, (at + name.len()) as u32));
    }
    only
}

/// Whether the `len` bytes at `at` are a name rather than part of a longer one.
///
/// This is what keeps the `Error` in `StandardError` from counting as a second occurrence of
/// `Error`, which is the difference between narrowing `Error = Class.new(StandardError)` and
/// refusing it.
fn is_whole_word(span: &str, at: usize, len: usize) -> bool {
    let before = span[..at].chars().next_back();
    let after = span[at + len..].chars().next();
    !before.is_some_and(is_name_char) && !after.is_some_and(is_name_char)
}

fn is_name_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// Whether Ruby would read `candidate`, on its own, as exactly a constant or exactly a variable.
///
/// **Prism is the authority and not a pattern of our own**, and the list of things it gets right
/// for free is the argument: that `nil`, `self`, `true`, `_1` and `__FILE__` cannot be assigned
/// to; that `x!` and `x?` are calls and not names; that `Ünicode` is a *constant* while `é` is a
/// variable, because Ruby's rule is the letter's case in Unicode rather than its being ASCII;
/// and that `x y`, which parses cleanly, is a call rather than the name it looks like. A regular
/// expression gets the last three wrong, and a hand-written keyword list goes stale the next
/// time Ruby adds one.
///
/// Asked of the *old* name too, which is what rules out every name the graph fabricated.
///
/// Warnings are deliberately not a gate: `x = 1` on its own warns that nothing ever reads `x`,
/// so a warning here says something about the probe rather than about the name.
#[must_use]
pub fn is_name(candidate: &str, constant: bool) -> bool {
    let source = format!("{candidate} = 1");
    let result = ruby_prism::parse(source.as_bytes());
    if result.errors().next().is_some() {
        return false;
    }
    let mut walk = Assigned {
        source: &source,
        constant,
        spelled: None,
    };
    walk.visit(&result.node());
    // Compared against the candidate rather than merely being present, because a name Prism
    // spells differently from the way it was written is not the same name: `" x"` assigns a
    // local called `x`, and `"a = 1\nb"` assigns one called `b`.
    walk.spelled.as_deref() == Some(candidate)
}

/// The name of the first assignment of the kind being asked about, as it is written.
///
/// A walk rather than a look at the first statement: it needs no case for a root that is not a
/// program, for an empty body, or for a statement that is not an assignment at all — each of
/// which is a branch that could only ever be taken one way. Anything the walk does not find is
/// a name that was not written, which is the answer either way.
struct Assigned<'s> {
    source: &'s str,
    constant: bool,
    spelled: Option<String>,
}

impl Assigned<'_> {
    fn take(&mut self, at: &Location<'_>) {
        let spelled = self.source[at.start_offset()..at.end_offset()].to_owned();
        self.spelled.get_or_insert(spelled);
    }
}

impl<'pr> Visit<'pr> for Assigned<'_> {
    fn visit_constant_write_node(&mut self, node: &ConstantWriteNode<'pr>) {
        if self.constant {
            self.take(&node.name_loc());
        }
    }

    fn visit_local_variable_write_node(&mut self, node: &LocalVariableWriteNode<'pr>) {
        if !self.constant {
            self.take(&node.name_loc());
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prism_decides_what_a_ruby_name_is_and_it_is_not_what_a_pattern_would_say() {
        // The point of the table is the disagreements. Everything above the blank line a
        // regular expression would also get right; everything below it is a case where one
        // would be wrong, and is why this asks Prism instead.
        for (candidate, constant, expected) in [
            ("name", false, true),
            ("_name", false, true),
            ("Person", true, true),
            ("MAX_AGE", true, true),
            ("X", true, true),
            ("name", true, false),
            ("Person", false, false),
            ("@name", false, false),
            ("$name", false, false),
            ("", false, false),
            ("HR::Person", true, false),
            //
            // A keyword, and the five names that look assignable and are not.
            ("nil", false, false),
            ("self", false, false),
            ("true", false, false),
            ("def", false, false),
            ("__FILE__", false, false),
            // Ruby's numbered block parameter: a name, and not one that may be assigned.
            ("_1", false, false),
            // Method names, which a pattern for "identifier" would accept.
            ("shout!", false, false),
            ("empty?", false, false),
            // Two words. This parses with no error at all, as a call with an argument, which is
            // the case that makes checking the parse *errors* insufficient on its own.
            ("first second", false, false),
            // Leading whitespace, likewise: `" x"` assigns a local, but not one called `" x"`.
            (" name", false, false),
            // Two statements, from a name with a newline in it.
            ("first\nsecond", false, false),
            // Ruby's case rule is Unicode's, not ASCII's: one of these is a constant and the
            // other is a variable, and neither is both.
            ("é", false, true),
            ("é", true, false),
            ("Ünicode", true, true),
            ("Ünicode", false, false),
        ] {
            assert_eq!(
                is_name(candidate, constant),
                expected,
                "is_name({candidate:?}, constant: {constant})"
            );
        }
    }

    #[test]
    fn a_span_wider_than_the_name_narrows_to_the_name_or_refuses() {
        // The first of these is why `narrow` exists at all: rubydex records the name span of
        // `Error = Class.new(StandardError)` as the whole assignment, and `StandardError` ends
        // in the very name being narrowed to.
        assert_eq!(
            narrow("Error = Class.new(StandardError)", "Error"),
            Some((0, 5))
        );
        assert_eq!(narrow("Wrapper = Class.new", "Wrapper"), Some((0, 7)));
        assert_eq!(narrow("Shim = Module.new", "Shim"), Some((0, 4)));
        // Exactly the name, which is every other span in the graph.
        assert_eq!(narrow("Person", "Person"), Some((0, 6)));
        // Two of them, either of which could be the one being defined.
        assert_eq!(
            narrow("Registry = Class.new { include Registry }", "Registry"),
            None
        );
        // Not in there at all, which is what a span left over from a file that has since been
        // edited looks like.
        assert_eq!(narrow("def shout", "Person"), None);
        // Present only as part of a longer name, which is not an occurrence of it.
        assert_eq!(narrow("StandardError.new", "Error"), None);
        assert_eq!(narrow("max_age = 1", "age"), None);
    }

    #[test]
    fn a_local_before_a_double_colon_is_not_a_shorthand() {
        // `mod::CONST` is a constant looked up on a local, and it is the reason the test is for
        // one colon rather than for a colon. Without the second half of it, the commonest
        // legitimate spelling of a local followed by a colon would be refused.
        assert!(is_shorthand("f(a:)", 3));
        assert!(is_shorthand("{ a:, b: }", 3));
        assert!(!is_shorthand("mod::CONST", 3));
        // The end of the file: a name is the last thing in it, so there is no byte to read.
        assert!(!is_shorthand("name", 4));
    }
}
