//! `require "..."`: the one under the cursor, and every one in the file.
//!
//! rubydex indexes the *call* to `require` as a method reference, but not its argument, so the
//! path string is invisible to the graph — and the path is exactly where people click. Finding
//! it means looking at the syntax, which is why this is the one place outside rubydex that
//! reaches for Prism directly.
//!
//! A text scan of the line would be cheaper and wrong: it would fire inside comments, inside
//! heredocs, and on `# require "foo"` in documentation.

use ruby_prism::{CallNode, Visit};

/// A require call whose path the cursor is inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Require {
    /// `require_relative` resolves against the requiring file's directory rather than the
    /// load path.
    pub relative: bool,
    /// The path as written, without quotes and without any `.rb` suffix.
    pub path: String,
    /// The span of the path text, quotes excluded — what the editor underlines.
    pub start: u32,
    pub end: u32,
}

/// The require whose path contains `offset`, if any.
#[must_use]
pub fn at(source: &str, offset: u32) -> Option<Require> {
    find(source, Some(offset)).into_iter().next()
}

/// Every `require` and `require_relative` in the file, in source order.
///
/// The same walk as [`at`] with the hit test taken out, and that is the whole difference between
/// the two: `definition` asks about the one path the cursor is in, `documentLink` asks about all
/// of them at once. A second visitor would be a second answer to what counts as a require, and
/// the two would one day disagree about `Foo.require "x"`.
#[must_use]
pub fn all(source: &str) -> Vec<Require> {
    find(source, None)
}

/// One parse and one walk, whichever of the two asked.
fn find(source: &str, offset: Option<u32>) -> Vec<Require> {
    let result = ruby_prism::parse(source.as_bytes());
    let mut finder = Finder {
        offset,
        found: Vec::new(),
    };
    finder.visit(&result.node());
    finder.found
}

struct Finder {
    /// `Some` when only the require containing this offset is wanted. It is both halves of what
    /// separates the two callers: what the hit test compares against, and what makes the walk
    /// stop at the first hit — with no offset there is nothing to stop at, because every require
    /// in the file is one.
    offset: Option<u32>,
    found: Vec<Require>,
}

impl<'pr> Visit<'pr> for Finder {
    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        if self.settled() {
            return;
        }
        if let Some(require) = self.require(node) {
            self.found.push(require);
        }
        if !self.settled() {
            ruby_prism::visit_call_node(self, node);
        }
    }
}

impl Finder {
    /// Whether there is anything left to look for.
    fn settled(&self) -> bool {
        self.offset.is_some() && !self.found.is_empty()
    }

