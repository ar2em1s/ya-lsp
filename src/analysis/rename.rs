//! `textDocument/prepareRename` and `textDocument/rename`: the one request that writes.
//!
//! # Why this module is mostly refusals
//!
//! Every other request is read-only: a wrong answer shows something unhelpful and the user looks
//! elsewhere. A rename edits files, so a wrong answer means code that does not run, or worse, code
//! that runs and means something else, discovered whenever the branch is next opened. So the
//! question here is not "how much can be renamed" but "what can be renamed *exactly*", and
//! everything else is declined out loud.
//!
//! Two things are exact:
//!
//! - **Locals and block parameters**, from [`scopes`](super::scopes). Prism resolves every
//!   local-variable node to its scope, so "every place this variable appears" is a fact about the
//!   file, not a text search.
//! - **Constants**, from the graph. rubydex links each constant reference to what it resolves to,
//!   so `Person` inside `module HR` and `HR::Person` at top level are known to be one constant, and
//!   a `Person` in another namespace is known not to be.
//!
//! Two are declined:
//!
//! - **Methods**, because `textDocument/references` finds a method's uses by name alone, and a
//!   rename built on that would edit `call`, `id` and `name` in hundreds of unrelated places while
//!   looking like it worked.
//! - **Instance variables**, because `@name` is exact inside one class body and stops being exact
//!   once a subclass or included module writes the same name.
//!
//! # The guard that makes it safe
//!
//! A plan is a list of byte spans, and *every span is checked against the bytes it will replace*
//! before any edit is emitted. That check fires on ordinary Ruby: rubydex promotes
//! `Error = Class.new(StandardError)` to a class whose name span is the **entire assignment**, so a
//! rename trusting the span would replace the whole line with `Failure` and delete the class.
//! [`narrow`] turns that into a correct one-word edit, or refuses.
//!
//! A refusal is always whole. Nothing here edits some of the places a name is written and not the
//! rest, because a half-applied rename is the one outcome worse than none.

use std::collections::HashSet;
use std::path::Path;

use ruby_prism::{ConstantWriteNode, LocalVariableWriteNode, Location, Visit};
use rubydex::model::{declaration::Declaration, definitions::Definition, graph::Graph, ids::UriId};

use super::{
    environment,
    indexed::Indexed,
    locator::{self, Located, Target},
    references, render, scopes,
    synthesized::Synthesized,
};
use crate::messages;
use crate::workspace::rails;

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
    /// A position ya-lsp *could* answer for and deliberately does not, with the sentence saying
    /// why.
    ///
    /// Distinct from [`Plan::Nothing`] because the user hears about them differently: they asked
    /// for this one on purpose by pressing a key, so a refusal is something they need to know.
    Refused(String),
    /// A position a rename has nothing to do with: a comment, a keyword, a string's insides.
    Nothing,
}

/// The constant rename a file move implies under Zeitwerk, as a cursor and a new name.
///
/// # Why this is a rename, not a convention
///
/// Zeitwerk loads `app/models/order.rb` *expecting* it to define `Order`, so a file keeping its old
/// class under a new path raises on the next boot. The answer is the ordinary constant rename: this
/// returns a cursor, and everything after it is [`plan`] and its guards, unchanged. Nothing here
/// decides what may be edited; it decides only **which name** becomes **what**, which is the one
/// question a file move asks and a cursor does not.
///
/// # The rules, in the order they are asked
///
/// 1. **A file whose name does not spell the class inside it has nothing at stake**, wherever it
///    sits. A spec, a rake task and an initializer all stop here.
/// 2. **The old path must be where Rails would look for the class the file declares**: under an
///    autoload root, spelled exactly.
/// 3. **The new path must spell a constant in the same namespace.** A move into another directory
///    needs a `module` around the class, which is not a rename and is not attempted.
/// 4. **`camelize` promises only a capital letter**, so the derived name goes through [`is_name`]
///    like a client-supplied one. `purchase-order.rb` spells `Purchase-order`; this is the only
///    path in the crate that could otherwise write a name Ruby cannot read.
///
/// **Every one of these answers with silence, on purpose.** `messages.md` allows a sentence for a
/// rename refusal only because of the deliberate keystroke. Dragging a file is not a request for a
/// rename, so none of these earns a sentence, and the broadest rule would otherwise fire for a
/// large share of every project's files.
///
/// Rule 2's spelling is exact, and [`rails::same_constant`] (which is not) only *finds* the class.
/// An application that registers the acronym `API` declares `APIKey` where this would spell
/// `ApiKey`; the acronym table is Ruby that only runs, so ya-lsp can read that spelling in the file
/// but cannot reproduce it for a name it has to write, and does not write one.
#[must_use]
pub fn moved(graph: &Graph, uri_id: UriId, old: &Path, new: &Path) -> Option<(u32, String)> {
    let (declared, at) = declared_after_itself(graph, uri_id, old)?;
    if rails::autoloaded_constant(old)? != declared {
        return None;
    }
    let wanted = rails::autoloaded_constant(new)?;
    if scope_of(&declared) != scope_of(&wanted) {
        return None;
    }
    let to = render::last_segment(&wanted);
    (to != render::last_segment(&declared) && is_name(to, true)).then(|| (at, to.to_owned()))
}

