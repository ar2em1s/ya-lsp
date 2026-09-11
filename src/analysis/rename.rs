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
    use crate::analysis::position::TextDocument;
    use crate::analysis::requests::file_name;
    use crate::analysis::testing::*;

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

    #[test]
    fn a_class_a_generated_document_declares_is_refused_by_rename() {
        // The existing guard covers this with nothing added. Renaming reads *every* definition of a
        // name and refuses unless all of them are somewhere ya-lsp is willing to edit — the case it
        // was written for is `class String` reopened beside Ruby's own signatures — and a generated
        // definition is not the user's own code by exactly the same test.
        //
        // The refusal is the safe answer and not a placeholder for a better one: the alternative
        // is a rename that reaches through the mapping and edits `db/schema.rb`, which is a
        // generated file whose column is not renamed by rewriting it.
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = synthetic_project(source);
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));
        harness.open(&uri, source);

        let renamed = harness.ask(
            "textDocument/rename",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": position_of(source, "Story"),
                "newName": "Article",
            }),
        );
        assert!(renamed.is_null(), "{renamed}");
        assert!(
            harness
                .messages()
                .iter()
                .any(|message| message.contains("Story")),
            "a refusal is said out loud"
        );
    }

    /// One spelling, `name`, used as five different variables in one file.
    ///
    /// The point of the fixture is that a word search cannot tell any of them apart. `name` is a
    /// method parameter, a block parameter shadowing it, a lambda parameter shadowing it again,
    /// a local in an unrelated method, and a word inside a comment and a string. A rename of any
    /// one of them must leave the other four exactly as they were, and the drawing is where that
    /// is read.
    const LOCALS: &str = "\
def greet(name)
  greeting = \"Hi #{name}\" # the name goes here
  [1, 2].each { |name| puts name }
  shout = ->(name) { name.upcase }
  \"name\" + greeting + shout.call(name)
end

def unrelated
  name = 1
  name + 1
end
";

    #[test]
    fn renaming_a_local_changes_its_own_scope_and_nothing_that_merely_spells_it() {
        let mut harness = Harness::new();
        let uri = harness.write("app/greet.rb", LOCALS);
        harness.index();

        // The parameter of `greet`, which is read twice: inside the interpolation, and in the
        // last line's argument. Everything else spelled `name` belongs to something else.
        assert_eq!(
            harness.renamed(&uri, LOCALS, "name)", "person"),
            "\
--- greet.rb ---
def greet(person)
  greeting = \"Hi #{person}\" # the name goes here
  [1, 2].each { |name| puts name }
  shout = ->(name) { name.upcase }
  \"name\" + greeting + shout.call(person)
end

def unrelated
  name = 1
  name + 1
end
"
        );
    }

    #[test]
    fn renaming_a_block_parameter_stops_at_the_block() {
        let mut harness = Harness::new();
        let uri = harness.write("app/greet.rb", LOCALS);
        harness.index();

        // The block's own `name`, which shadows the parameter. Prism resolves the two to
        // different scopes and that indexing is the whole of the rule — nothing here knows the
        // word "shadow".
        assert_eq!(
            harness.renamed(&uri, LOCALS, "name| puts", "each_one"),
            "\
--- greet.rb ---
def greet(name)
  greeting = \"Hi #{name}\" # the name goes here
  [1, 2].each { |each_one| puts each_one }
  shout = ->(name) { name.upcase }
  \"name\" + greeting + shout.call(name)
end

def unrelated
  name = 1
  name + 1
end
"
        );
    }

    #[test]
    fn a_word_in_a_comment_or_a_string_is_not_a_position_a_rename_answers_for() {
        let mut harness = Harness::new();
        let uri = harness.write("app/greet.rb", LOCALS);
        harness.index();

        // The two places an editor's own word matching would offer to rename. `null` from
        // `prepareRename` is what stops the box from opening at all, and nothing is said about
        // it: the cursor is on prose, and there is no refusal to explain.
        for needle in ["name goes here", "\"name\" +"] {
            assert!(
                harness.prepare_rename(&uri, LOCALS, needle).is_null(),
                "{needle:?} is not renameable"
            );
        }
        assert!(harness.messages().is_empty());
    }

    const ADMIN: &str = "\
class Admin
  def hire
    HR::Person.build
  end