    fn require(&self, node: &CallNode<'_>) -> Option<Require> {
        let relative = match node.name().as_slice() {
            b"require" => false,
            b"require_relative" => true,
            _ => return None,
        };
        // `Foo.require "x"` is somebody else's method, not Kernel's.
        if node.receiver().is_some() {
            return None;
        }

        let arguments = node.arguments()?;
        let first = arguments.arguments().iter().next()?;
        let string = first.as_string_node()?;

        // The hit test uses the whole literal, quotes included, so that a cursor resting on the
        // opening quote still counts; the reported span is the content, which is what the
        // editor should underline. `all` has no cursor and skips it — the *span* is the same
        // either way, so a link and a jump underline the same characters.
        let literal = string.location();
        if let Some(offset) = self.offset
            && (offset < literal.start_offset() as u32 || offset > literal.end_offset() as u32)
        {
            return None;
        }

        let content = string.content_loc();
        let path = String::from_utf8_lossy(string.unescaped()).into_owned();
        Some(Require {
            relative,
            path: path.trim_end_matches(".rb").to_owned(),
            start: content.start_offset() as u32,
            end: content.end_offset() as u32,
        })
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::testing::*;

    fn find(source: &str, needle: &str) -> Option<Require> {
        let offset = source.find(needle).expect("needle") as u32;
        at(source, offset)
    }

    #[test]
    fn finds_the_path_and_reports_the_span_without_quotes() {
        let source = "require \"lib/person\"\n";
        let found = find(source, "lib/person").expect("a require");
        assert_eq!(found.path, "lib/person");
        assert!(!found.relative);
        assert_eq!(
            &source[found.start as usize..found.end as usize],
            "lib/person"
        );
    }

    #[test]
    fn a_require_written_on_a_receiver_is_somebody_elses_method() {
        // `Kernel#require` is the one this navigates. `Foo.require "x"` is a method that
        // happens to share the name, and its argument is an ordinary string.
        assert_eq!(find("Foo.require \"person\"\n", "person"), None);
        assert_eq!(find("self.require_relative \"sibling\"\n", "sibling"), None);
    }

    #[test]
    fn distinguishes_require_relative() {
        let found = find("require_relative \"sibling\"\n", "sibling").expect("a require");
        assert!(found.relative);
        assert_eq!(found.path, "sibling");
    }

    #[test]
    fn a_trailing_rb_is_dropped_the_way_ruby_drops_it() {
        assert_eq!(
            find("require \"person.rb\"\n", "person").unwrap().path,
            "person"
        );
    }

    #[test]
    fn the_cursor_has_to_be_in_the_path() {
        // On the method name, not the argument: that is a method reference, and the graph
        // answers for it.
        let source = "require \"person\"\n";
        assert!(at(source, 0).is_none());
        assert!(at(source, source.len() as u32).is_none());
    }

    #[test]
    fn a_require_in_a_comment_or_a_string_is_not_a_require() {
        // The whole reason this parses instead of scanning the line.
        assert!(find("# require \"person\"\n", "person").is_none());
        assert!(find("puts \"require \\\"person\\\"\"\n", "person").is_none());
    }

    #[test]
    fn an_interpolated_path_is_left_alone() {
        // `require "lib/#{name}"` has no static path to resolve.
        let source = "require \"lib/#{name}\"\n";
        assert!(at(source, 10).is_none());
    }

    #[test]
    fn a_require_nested_in_a_block_is_still_found() {
        let source = "if RUBY_VERSION > \"3\"\n  require \"person\"\nend\n";
        assert_eq!(find(source, "person\"").unwrap().path, "person");
    }

    #[test]
    fn an_unparseable_file_does_not_panic() {
        assert!(at("class Broken\n  def foo\n", 5).is_none());
        assert!(all("class Broken\n  def foo\n").is_empty());
    }

    #[test]
    fn all_keeps_every_require_in_source_order() {
        // The half `at` cannot answer: none of these is under a cursor, and the order is the
        // file's because that is the order the client underlines them in.
        let source = "require \"a\"\nrequire_relative \"b\"\nif x\n  require \"c\"\nend\n";
        let found = all(source);

        assert_eq!(
            found
                .iter()
                .map(|require| (require.relative, require.path.as_str()))
                .collect::<Vec<_>>(),
            vec![(false, "a"), (true, "b"), (false, "c")]
        );
        // The same spans `at` reports, so a link and a jump underline the same characters.
        for require in &found {
            assert_eq!(
                at(source, require.start),
                Some(require.clone()),
                "{}",
                require.path
            );
        }
    }

    #[test]
    fn all_declines_exactly_what_at_declines() {
        // One walk, so this is not a second rule: a receiver, an interpolated path, a comment
        // and a string are each skipped by the code both callers go through.
        assert!(all("Foo.require \"person\"\n").is_empty());
        assert!(all("require \"lib/#{name}\"\n").is_empty());
        assert!(all("# require \"person\"\nputs \"require \\\"x\\\"\"\n").is_empty());
    }

    #[test]
    fn a_require_path_navigates_to_the_file_it_names() {
        // The graph indexes the call to `require` but never its argument, so this is the one
        // navigation answer that comes from parsing rather than from the index.
        let mut harness = Harness::new();
        let library = harness.write("lib/person.rb", LIBRARY);
        let source = "require \"person\"\nrequire_relative \"person\"\nrequire \"nope\"\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        for needle in ["person\"\nrequire_relative", "person\"\nrequire \"nope"] {
            let targets = harness.definition_at(&caller, source, needle);
            assert_eq!(
                targets[0]["targetUri"],
                serde_json::json!(library.as_str()),
                "{needle}: {targets}"
            );
            assert_eq!(targets[0]["targetRange"]["start"]["line"], 0);
        }

        assert_eq!(
            harness.definition_at(&caller, source, "nope"),
            serde_json::Value::Null,
            "a require of a file we do not have is not an error"
        );
    }

    #[test]
    fn every_require_in_the_file_is_a_link_and_one_that_resolves_nowhere_is_not() {
        // The same source the jump above navigates, asked without a cursor. Both spellings are
        // underlined and the third line is not: a link whose target the graph cannot name would
        // be an underline that opens nothing, which is worse than leaving the path plain. The
        // protocol's other spelling — a link with no `target`, resolved on click — says the same
        // thing a round trip later and there is nothing here that a round trip could learn.
        let mut harness = Harness::new();
        let library = harness.write("lib/person.rb", LIBRARY);
        let source = "require \"person\"\nrequire_relative \"person\"\nrequire \"nope\"\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        assert_eq!(
            harness.links(&caller, source),
            serde_json::json!([
                {
                    "range": {
                        "start": { "line": 0, "character": 9 },
                        "end": { "line": 0, "character": 15 },
                    },
                    "target": library.as_str(),
                },
                {
                    "range": {
                        "start": { "line": 1, "character": 18 },
                        "end": { "line": 1, "character": 24 },
                    },
                    "target": library.as_str(),
                },
            ])
        );
    }

    #[test]
    fn a_file_whose_requires_all_resolve_nowhere_answers_null() {
        // `null` rather than `[]`, which is what every other whole-file answer here says when
        // it found nothing, and the shape a client reads as "no links" rather than as "a link
        // list that happens to be empty this keystroke".
        let mut harness = Harness::new();
        let source = "require \"nope\"\nclass Person\nend\n";
        let caller = harness.write("lib/main.rb", source);
        harness.index();

        assert_eq!(harness.links(&caller, source), serde_json::Value::Null);
    }

    #[test]
    fn a_link_in_a_template_is_placed_in_the_markup_the_editor_has() {
        // A template is read as its Ruby view — markup blanked, one space per byte — and
        // addressed as the text the client holds. The require is parsed out of the first and
        // the range comes back in the second, so the underline lands on the path rather than
        // that many columns into the `<% %>` it is written in.
        let mut harness = Harness::new();
        let library = harness.write("lib/person.rb", LIBRARY);
        let source = "<h1>Hi</h1>\n<% require \"person\" %>\n";
        let template = harness.write("app/views/stories/show.html.erb", source);
        harness.index();

        assert_eq!(
            harness.links(&template, source),
            serde_json::json!([{
                "range": {
                    "start": { "line": 1, "character": 12 },
                    "end": { "line": 1, "character": 18 },
                },
                "target": library.as_str(),
            }])
        );
    }
}