/// The class `path`'s file name spells, as the file itself spells it, and where it is written.
///
/// Only the *last segment* is asked, through [`rails::same_constant`], so a file declaring
/// `APIKey`, or one under a namespace no directory conjures, is still found. The exactness that
/// decides whether ya-lsp may write is [`moved`]'s, on the whole name, one step later.
///
/// `None` where nothing matches, and also where **two** spellings do: `class ApiKey` and
/// `class APIKey` in one file are two classes, and nothing says which one names the file. A class
/// reopened in its own file is one name, and is not that case.
fn declared_after_itself(graph: &Graph, uri_id: UriId, path: &Path) -> Option<(String, u32)> {
    let named = rails::named_constant(path)?;
    let document = graph.documents().get(&uri_id)?;
    let mut found: Option<(String, u32)> = None;
    for definition in document
        .definitions()
        .iter()
        .filter_map(|id| graph.definitions().get(id))
        .filter(|it| matches!(it, Definition::Class(_) | Definition::Module(_)))
    {
        // A definition the resolve never linked has no name to compare, and the empty stand-in
        // equals nothing: `named` came from `camelize`, which answers `None` rather than a name
        // without a leading capital. So the arm below is one refusal, not two.
        let name = graph
            .definition_to_declaration_id(definition)
            .and_then(|id| graph.declarations().get(id))
            .map_or("", Declaration::name);
        if !rails::same_constant(render::last_segment(name), &named) {
            continue;
        }
        match &found {
            // The same class reopened lower in the same file. One name, and its first place is as
            // good a cursor as the second.
            Some((seen, _)) if seen == name => continue,
            Some(_) => return None,
            None => found = Some((name.to_owned(), definition.name_offset()?.start())),
        }
    }
    found
}

/// Everything a name has before its last segment, `""` for a name that has only one.
fn scope_of(name: &str) -> &str {
    name.rsplit_once("::").map_or("", |(scope, _)| scope)
}

/// The rename at `offset`, or why there is not one.
///
/// `uri` is the document's URI as the graph spells it, which a local's edits carry: those never
/// leave the file, so there is nothing to look up.
#[must_use]
pub fn plan(
    graph: &Indexed,
    synthesized: &Synthesized,
    uri: &str,
    source: &str,
    offset: u32,
    own: &HashSet<UriId>,
    layout: environment::Layout<'_>,
) -> Plan {
    // The scope walk is asked first, as in `highlight`: it is the half that can say no, claiming
    // the cursor only when it really is on a variable.
    if let Some((name, occurrences)) = scopes::variable(source, offset) {
        return variable(uri, source, &name, &occurrences);
    }
    constant(graph, synthesized, UriId::from(uri), offset, own, layout)
}

