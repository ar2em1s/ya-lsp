//! `require "..."` under the cursor.
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
    let result = ruby_prism::parse(source.as_bytes());
    let mut finder = Finder {
        offset,
        found: None,
    };
    finder.visit(&result.node());
    finder.found
}

struct Finder {
    offset: u32,
    found: Option<Require>,
}

impl<'pr> Visit<'pr> for Finder {
    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        if self.found.is_none() {
            self.found = self.require_at(node);
        }
        if self.found.is_none() {
            ruby_prism::visit_call_node(self, node);
        }
    }
}

impl Finder {
    fn require_at(&self, node: &CallNode<'_>) -> Option<Require> {
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
        // editor should underline.
        let literal = string.location();
        if self.offset < literal.start_offset() as u32 || self.offset > literal.end_offset() as u32
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

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
