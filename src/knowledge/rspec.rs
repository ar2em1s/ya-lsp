//! RSpec, as the proof that the seam is real.
//!
//! **This declares one thing and ships nothing**, both deliberately. What it proves is that a body
//! of knowledge can be added without editing core, so the test is how many files outside this one
//! it takes: **none**. A shipping RSpec module is separate work, made cheap by this one.
//!
//! It is the right prover because it is shaped **unlike** Rails in the two ways that matter.
//!
//! **Its documents live in a tree `environment.rs` fences**, and that already works: the fence's
//! target list is read of the *cursor* too, and the cursor list is wider (a cursor inside `spec/`
//! sees `spec/`), so `let(:user)` answering inside a spec is legal without touching the fence.
//!
//! **Its declarations have no named owner.** `RSpec.describe Foo do` is an anonymous subclass, so
//! `let(:user)` has no class to hang off. That is an **RBS bound, not a Rails one** (RBS cannot
//! declare on an anonymous class either), and the crate already has the answer: a relation class is
//! a name this pass mints, and so is a `Struct`'s unspellable namespace. This mints one per file,
//! enough to prove the shape; a shipping module would mint one per `describe`.

use std::collections::BTreeSet;

use super::{Counted, Declared, Declaring, ListId, Wants};
use crate::generated::{Declared as Member, Facts, Owner, Source};
use crate::workspace::{DocUri, Features};

/// Files whose name ends `_spec.rb`.
pub const GROUPS: ListId = ListId("rspec.groups");

static WANTS: [Wants; 1] = [Wants {
    list: GROUPS,
    calls: &[],
    constants: &[],
    modules: &[],
    defines: &[],
    path: Some("_spec.rb"),
    tags: false,
    inherits: false,
    // a gem's own specs are about a project that is not this one: `is_own_code`'s sentence, read
    // straight
    engines: false,
    gems: false,
    reads_only: false,
}];

/// The example groups a spec file writes.
#[derive(Debug, Default)]
pub struct RSpec;

impl super::Knowledge for RSpec {
    fn name(&self) -> &'static str {
        "rspec"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn wants(&self) -> &'static [Wants] {
        &WANTS
    }

    /// No key of its own, because this ships nothing a project could decline.
    fn wanted(&self, list: ListId, _features: Features) -> bool {
        list == GROUPS
    }

    /// Every `let(:name)` in the file, as an instance method on the group the file mints.
    ///
    /// A scan, not a parse, because what is proved is the seam, not the reader. The reader a
    /// shipping module needs is a Prism walk of nested `describe` blocks, which is that work, not
    /// this.
    fn declare(&mut self, declaring: &Declaring<'_>, into: &mut Declared) -> Counted {
        let mut named = 0;
        for uri in declaring
            .context
            .documents(GROUPS)
            .iter()
            .filter_map(|uri| DocUri::from_graph_uri(uri))
        {
            let Some(source) = (declaring.text)(&uri) else {
                continue;
            };
            let owner = Owner::Instance(group(&uri));
            let mut facts = Facts::default();
            for name in lets(&source) {
                facts.declare(Member {
                    owner: owner.clone(),
                    name,
                    parameters: "()".to_owned(),
                    returns: "untyped".to_owned(),
                    overloads: Vec::new(),
                    because: format!("A `let` in `{}`.", (declaring.caption)(&uri)),
                    at: None,
                    from: Source::Derived,
                });
                named += 1;
            }
            super::add(into, &uri, facts);
        }
        vec![("example-group helpers", named)]
    }
}

/// The name this file's example group is minted under.
///
/// A name nothing else can write, for the relation class's reason: the group is an anonymous
/// subclass, so any name is invented, and one that could collide with a constant somebody wrote
/// would take their class off the map.
fn group(uri: &DocUri) -> String {
    let stem = uri
        .as_str()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .trim_end_matches(".rb");
    let camel: String = stem
        .split('_')
        .filter_map(|word| {
            let mut letters = word.chars();
            let first = letters.next()?;
            Some(first.to_uppercase().collect::<String>() + letters.as_str())
        })
        .collect();
    format!("RSpecExampleGroup::{camel}")
}

/// Every `let(:name)` the source writes, in order and once each.
fn lets(source: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for line in source.lines() {
        let line = line.trim_start();
        let Some(rest) = line
            .strip_prefix("let(:")
            .or_else(|| line.strip_prefix("let!(:"))
        else {
            continue;
        };
        // **The closing paren must be there.** `split(')').next()` answers `Some` for every input,
        // so an unterminated `let(:name` would declare a member off a line that is not Ruby.
        // `split_once` asks the question the scan means.
        let Some((name, _)) = rest.split_once(')') else {
            continue;
        };
        // `let()` names nothing, and a group writing one `let` twice declares one member: the
        // second is the one that runs, and either way there is one name.
        if !name.is_empty() && seen.insert(name.to_owned()) {
            found.push(name.to_owned());
        }
    }
    found
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_let_the_scan_reads_and_the_four_lines_it_refuses() {
        let read = lets(
            "\
RSpec.describe Story do
  let(:story) { Story.new }
  let!(:author) { User.new }
    let(:story) { Story.other }
  let(:) { nil }
  let(:unterminated
  letter(:not_a_let) { 1 }
  it \"works\" do
  end
end
",
        );
        // In order and once each: the indented re-`let` is the same name, `let(:)` names nothing,
        // `let(:unterminated` never closes, and `letter(` is a different call that starts the same
        // way.
        assert_eq!(read, vec!["story".to_owned(), "author".to_owned()]);
    }

    #[test]
    fn the_group_a_file_mints_is_named_after_the_file() {
        let uri = DocUri::from_path(std::path::Path::new("/app/spec/models/story_spec.rb"))
            .expect("a file uri");
        assert_eq!(group(&uri), "RSpecExampleGroup::StorySpec");
    }
}