/// A local, a parameter or an instance variable.
fn variable(uri: &str, source: &str, name: &str, occurrences: &[scopes::Occurrence]) -> Plan {
    // An instance variable's name carries its sigil, which is the whole test. The refusal is the
    // plan's, not a limit of the scope walk: `@name` is exact inside one class body but not once a
    // subclass or included module writes the same name, and one file cannot see either.
    if name.starts_with('@') {
        return Plan::Refused(messages::rename_refuses_instance_variables());
    }
    // `it` and `_1` are read everywhere and written nowhere, because Ruby supplies them. There is
    // nowhere to put the new name.
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
/// Three ordinary Ruby spellings put a variable's name where it is also something else, all
/// followed by a colon:
///
/// - `def f(a:)`: a keyword parameter, part of the method's interface. Renaming it changes what
///   every caller writes, and callers are found by matching a method name, the search this module
///   refuses to build a rename on.
/// - `{ a:, b: }`: Ruby 3.1's hash shorthand, where one word is the key *and* a read of the local.
///   Replacing the span renames the key too, which changes the hash, not the variable, and still
///   parses.
/// - `f(a:)`: the same shorthand in an argument list, with the same effect.
///
/// `::` is deliberately excluded: `mod::CONST` is a constant looked up on a local, and that colon
/// belongs to the lookup. Without the second test, the commonest legitimate spelling of a local
/// before a colon would be refused too.
fn is_shorthand(source: &str, end: u32) -> bool {
    let rest = source.as_bytes().get(end as usize..).unwrap_or_default();
    rest.first() == Some(&b':') && rest.get(1) != Some(&b':')
}

/// A constant, or something the graph resolves that is not one.
fn constant(
    graph: &Indexed,
    synthesized: &Synthesized,
    uri_id: UriId,
    offset: u32,
    own: &HashSet<UriId>,
    layout: environment::Layout<'_>,
) -> Plan {
    // As in goto-definition and references: several targets can share the narrowest span, so take
    // the first that has something to say, not the first that exists.
    locator::locate(graph, uri_id, offset)
        .into_iter()
        // **Never the tree fence, always the outside one.** A rename must edit the suite (a work
        // list that silently omits `spec/` breaks it), and must never edit a document outside the
        // project, which this workspace does not contain.
        .find_map(|located| {
            decide(
                graph,
                synthesized,
                &located,
                own,
                environment::Fence::uses(locator::uri_of(graph, uri_id), layout),
            )
        })
        .unwrap_or(Plan::Nothing)
}

/// What one target comes to, or `None` to ask the next one that shares its span.
fn decide(
    graph: &Indexed,
    synthesized: &Synthesized,
    located: &Located<'_>,
    own: &HashSet<UriId>,
    fence: environment::Fence<'_>,
) -> Option<Plan> {
    let resolution = locator::resolve(graph, located, fence);
    if resolution.declarations.is_empty() {
        return None;
    }
    // A call is a method however it resolved, and `Target::Definition` is whichever of the two was
    // defined, which only the resolution knows: `def` and `attr_reader` are both methods, `class`
    // and `=` are both constants.
    if matches!(located.target, Target::Call(_)) || defines_a_method(graph, &resolution) {
        return Some(Plan::Refused(messages::rename_refuses_methods()));
    }

    let name = render::last_segment(
        graph
            .declarations()
            .get(&resolution.declarations[0])?
            .name(),
    );
    // Anything the graph fabricated instead of read: a singleton's `<Person>`, and any other name
    // nobody could have typed. Silent, because the cursor is on `class << self` or similar, and
    // nobody meant to rename that.
    if !is_name(name, true) {
        return Some(Plan::Nothing);
    }
    let name = name.to_owned();

    let sites: Vec<&rubydex::model::definitions::Definition> = resolution
        .declarations
        .iter()
        .flat_map(|id| locator::definitions_of(graph, *id))
        .collect();
    // **A generated document does not vote on whether a name may be renamed.** The guard below asks
    // this of every place the name is *written*, and a document ya-lsp wrote is not a place:
    // `DocUri::from_graph_uri` refuses its scheme, so a definition in one can never become an edit,
    // and it holds nothing the user typed (it was rendered from the very files this rename changes,
    // and is rewritten at the next settle). Counting it would mean *must not be renamed* where the
    // truth is only *cannot be edited*, and would block every model, mailer, job, worker, concern
    // with a class side, `Struct.new` class, and class carrying a Sorbet `sig` or YARD `@return`.
    //
    // A model whose table has not been renamed yet is fine to rename. Arguing that Rails would
    // inflect `articles` and find no table is not the server's job: `self.table_name` may already
    // be there, or the migration may be the next commit. Until it follows, the renamed model's
    // generated members thin out, which is what a half-done rename honestly looks like. See
    // `synthesized.md`.
    let written: Vec<&UriId> = sites
        .iter()
        .map(|site| site.uri_id())
        .filter(|uri| !synthesized.is_generated(uri))
        .collect();
    if written.is_empty() {
        return Some(Plan::Refused(if sites.is_empty() {
            // Written in no file at all: `Ghost::Thing = 1` with no `module Ghost` anywhere. Its
            // one definition is somewhere ya-lsp cannot see: an excluded file, an unresolved gem, a
            // constant made by metaprogramming.
            messages::rename_refuses_foreign(&name)
        } else {
            // Every declaration of it is one ya-lsp wrote: `Story::ActiveRecord_Relation`, the
            // module `routes.rs` puts the url helpers in. No file holds the name, so this is
            // "cannot", not "will not", and whatever implied it renames it.
            messages::rename_refuses_generated(&name)
        }));
    }
    // Every place it *is* written must be somewhere ya-lsp will edit, and a bundle is not. The real
    // case is one `class String` reopened in the user's code: its other definition is in Ruby's own
    // signatures, so renaming it would rename half a name and leave core Ruby calling the other
    // half.
    if !written.iter().all(|uri| own.contains(uri)) {
        return Some(Plan::Refused(messages::rename_refuses_foreign(&name)));
    }

    // `references` already answers exactly this question, drops fabricated references, and confines
    // it to the user's own code. `include_declaration` is required: a rename that changes every use
    // but not the `class` line is broken code.
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
/// Usually all of it, and the exception is real: rubydex promotes
/// `Error = Class.new(StandardError)` to a class and records the entire assignment as its name
/// span, so replacing the span would delete the `Class.new` too. `Shim = Module.new` is the same
/// shape. Narrowing to a *whole-word* occurrence recovers those, and requiring it to be the
/// **only** one keeps it honest: `Registry = Class.new { include Registry }` has two, either of
/// which could be the one declared, so it refuses instead of guessing.
///
/// `None` also covers a span that does not contain the name, which is what a stale offset looks
/// like (a file edited between the plan and the check).
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

/// Whether the `len` bytes at `at` are a name, not part of a longer one.
///
/// This keeps the `Error` in `StandardError` from counting as a second `Error`: the difference
/// between narrowing `Error = Class.new(StandardError)` and refusing it.
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
/// **Prism is the authority, not a pattern of ours**, and what it gets right for free is the
/// argument: `nil`, `self`, `true`, `_1` and `__FILE__` cannot be assigned; `x!` and `x?` are
/// calls, not names; `Ünicode` is a *constant* while `é` is a variable, because Ruby uses Unicode
/// case, not ASCII; and `x y`, which parses cleanly, is a call, not the name it looks like. A
/// regular expression gets the last three wrong, and a hand-written keyword list goes stale when
/// Ruby adds one.
///
/// Asked of the *old* name too, which rules out every name the graph fabricated.
///
/// Warnings are not a gate: `x = 1` alone warns that `x` is never read, so a warning here is about
/// the probe, not the name.
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
    // Compared against the candidate, not merely present, because a name Prism spells differently
    // from how it was written is a different name: `" x"` assigns a local called `x`, and
    // `"a = 1\nb"` assigns one called `b`.
    walk.spelled.as_deref() == Some(candidate)
}

/// The name of the first assignment of the kind being asked about, as written.
///
/// A walk, not a look at the first statement: it needs no case for a non-program root, an empty
/// body, or a non-assignment statement, each a branch that could only go one way. Anything the walk
/// does not find is a name that was not written, which is the answer either way.
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
        // The point of the table is the disagreements. Everything above the blank line a regular
        // expression would also get right; everything below it is a case where one would be wrong,
        // and why this asks Prism.
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
            // Two words. This parses with no error, as a call with an argument, which is why
            // checking parse *errors* alone is not enough.
            ("first second", false, false),
            // Leading whitespace, likewise: `" x"` assigns a local, but not one called `" x"`.
            (" name", false, false),
            // Two statements, from a name with a newline in it.
            ("first\nsecond", false, false),
            // Ruby's case rule is Unicode's, not ASCII's: one of these is a constant and the other
            // a variable, and neither is both.
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
        // The first of these is why `narrow` exists: rubydex records
        // `Error = Class.new(StandardError)`'s name span as the whole assignment, and
        // `StandardError` ends in the very name being narrowed to.
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
        // Not there at all: what a span left over from a since-edited file looks like.
        assert_eq!(narrow("def shout", "Person"), None);
        // Present only as part of a longer name, which is not an occurrence of it.
        assert_eq!(narrow("StandardError.new", "Error"), None);
        assert_eq!(narrow("max_age = 1", "age"), None);
    }

    #[test]
    fn a_local_before_a_double_colon_is_not_a_shorthand() {
        // `mod::CONST` is a constant looked up on a local, and why the test is for one colon, not
        // any colon. Without the second half, the commonest legitimate spelling of a local followed
        // by a colon would be refused.
        assert!(is_shorthand("f(a:)", 3));
        assert!(is_shorthand("{ a:, b: }", 3));
        assert!(!is_shorthand("mod::CONST", 3));
        // The end of the file: the name is the last thing in it, so there is no byte to read.
        assert!(!is_shorthand("name", 4));
    }

    #[test]
    fn a_class_a_generated_document_also_declares_renames_in_the_files_that_hold_it() {
        // **A generated document does not vote.** Renaming reads every definition of a name and
        // refuses unless all are somewhere ya-lsp will edit (the guard exists for `class String`
        // reopened beside Ruby's own signatures), and a generated definition would fail that same
        // URI test. That would conflate two kinds of "not mine": a real file this server will not
        // edit, and a document it wrote itself, which is not a file and holds nothing the user
        // typed.
        //
        // `Story` here is declared in the project's own `app/models/story.rb` *and* in the document
        // the schema implied (a generator writing one member onto a name spells `class Story … end`
        // to hang it off). Only the first is a place, and it is the only one the rename touches.
        let source = "Story.new.title\n";
        let (mut harness, schema, uri) = synthetic_project(source);
        harness.synthesize(&schema, SCHEMA_RBS, title_only(&schema));
        harness.open(&uri, source);

        assert_eq!(
            harness.renamed(&uri, source, "Story", "Article"),
            "\
--- main.rb ---
Article.new.title
--- story.rb ---
class Article
end
"
        );
        // Nothing is said, because nothing was declined.
        assert_eq!(harness.messages(), Vec::<String>::new());
    }

    #[test]
    fn a_constant_only_a_generator_declares_is_refused_and_not_as_a_gem_s() {
        // What is left once a generated document stops voting: a name no file declares at all.
        // `Story::ActiveRecord_Relation` and the route helpers' module are the real cases: written
        // down nowhere, so there is no span a rename could replace.
        //
        // It reaches the refusal below through the *same* emptiness as a name with no definition
        // anywhere, but deserves a different sentence. `Phantom` is not defined in a gem or in
        // Ruby, and "rename your own name for it instead" does not apply: the user has no other
        // name for it, and what renames it is the thing it was worked out from.
        let source = "Phantom.new\n";
        let (mut harness, schema, uri) = synthetic_project(source);
        harness.synthesize(&schema, "class Phantom\nend\n", Vec::new());
        harness.open(&uri, source);

        assert!(harness.prepare_rename(&uri, source, "Phantom").is_null());
        assert_eq!(
            harness.messages(),
            vec![messages::rename_refuses_generated("Phantom")]
        );
    }

    /// The one project every file-move test is read against: a model, and a caller of it.
    ///
    /// Written out, not shared with the fixtures above, because the *paths* are under test: a move
    /// is a path changing, and half these cases are about which directory the file lands in.
    fn a_project_with(relative: &str, source: &str) -> (Harness, DocUri) {
        let mut harness = Harness::new();
        harness.write("app/controllers/orders_controller.rb", USES_ORDER);
        let uri = harness.write(relative, source);
        harness.index();
        (harness, uri)
    }

    const ORDER: &str = "class Order\n  def total\n  end\nend\n";
    const USES_ORDER: &str = "\
class OrdersController
  def show
    Order.new.total
  end