end

module Other
  class Person
  end
end

def elsewhere
  Other::Person.new
end
";

    #[test]
    fn a_rename_edits_the_suite_and_is_never_fenced_by_a_test_tree() {
        // The surface where dropping a test tree would do real damage rather than merely hide a
        // row: a completion list that leaves out a spec-only name costs a keystroke, and a
        // rename that leaves out the spec's uses costs a red suite and a diff the user has
        // already accepted. `environment` names this request as one that must never ask, and
        // this is where that is pinned. A constant, because a method rename is declined here
        // for an unrelated and much older reason — see this module's header.
        let mut harness = Harness::new();
        let source = "class Store\n  def ship\n  end\nend\n";
        let store = harness.write("app/models/store.rb", source);
        harness.write(
            "spec/models/store_spec.rb",
            "describe Store do\n  it \"ships\" do\n    Store.new.ship\n  end\nend\n",
        );
        harness.index();

        assert_eq!(
            harness.renamed(&store, source, "Store", "Depot"),
            "\
--- store.rb ---
class Depot
  def ship
  end
end
--- store_spec.rb ---
describe Depot do
  it \"ships\" do
    Depot.new.ship
  end
end
"
        );
    }

    #[test]
    fn renaming_a_constant_follows_the_resolution_across_files_and_namespaces() {
        let mut harness = Harness::new();
        let hr = harness.write("app/hr.rb", HR);
        harness.write("app/admin.rb", ADMIN);
        harness.index();

        // Every spelling of the one constant changes: the `class` line, the bare `Person`
        // inside its own namespace, the superclass of `Boss`, and the qualified `HR::Person` in
        // both files. Only the last segment of a qualified reference moves, which is a fact
        // about how rubydex records them rather than anything this had to arrange.
        //
        // `Other::Person` and the `class Person` inside `module Other` are the control, and
        // they are in the drawing rather than in a second assertion: `admin.rb` is shown whole,
        // so the two names that did not change are as visible as the one that did.
        assert_eq!(
            harness.renamed(&hr, HR, "Person\n", "Employee"),
            "\
--- admin.rb ---
class Admin
  def hire
    HR::Employee.build
  end
end

module Other
  class Person
  end
end

def elsewhere
  Other::Person.new
end
--- hr.rb ---
module HR
  class Employee
    ROLE = \"staff\"

    def self.build
      Employee.new
    end
  end

  class Boss < Employee
    def peer
      HR::Employee.new
    end
  end
end
"
        );
    }

    #[test]
    fn renaming_a_constant_from_a_use_of_it_answers_the_same_as_from_where_it_is_written() {
        let mut harness = Harness::new();
        let hr = harness.write("app/hr.rb", HR);
        let admin = harness.write("app/admin.rb", ADMIN);
        harness.index();

        let from_the_class_line = harness.renamed(&hr, HR, "Person\n", "Employee");
        // The `Person` in `HR::Person` in the *other* file: a constant reference rather than a
        // definition, which reaches the plan down a different arm of `locate`.
        let from_a_qualified_use = harness.renamed(&admin, ADMIN, "Person.build", "Employee");
        assert_eq!(from_a_qualified_use, from_the_class_line);
    }

    #[test]
    fn a_constant_assigned_rather_than_declared_moves_with_its_namespace() {
        let mut harness = Harness::new();
        let source = "\
module HR
  MAX_STAFF = 10

  def self.room?
    HR::MAX_STAFF > 1 && MAX_STAFF < 100
  end
end
";
        let uri = harness.write("app/limits.rb", source);
        harness.index();

        // `MAX_STAFF = 10` is a constant rather than a namespace, and rubydex records no name
        // span for one — `locator::spans` falls back to the whole construct, which for this
        // kind is exactly the name and nothing else. Worth a test rather than a comment,
        // because the *next* fixture is the kind where that fallback is not the name.
        assert_eq!(
            harness.renamed(&uri, source, "MAX_STAFF = ", "MAX_HEADCOUNT"),
            "\
--- limits.rb ---
module HR
  MAX_HEADCOUNT = 10

  def self.room?
    HR::MAX_HEADCOUNT > 1 && MAX_HEADCOUNT < 100
  end
end
"
        );
    }

    #[test]
    fn a_class_made_with_class_new_is_narrowed_to_its_name_rather_than_replaced_whole() {
        let mut harness = Harness::new();
        let source = "\
module HR
  Failure = Class.new(StandardError)
  Shim = Module.new

  def self.fail!
    raise Failure
  end
end
";
        let uri = harness.write("app/errors.rb", source);
        harness.index();

        // The case that makes the confirmation step load-bearing rather than defensive.
        // rubydex promotes `Failure = Class.new(StandardError)` to a class, and the *name* span
        // it records for it is the whole assignment — so a rename that trusted the span would
        // write `raise BuildFailed` and, on the line above, replace the entire
        // `Failure = Class.new(StandardError)` with `BuildFailed`, deleting the class.
        //
        // `StandardError` is in the drawing on purpose: it ends in the very name being narrowed
        // to, and it is why the search inside the span is for a whole word.
        assert_eq!(
            harness.renamed(&uri, source, "Failure = ", "BuildFailed"),
            "\
--- errors.rb ---
module HR
  BuildFailed = Class.new(StandardError)
  Shim = Module.new

  def self.fail!
    raise BuildFailed
  end
end
"
        );
        assert!(harness.messages().is_empty(), "nothing to explain");
    }

    #[test]
    fn a_name_written_twice_inside_one_span_refuses_rather_than_guessing_which_is_which() {
        let mut harness = Harness::new();
        let source = "\
Registry = Class.new { include Registry }
";
        let uri = harness.write("app/registry.rb", source);
        harness.index();

        // The other side of the narrowing rule. Two whole-word occurrences inside the one span
        // rubydex hands back, either of which could be the one being defined, so nothing is
        // changed and the sentence says which file to look at.
        assert_eq!(
            harness.renamed(&uri, source, "Registry = ", "Catalogue"),
            "null"
        );
        assert_eq!(
            harness.messages(),
            vec![messages::rename_could_not_confirm(
                "Registry",
                "registry.rb"
            )]
        );
    }

    #[test]
    fn a_method_is_refused_out_loud_rather_than_renamed_by_a_name_match() {
        let mut harness = Harness::new();
        let source = "\
class Person
  def shout
    :loud
  end
end

class Siren
  def shout
    :louder
  end
end

Person.new.shout
";
        let uri = harness.write("app/shout.rb", source);
        harness.index();

        // Two classes define `shout`, which is the ordinary reason a method rename would go
        // wrong: `references` matches a method by name, so a rename built on it would edit
        // `Siren#shout` and the call below along with the one asked about, and would look as
        // though it had worked.
        //
        // From the `def` line and from the call site alike, since those reach the plan down
        // different arms of `locate`.
        for needle in ["shout\n    :loud", "shout\n"] {
            assert!(harness.prepare_rename(&uri, source, needle).is_null());
            assert_eq!(harness.messages(), vec![messages::rename_refuses_methods()]);
        }
    }

    #[test]
    fn an_instance_variable_is_refused_because_a_subclass_can_share_it() {
        let mut harness = Harness::new();
        let source = "\
class Person
  def initialize
    @name = \"anon\"
  end

  def name
    @name
  end
end
";
        let uri = harness.write("app/person.rb", source);
        harness.index();

        // The scope walk answers for `@name` — `documentHighlight` lights both of these up —
        // and the refusal is the plan's rather than a limit of the walk: what makes it unsafe
        // is a subclass or an included module in another file writing the same name, which one
        // file cannot see.
        assert!(harness.prepare_rename(&uri, source, "@name = ").is_null());
        assert_eq!(
            harness.messages(),
            vec![messages::rename_refuses_instance_variables()]
        );
    }

    #[test]
    fn a_parameter_ruby_supplies_has_nowhere_to_put_a_new_name() {
        let mut harness = Harness::new();
        let source = "\
[1, 2].each { it + 1 }
[3, 4].each { _1 * 2 }
";
        let uri = harness.write("app/implicit.rb", source);
        harness.index();

        // `it` and `_1` are read everywhere and written nowhere, because the block declares
        // them rather than the file naming them. Renaming one would mean writing a parameter
        // list that is not there, which is a refactoring rather than a rename.
        for (needle, name) in [("it +", "it"), ("_1 *", "_1")] {
            assert!(harness.prepare_rename(&uri, source, needle).is_null());
            assert_eq!(
                harness.messages(),
                vec![messages::rename_refuses_implicit_parameters(name)]
            );
        }
    }

    #[test]
    fn a_variable_that_is_also_a_keyword_or_a_hash_key_is_refused() {
        let mut harness = Harness::new();
        let source = "\
def call(host:, port:)
  config = { host:, port: port }
  connect(host:)
  config
end

def other(host)
  host.to_s
end
";
        let uri = harness.write("app/call.rb", source);
        harness.index();

        // Three ordinary Ruby spellings put a name somewhere it means more than the variable,
        // and all three are in this fixture. `host:` in the parameter list is the method's
        // interface, so renaming it changes what every caller writes; `{ host:, ... }` and
        // `connect(host:)` are Ruby 3.1's shorthand, where the one word is the key *and* a read
        // of the local — replacing the span renames the key with it, which changes the hash and
        // still parses.
        for needle in ["host:, port:)", "host:, port: port", "host:)"] {
            assert!(
                harness.prepare_rename(&uri, source, needle).is_null(),
                "{needle:?}"
            );
            assert_eq!(
                harness.messages(),
                vec![messages::rename_refuses_shorthand("host")]
            );
        }

        // `port` is written out in full at its one read, and refused all the same: the keyword
        // parameter that declares it is the interface either way.
        assert!(harness.prepare_rename(&uri, source, "port }").is_null());
        assert_eq!(
            harness.messages(),
            vec![messages::rename_refuses_shorthand("port")]
        );

        // And the control: the same spelling in a method that takes it positionally renames,
        // because nothing about a positional parameter's name reaches a caller.
        assert_eq!(
            harness.renamed(&uri, source, "host)", "hostname"),
            "\
--- call.rb ---
def call(host:, port:)
  config = { host:, port: port }
  connect(host:)
  config
end

def other(hostname)
  hostname.to_s
end
"
        );
    }

    #[test]
    fn a_local_before_a_double_colon_renames_because_that_colon_is_a_lookup() {
        let mut harness = Harness::new();
        let source = "\
def read
  source = Object
  source::NAME
end
";
        let uri = harness.write("app/read.rb", source);
        harness.index();

        // The reason the shorthand test is for one colon rather than for a colon. `source` here
        // is a local with a constant looked up on it, which is the commonest legitimate
        // spelling of a name followed by a colon and must not be caught by the rule above.
        assert_eq!(
            harness.renamed(&uri, source, "source =", "holder"),
            "\
--- read.rb ---
def read
  holder = Object
  holder::NAME
end
"
        );
    }

    #[test]
    fn a_new_name_ruby_would_not_read_as_a_name_changes_nothing() {
        let mut harness = Harness::new();
        let uri = harness.write("app/greet.rb", LOCALS);
        harness.index();

        // The prepare said yes, so the position is fine and it is the *name* that is not. LSP
        // has nowhere to put a validation rule, so the client asks with whatever was typed and
        // this is the only place it can be answered.
        assert!(harness.prepare_rename(&uri, LOCALS, "name)").is_object());
        for candidate in ["Person", "nil", "shout!", "two words", ""] {
            assert_eq!(
                harness.renamed(&uri, LOCALS, "name)", candidate),
                "null",
                "{candidate:?}"
            );
            assert_eq!(
                harness.messages(),
                vec![messages::rename_needs_a_ruby_name(candidate, false)]
            );
        }
    }

    #[test]
    fn a_constant_cannot_be_renamed_to_something_that_is_not_one() {
        let mut harness = Harness::new();
        let hr = harness.write("app/hr.rb", HR);
        harness.write("app/admin.rb", ADMIN);
        harness.index();

        // The other half of the rule, and the reason the plan carries which kind it is: a
        // constant renamed to a lowercase name is not a constant any more, and every reference
        // to it would stop resolving. `HR::Employee` is refused too — it is a path rather than
        // a name, and only the last segment of a reference is ever replaced, so splicing one in
        // would write `HR::HR::Employee` at the qualified use sites.
        for candidate in ["employee", "HR::Employee", "@Employee"] {
            assert_eq!(
                harness.renamed(&hr, HR, "Person\n", candidate),
                "null",
                "{candidate:?}"
            );
            assert_eq!(
                harness.messages(),
                vec![messages::rename_needs_a_ruby_name(candidate, true)]
            );
        }
    }

    #[test]
    fn a_name_defined_in_a_gem_is_refused_from_the_project_that_uses_it() {
        let (dir, _gem_home, env) =
            project_with_gem("module Shouty\n  class Megaphone\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "Shouty::Megaphone.new\n";
        let uri = harness.write("app/main.rb", source);
        harness.index();
        harness.index_gems();

        // Renaming this would edit the gem, or edit the project and leave the gem defining the
        // old name. Both are wrong, and the sentence says which it is rather than leaving the
        // editor to report that nothing can be renamed here.
        assert!(harness.prepare_rename(&uri, source, "Megaphone").is_null());
        assert_eq!(
            harness.messages(),
            vec![messages::rename_refuses_foreign("Megaphone")]
        );
    }

    #[test]
    fn a_class_the_project_reopens_from_a_gem_is_refused_along_with_it() {
        let (dir, _gem_home, env) =
            project_with_gem("module Shouty\n  class Megaphone\n  end\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);

        let source = "\
module Shouty
  class Megaphone
    def blast
      :loud
    end
  end
end
";
        let uri = harness.write("lib/reopen.rb", source);
        harness.index();
        harness.index_gems();

        // The case the rule is really about, and the one a "is any of it mine?" test would get
        // wrong: the project reopens a class the gem defines, so one of the two places the name
        // is written is a file ya-lsp will not edit. Renaming the project's half alone would
        // leave the gem defining `Megaphone` and the project defining something else.
        assert!(harness.prepare_rename(&uri, source, "Megaphone").is_null());
        assert_eq!(
            harness.messages(),
            vec![messages::rename_refuses_foreign("Megaphone")]
        );
    }

    #[test]
    fn nothing_inside_a_gem_is_renameable_even_from_a_position_that_would_be_exact() {
        let (dir, gem_home, env) = project_with_gem(
            "module Shouty\n  def self.blast(volume)\n    volume * 2\n  end\nend\n",
        );
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        harness.write("app/main.rb", "Shouty.blast(1)\n");
        harness.index();
        harness.index_gems();

        // `volume` is a local, which is exact wherever it is written — so the refusal is not
        // about precision, it is that ya-lsp never proposes an edit to a file that is not the
        // user's own. Silently, as every other request inside a bundle is: a gem is opened to
        // be read, and nobody pressing rename in one expects it to work.
        let inside = DocUri::from_path(&gem_home.path().join("gems/shouty-1.2.3/lib/shouty.rb"))
            .expect("a gem file");
        let source = "module Shouty\n  def self.blast(volume)\n    volume * 2\n  end\nend\n";
        assert!(harness.prepare_rename(&inside, source, "volume)").is_null());
        assert!(harness.messages().is_empty(), "nothing said, deliberately");
    }

    #[test]
    fn an_edit_carries_the_version_it_was_computed_against_when_the_client_takes_one() {
        let mut harness = Harness::new();
        let uri = harness.write("app/greet.rb", LOCALS);
        harness.index();
        harness.open(&uri, LOCALS);

        let answer = harness.ask(
            "textDocument/rename",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": position_of(LOCALS, "name)"),
                "newName": "person",
            }),
        );
        // The richer shape, and the reason it is worth negotiating for: the version pins the
        // text the edit was computed against, so a client can reject a rename the user has
        // typed past rather than applying it to text that has moved.
        assert_eq!(answer["changes"], serde_json::Value::Null);
        assert_eq!(answer["documentChanges"][0]["textDocument"]["version"], 1);
        assert_eq!(
            answer["documentChanges"][0]["textDocument"]["uri"],
            serde_json::json!(uri.to_lsp().expect("an LSP uri")),
        );
    }

    #[test]
    fn a_client_that_did_not_ask_for_document_changes_gets_the_older_map() {
        let mut harness = Harness::new();
        harness.analysis.client.versioned_edits = false;
        let uri = harness.write("app/greet.rb", LOCALS);
        harness.index();

        // A client that did not advertise `documentChanges` may not merely ignore the shape it
        // did not ask for; it can fail to apply the edit at all. The older map has no version
        // in it, which is exactly what advertising the newer one buys.
        let answer = harness.ask(
            "textDocument/rename",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": position_of(LOCALS, "name)"),
                "newName": "person",
            }),
        );
        assert_eq!(answer["documentChanges"], serde_json::Value::Null);
        let edits = &answer["changes"][uri.to_lsp().expect("an LSP uri").as_str()];
        assert_eq!(edits.as_array().map(Vec::len), Some(3), "{answer}");
    }

    #[test]
    fn the_prepare_range_is_the_word_under_the_cursor_and_not_the_span_it_was_found_in() {
        let mut harness = Harness::new();
        let source = "Failure = Class.new(StandardError)\n";
        let uri = harness.write("app/errors.rb", source);
        harness.index();

        // The editor puts its rename box over exactly this range and pre-fills it with the text
        // inside, so the range has to be the name rather than the span rubydex recorded — which
        // for this shape is the whole assignment.
        assert_eq!(
            harness.prepare_rename(&uri, source, "Failure"),
            serde_json::json!({
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 7 },
            })
        );
        // And a cursor inside the recorded span but outside the name answers `null` rather than
        // offering to rename something the cursor is not on. Here it lands on the `=`.
        assert!(harness.prepare_rename(&uri, source, "= Class").is_null());
    }

    #[test]
    fn a_singleton_class_is_not_a_name_anybody_typed() {
        let mut harness = Harness::new();
        let source = "\
class Person
  class << self
    def build
      new
    end
  end
end
";
        let uri = harness.write("app/person.rb", source);
        harness.index();

        // A cursor on `class << self` resolves to the singleton, whose name the graph spells
        // `Person::<Person>`. Asked of the *old* name, the same check that vets a new one rules
        // that out — and silently, because nobody meant to rename it.
        // The name span rubydex records for `class << self` is the `self`, which is where a
        // cursor has to be for this to be reached at all.
        assert!(harness.prepare_rename(&uri, source, "self\n").is_null());
        assert!(harness.messages().is_empty());
    }

    #[test]
    fn a_constant_that_resolves_to_nothing_is_nothing_to_rename() {
        let mut harness = Harness::new();
        let source = "Missing::Gone.new
";
        let uri = harness.write("app/typo.rb", source);
        harness.index();

        // Both halves of a name nothing in the graph defines — a typo, or a gem that did not
        // resolve. There is no set of places it is written to change, so there is nothing to
        // refuse either: the answer is the same `null` a comment gets.
        for needle in ["Missing", "Gone"] {
            assert!(harness.prepare_rename(&uri, source, needle).is_null());
        }
        assert!(harness.messages().is_empty());
    }

    /// A rename drawn over the file the user actually has, markup and all.
    ///
    /// `Harness::renamed` draws what `with_text` hands out, which for a template is the blanked
    /// view — right for a Ruby file, and the thing that would hide the whole question here. The
    /// ranges a rename returns are the *template's* own offsets, so applying them to the real
    /// file is both the honest drawing and the assertion that byte-preserving blanking works.
    fn renamed_on_disk(
        harness: &mut Harness,
        uri: &DocUri,
        source: &str,
        needle: &str,
        to: &str,
    ) -> String {
        let answer = harness.ask(
            "textDocument/rename",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": position_of(source, needle),
                "newName": to,
            }),
        );
        if answer.is_null() {
            return "null".to_owned();
        }
        let mut drawn = Vec::new();
        for (at, edits) in edits_in(&answer) {
            let at = DocUri::from_uri_str(&at).expect("a document URI");
            let on_disk = std::fs::read_to_string(at.to_path().expect("a path")).expect("readable");
            let mut text = TextDocument::new(on_disk, harness.analysis.encoding);
            for edit in edits.iter().rev() {
                text.apply(Some(edit.range), &edit.new_text);
            }
            drawn.push(format!("--- {} ---\n{}", file_name(&at), text.text()));
        }
        drawn.join("")
    }

    #[test]
    fn a_local_renames_across_the_tag_it_was_declared_in() {
        // `rename` is the only module in the crate that writes, and `COVERAGE_FLOORS` holds it
        // at 100 for that reason — so the template path ships with its own fixtures or not at
        // all. The block parameter is declared in one tag and read in another, and what makes
        // the two edits land on the real file is that blanking moved no byte.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        let view = harness.write("app/views/stories/index.html.erb", VIEW);
        harness.index();

        assert_eq!(
            renamed_on_disk(&mut harness, &view, VIEW, "story| %>", "item"),
            "\
--- index.html.erb ---
<h1>Stories</h1>
<% @stories.each do |item| %>
  <p><%= item.title %> &mdash; <%= Story::TAGLINE %></p>
<% end %>
"
        );
    }

    #[test]
    fn a_constant_a_template_names_renames_with_its_declaration() {
        // The half that reaches out of the template: the
        // declaration is in a Ruby file this rename has to edit as well, at coordinates from
        // two different coordinate systems that are the same coordinate system.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        let view = harness.write("app/views/stories/index.html.erb", VIEW);
        harness.index();

        assert_eq!(
            renamed_on_disk(&mut harness, &view, VIEW, "TAGLINE %>", "STRAPLINE"),
            "\
--- story.rb ---
class Story
  STRAPLINE = \"news\"

  def title
    @title
  end
end
--- index.html.erb ---
<h1>Stories</h1>
<% @stories.each do |story| %>
  <p><%= story.title %> &mdash; <%= Story::STRAPLINE %></p>
<% end %>
"
        );
    }

    #[test]
    fn a_namespace_nothing_writes_down_is_refused_and_what_it_holds_is_not() {
        let mut harness = Harness::new();
        let source = "\
Ghost::Thing = 1

def read
  Ghost::Thing
end
";
        let uri = harness.write("app/ghost.rb", source);
        harness.index();

        // `Ghost::Thing = 1` with no `module Ghost` anywhere leaves rubydex holding a `Ghost`
        // that is written down in no file at all. Renaming it would change every use of a name
        // whose one definition is somewhere ya-lsp cannot see — an excluded file, a gem that
        // did not resolve, a constant some metaprogramming makes — so it is refused for the
        // same reason a gem's name is, and with the same sentence.
        assert!(harness.prepare_rename(&uri, source, "Ghost").is_null());
        assert_eq!(
            harness.messages(),
            vec![messages::rename_refuses_foreign("Ghost")]
        );

        // And the control, which is what makes that a rule about the namespace rather than
        // about the line: the constant inside it is written down here, so it renames.
        assert_eq!(
            harness.renamed(&uri, source, "Thing = ", "Wraith"),
            "\
--- ghost.rb ---
Ghost::Wraith = 1

def read
  Ghost::Wraith
end
"
        );
    }
}