end
";

    #[test]
    fn moving_a_model_renames_the_class_in_it_and_every_use_of_it() {
        // The headline case, and why the request is worth answering: under Zeitwerk
        // `app/models/purchase.rb` is *expected* to define `Purchase`, so a file arriving there
        // still declaring `Order` raises `NameError` on the next boot, at a point nothing connects
        // back to the drag that caused it.
        let (mut harness, uri) = a_project_with("app/models/order.rb", ORDER);

        assert_eq!(
            harness.moved_file(&uri, "app/models/purchase.rb"),
            "\
--- orders_controller.rb ---
class OrdersController
  def show
    Purchase.new.total
  end
end
--- order.rb ---
class Purchase
  def total
  end
end
"
        );
        assert_eq!(harness.messages(), Vec::<String>::new());
    }

    #[test]
    fn a_model_with_a_table_follows_its_own_file_like_any_other_class() {
        // The case that matters most, seen from the other side. A model with a table gets a
        // generated document spelling `class Story … end`, as does every mailer, job and class with
        // a Sorbet `sig`: most files anybody moves in a Rails application. If a generated document
        // voted on the rename, all of them would refuse. `title` here is written in `db/schema.rb`
        // and nowhere else.
        let mut harness = Harness::new();
        harness.write("db/schema.rb", SCHEMA);
        let uri = harness.write(
            "app/models/story.rb",
            "class Story
end
",
        );
        harness.write(
            "app/main.rb",
            "Story.new.title
",
        );
        harness.index();

        assert_eq!(
            harness.moved_file(&uri, "app/models/article.rb"),
            "\
--- main.rb ---
Article.new.title
--- story.rb ---
class Article
end
"
        );
        assert_eq!(harness.messages(), Vec::<String>::new());
    }

    /// Every shape of move that answers nothing, and the rule each stops at.
    ///
    /// **A table, not a test each, because every row has the same two assertions**: `null`, and
    /// nothing said. A drag is not a keystroke, so none of these earns a sentence; `messages.md`'s
    /// exception for a rename refusal comes from the deliberate key, not from the request being a
    /// rename. What must stay readable is *where each rule stops*, and eight rows side by side show
    /// that where eight tests asserting `"null"` would not.
    #[test]
    fn where_a_file_move_stops() {
        let rows: &[(&str, &str, &str, &str)] = &[
            // Rule 1: the file is not named after the class inside it. A spec, a rake task, an
            // initializer and a file of four unrelated classes all stop here, which keeps this from
            // answering for every file anybody moves.
            (
                "spec/models/order_spec.rb",
                "RSpec.describe Order\n",
                "spec/models/purchase_spec.rb",
                "not named after its class",
            ),
            // Rule 2, first half: outside `app/`, nothing relates a file name to a class name, and
            // whether this project autoloads `lib/` is in configuration ya-lsp does not read.
            ("lib/order.rb", ORDER, "lib/purchase.rb", "never autoloaded"),
            // The commoner direction of the same rule: an autoloaded class dragged somewhere
            // nothing loads it by name.
            (
                "app/models/order.rb",
                ORDER,
                "lib/order.rb",
                "autoloaded no more",
            ),
            // Rule 2, second half: `config/initializers/inflections.rb` is Ruby that only runs, so
            // nothing on disk says this project registered `API`. What *is* on disk is the answer
            // (the file writes `APIKey` where a default inflector spells `ApiKey`), and ya-lsp can
            // read that spelling but cannot reproduce it for a name it must write.
            (
                "app/models/api_key.rb",
                "class APIKey\nend\n",
                "app/models/api_token.rb",
                "an acronym this crate cannot spell",
            ),
            // Rule 3: `app/models/shop/order.rb` is where Zeitwerk looks for `Shop::Order`, which
            // needs a `module Shop` around the body as well as a new name: two edits of different
            // kinds, one of which reindents the whole file.
            (
                "app/models/order.rb",
                ORDER,
                "app/models/shop/order.rb",
                "a module, not a rename",
            ),
            // Rule 4: `camelize` promises only a capital letter, so the derived name goes through
            // `is_name` like a client-supplied one. This is the only path in the crate that could
            // otherwise write a name Ruby cannot read.
            (
                "app/models/order.rb",
                ORDER,
                "app/models/purchase-order.rb",
                "not a name Ruby would read",
            ),
            // Not a rule at all: the same file name under a different autoload root. Zeitwerk
            // expects `Order` at both, so there is nothing to rename.
            (
                "app/models/order.rb",
                ORDER,
                "app/services/order.rb",
                "the same class",
            ),
            // Two *spellings* in one file are two classes, and nothing says which one names the
            // file. (A class reopened in its own file is one name, and renames; see the test
            // below.)
            (
                "app/models/order.rb",
                "class Order\nend\n\nclass ORDER\nend\n",
                "app/models/purchase.rb",
                "two classes, one file name",
            ),
        ];
        for (from, source, to, why) in rows {
            let (mut harness, uri) = a_project_with(from, source);
            assert_eq!(
                harness.moved_file(&uri, to),
                "null",
                "{from} -> {to}: {why}"
            );
            assert_eq!(harness.messages(), Vec::<String>::new(), "{from}: {why}");
        }
    }

    #[test]
    fn a_namespaced_class_follows_its_own_file_and_leaves_its_directories_alone() {
        // The other half of the rule above: the directories are unchanged, so the namespace is too,
        // and only the last segment moves. The directories conjure `Chat::Thread::Policy`, and
        // nothing about it is touched.
        let source = "module Chat\n  module Thread\n    module Policy\n      class MessageExistence\n      end\n    end\n  end\nend\n";
        let (mut harness, uri) = a_project_with(
            "app/services/chat/thread/policy/message_existence.rb",
            source,
        );

        assert_eq!(
            harness.moved_file(&uri, "app/services/chat/thread/policy/message_present.rb"),
            "\
--- message_existence.rb ---
module Chat
  module Thread
    module Policy
      class MessagePresent
      end
    end
  end
end
"
        );
    }

    #[test]
    fn a_class_a_gem_also_declares_refuses_a_file_move_exactly_as_it_refuses_a_cursor() {
        // Everything after the cursor is `plan` and its guards, which is the whole design: a class
        // the project reopens from a gem has one definition in each, so renaming the project's half
        // alone would leave the gem defining the old name. A file move reaches that refusal by the
        // same route and says the same sentence.
        //
        // **It is the only sentence this request can produce**, which is worth pinning: everything
        // `moved` decides is silent, and what speaks is the shared `renaming` path, with its
        // sentences about a plan that was made but could not be carried out, not about a move
        // ya-lsp has no rule for. It fires rarely.
        let (dir, _gem_home, env) = project_with_gem("class Megaphone\nend\n");
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let uri = harness.write(
            "app/models/megaphone.rb",
            "class Megaphone\n  def blast\n  end\nend\n",
        );
        harness.index();
        harness.index_gems();

        assert_eq!(
            harness.moved_file(&uri, "app/models/loudspeaker.rb"),
            "null"
        );
        assert_eq!(
            harness.messages(),
            vec![messages::rename_refuses_foreign("Megaphone")]
        );
    }

    #[test]
    fn a_class_reopened_in_its_own_file_is_one_name_and_not_two() {
        // The other half of the table's last row. A class reopened lower in its own file gives
        // **two** definitions of **one** name, not the ambiguity that row is about, so this
        // renames, and the name's first place is as good a cursor as the second.
        let reopened = "class Order\nend\n\nclass Order\n  def total\n  end\nend\n";
        let (mut harness, uri) = a_project_with("app/models/order.rb", reopened);
        assert!(
            harness
                .moved_file(&uri, "app/models/purchase.rb")
                .contains("class Purchase")
        );
    }

    #[test]
    fn every_file_of_one_move_is_one_edit() {
        // Several files dragged together are one request, one undo step for the user, and so one
        // answer: a second `WorkspaceEdit` would be a second undo for one action.
        let mut harness = Harness::new();
        let order = harness.write("app/models/order.rb", ORDER);
        let line = harness.write("app/models/line.rb", "class Line\nend\n");
        harness.index();

        let answer = harness.ask(
            "workspace/willRenameFiles",
            serde_json::json!({
                "files": [
                    { "oldUri": order.as_str(), "newUri": moved_to(&order, "purchase.rb") },
                    { "oldUri": line.as_str(), "newUri": moved_to(&line, "item.rb") },
                ],
            }),
        );
        let changed: Vec<String> = edits_in(&answer)
            .iter()
            .map(|(uri, edits)| {
                format!("{} x{}", uri.rsplit('/').next().unwrap_or(uri), edits.len())
            })
            .collect();
        assert_eq!(changed, vec!["order.rb x1", "line.rb x1"]);
    }

    /// The same directory, a different file name, as a URI string.
    fn moved_to(from: &DocUri, name: &str) -> String {
        let path = from.to_file_path().expect("a path");
        DocUri::from_path(&path.parent().expect("a directory").join(name))
            .expect("a path")
            .as_str()
            .to_owned()
    }

    #[test]
    fn a_project_that_is_not_rails_is_never_asked_to_follow_zeitwerk() {
        // The rule is Zeitwerk's: a plain Ruby project has no relation between file name and class,
        // so following one would invent a convention the project never opted into.
        // `rails.enabled = false` is the gate, and the capability is still advertised: a request
        // method is the wire contract and is never switchable.
        let mut harness = Harness::configured("[rails]\nenabled = false\n");
        let uri = harness.write("app/models/order.rb", ORDER);
        harness.index();

        assert_eq!(harness.moved_file(&uri, "app/models/purchase.rb"), "null");
        assert_eq!(harness.messages(), Vec::<String>::new());
    }

    /// One spelling, `name`, used as five different variables in one file.
    ///
    /// A word search cannot tell them apart: a method parameter, a block parameter shadowing it, a
    /// lambda parameter shadowing it again, a local in an unrelated method, and the word in a
    /// comment and a string. Renaming any one must leave the other four exactly as they were, and
    /// the drawing is where that shows.
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

        // The parameter of `greet`, read twice: in the interpolation and in the last line's
        // argument. Everything else spelled `name` belongs to something else.
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

        // The block's own `name`, which shadows the parameter. Prism resolves the two to different
        // scopes, and that indexing is the whole rule; nothing here knows the word "shadow".
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
        // `prepareRename` stops the box from opening, and nothing is said: the cursor is on prose,
        // and there is no refusal to explain.
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
        // Where dropping a test tree would do real damage instead of hiding a row: a completion
        // list missing a spec-only name costs a keystroke, but a rename missing the spec's uses
        // costs a red suite and a diff the user already accepted. `environment` names this request
        // as one that must never fence, and this pins it. A constant, because method renames are
        // declined here for a separate reason; see the module header.
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

        // Every spelling of the one constant changes: the `class` line, the bare `Person` inside
        // its namespace, the superclass of `Boss`, and the qualified `HR::Person` in both files.
        // Only the last segment of a qualified reference moves, which is how rubydex records them,
        // not something arranged here.
        //
        // `Other::Person` and the `class Person` inside `module Other` are the control, shown in
        // the drawing instead of a second assertion: `admin.rb` is drawn whole, so the unchanged
        // names are as visible as the changed one.
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
        // The `Person` in `HR::Person` in the *other* file: a constant reference, not a definition,
        // which reaches the plan through a different arm of `locate`.
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

        // `MAX_STAFF = 10` is a constant, not a namespace, and rubydex records no name span for
        // one: `locator::spans` falls back to the whole construct, which for this kind is exactly
        // the name. A test, not a comment, because the *next* fixture is a kind where that fallback
        // is not the name.
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

        // Why the confirmation step is required, not defensive. rubydex promotes
        // `Failure = Class.new(StandardError)` to a class and records the whole assignment as its
        // *name* span, so a rename trusting the span would write `raise BuildFailed` and, on the
        // line above, replace all of `Failure = Class.new(StandardError)` with `BuildFailed`,
        // deleting the class.
        //
        // `StandardError` is in the drawing on purpose: it ends in the very name being narrowed to,
        // which is why the search inside the span is for a whole word.
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

        // The other side of the narrowing rule: two whole-word occurrences inside the one span
        // rubydex returns, either of which could be the one defined, so nothing changes and the
        // sentence says which file to look at.
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

        // Two classes define `shout`, the ordinary reason a method rename goes wrong: `references`
        // matches methods by name, so a rename built on it would edit `Siren#shout` and the call
        // below along with the one asked about, and look like it had worked.
        //
        // From the `def` line and the call site alike, since they reach the plan through different
        // arms of `locate`.
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

        // The scope walk answers for `@name` (`documentHighlight` lights both up), and the refusal
        // is the plan's, not a limit of the walk: the danger is a subclass or included module in
        // another file writing the same name, which one file cannot see.
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

        // `it` and `_1` are read everywhere and written nowhere, because the block supplies them.
        // Renaming one would mean writing a parameter list that is not there: a refactoring, not a
        // rename.
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

        // Three ordinary Ruby spellings put a name where it means more than the variable, and all
        // three are here. `host:` in the parameter list is the method's interface, so renaming it
        // changes what callers write; `{ host:, ... }` and `connect(host:)` are Ruby 3.1 shorthand,
        // where one word is the key *and* a read of the local, so replacing it renames the key too,
        // changing the hash while still parsing.
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

        // `port` is written out in full at its one read, and is refused anyway: the keyword
        // parameter declaring it is the interface either way.
        assert!(harness.prepare_rename(&uri, source, "port }").is_null());
        assert_eq!(
            harness.messages(),
            vec![messages::rename_refuses_shorthand("port")]
        );

        // The control: the same spelling in a method taking it positionally renames, because a
        // positional parameter's name never reaches a caller.
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

        // Why the shorthand test is for one colon, not any colon. `source` here is a local with a
        // constant looked up on it, the commonest legitimate spelling of a name followed by a
        // colon, which the rule above must not catch.
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

        // The prepare said yes, so the position is fine and the *name* is not. LSP has no place for
        // a validation rule, so the client asks with whatever was typed, and this is the only place
        // to answer it.
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

        // The other half of the rule, and why the plan carries its kind: a constant renamed to a
        // lowercase name is no longer a constant, and every reference would stop resolving.
        // `HR::Employee` is refused too: it is a path, not a name, and only a reference's last
        // segment is replaced, so splicing it in would write `HR::HR::Employee` at qualified uses.
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

        // Renaming this would edit the gem, or edit the project and leave the gem defining the old
        // name. Both are wrong, and the sentence says which, instead of the editor reporting that
        // nothing can be renamed here.
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

        // The case the rule is really for, and the one an "is any of it mine?" test gets wrong: the
        // project reopens a class the gem defines, so one of the two places the name is written is
        // a file ya-lsp will not edit. Renaming only the project's half would leave the gem
        // defining `Megaphone` and the project defining something else.
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

        // `volume` is a local, exact wherever it is written, so the refusal is not about precision:
        // ya-lsp never proposes an edit to a file that is not the user's own. Silently, like every
        // other request inside a bundle: a gem is opened to be read, and nobody pressing rename in
        // one expects it to work.
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
        // The richer shape, and why it is worth negotiating: the version pins the text the edit was
        // computed against, so a client can reject a rename the user has typed past instead of
        // applying it to moved text.
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

        // A client that did not advertise `documentChanges` may not just ignore that shape; it can
        // fail to apply the edit at all. The older map carries no version, which is what
        // advertising the newer one buys.
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

        // The editor places its rename box over exactly this range and pre-fills it with the text
        // inside, so the range must be the name, not the span rubydex recorded, which for this
        // shape is the whole assignment.
        assert_eq!(
            harness.prepare_rename(&uri, source, "Failure"),
            serde_json::json!({
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 7 },
            })
        );
        // A cursor inside the recorded span but outside the name answers `null` instead of offering
        // to rename something the cursor is not on. Here it lands on the `=`.
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

        // A cursor on `class << self` resolves to the singleton, which the graph spells
        // `Person::<Person>`. The check that vets a new name, asked of the *old* one, rules it out,
        // silently, because nobody meant to rename it. The name span rubydex records for
        // `class << self` is the `self`, which is where the cursor must be for this to be reached.
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

        // Both halves of a name the graph does not define: a typo, or an unresolved gem. There is
        // no set of places to change, so nothing to refuse either: the answer is the same `null` a
        // comment gets.
        for needle in ["Missing", "Gone"] {
            assert!(harness.prepare_rename(&uri, source, needle).is_null());
        }
        assert!(harness.messages().is_empty());
    }

    /// A rename drawn over the file the user really has, markup and all.
    ///
    /// `Harness::renamed` draws what `with_text` returns, which for a template is the blanked view:
    /// right for a Ruby file, and it would hide the whole question here. A rename's ranges are the
    /// *template's* own offsets, so applying them to the real file is both the honest drawing and
    /// the proof that byte-preserving blanking works.
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
            let at = DocUri::from_graph_uri(&at).expect("a document URI");
            let on_disk =
                std::fs::read_to_string(at.to_file_path().expect("a path")).expect("readable");
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
        // `rename` is the only module in the crate that writes, and `COVERAGE_FLOORS` holds it at
        // 100% for that reason, so the template path ships with its own fixtures or not at all. The
        // block parameter is declared in one tag and read in another, and blanking moving no byte
        // is what makes both edits land on the real file.
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
        // The half that reaches out of the template: the declaration is in a Ruby file this rename
        // must edit as well, at coordinates from two coordinate systems that turn out to be the
        // same one.
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
        // written in no file. Renaming it would change every use of a name whose only definition
        // ya-lsp cannot see (an excluded file, an unresolved gem, metaprogramming), so it is
        // refused for the same reason as a gem's name, with the same sentence.
        assert!(harness.prepare_rename(&uri, source, "Ghost").is_null());
        assert_eq!(
            harness.messages(),
            vec![messages::rename_refuses_foreign("Ghost")]
        );

        // The control, which makes that a rule about the namespace, not the line: the constant
        // inside it is written here, so it renames.
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
