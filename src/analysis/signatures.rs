//! What ya-lsp removes from an RBS document before it reaches the index.
//!
//! One thing, and for one reason. rbs's `interface _Foo … end` declares a *structural* type — a
//! shape a value can satisfy, never a namespace a method can be called on. rubydex does not model
//! them: `visit_interface_node`'s default walks straight into the members, and rubydex overrides
//! it nowhere, so the members are filed on whatever lexical scope encloses the block and the
//! interface itself never enters the graph at all. `workspace/symbol "_Range"` finds nothing.
//!
//! What is left is orphaned methods with no way to tell them apart from real ones, and the scope
//! they land in is usually `Object` — every receiver's ancestor. Measured against rbs 4.1.3: 99
//! `interface` blocks across 31 files, and on an instance receiver they were most of a band of
//! 30 rows sitting between the class's own methods and `Kernel`'s. `"hi".begin`,
//! `"hi".exclude_end?`, `4.each_entry` — none of which exist.
//!
//! # Why the text is edited rather than the answers filtered
//!
//! The obvious fix is to drop these where completion builds its list. It is also incomplete: the
//! same declarations are `workspace/symbol` results and goto-definition targets, and for `rand`
//! the *first* target offered was `interface _Rand` in `core/array.rbs`. A filter would have to
//! be repeated in three modules and remembered in a fourth.
//!
//! Removing the text is one rule in one place — **ya-lsp does not index RBS interfaces** — and
//! costs nothing per request. Every byte of the block is replaced with a space except its
//! newlines, so every offset and every line number in the rest of the file is exactly what it was
//! and everything else in it still resolves, hovers and navigates.
//!
//! Nothing is lost by it. RBS reaches an interface's methods through `include _Foo`, and rbs's own
//! core and stdlib contain no such include — nor could rubydex resolve one, since it has no
//! declaration to resolve it to.

use ruby_rbs::node::{InterfaceNode, Node, Visit, parse};

/// `source` with every `interface … end` blanked out, or `None` when there is nothing to do.
///
/// `None` rather than an unchanged copy so the caller can keep the parallel path: most signature
/// files declare no interface, and those should reach rubydex as a plain path to read on a worker
/// thread.
#[must_use]
pub fn without_interfaces(source: &str) -> Option<String> {
    // The parser costs more than a substring scan, and the scan says no for four files in five.
    // It cannot say a false no: `interface` is the keyword, so a block cannot exist without it.
    if !source.contains("interface") {
        return None;
    }
    // A signature rbs itself cannot parse is one rubydex will not index either. Leaving it alone
    // is what the caller does with every other file it cannot improve.
    let signature = parse(source).ok()?;

    let mut spans = Spans(Vec::new());
    spans.visit(&signature.as_node());
    if spans.0.is_empty() {
        return None;
    }
    let edited = blank(source, &spans.0)?;

    // The guard, and it is not paranoia — the first version of this shipped a `core/array.rbs`
    // that rbs refused with "cannot start a declaration", because a block's `%a{…}` annotations
    // sit *outside* the span its node reports and were left with nothing to annotate. rubydex
    // then indexed none of the file and `[].` offered seven methods instead of a hundred and
    // fifty, silently, through a green suite.
    //
    // Editing a file that a parser has to read afterwards is only safe if the parser agrees, so
    // it is asked. A file this cannot improve keeps its interfaces, which is where it started.
    if parse(&edited).is_err() {
        return None;
    }
    Some(edited)
}

/// Where each `interface … end` begins and ends, in bytes.
struct Spans(Vec<(usize, usize)>);

impl Visit for Spans {
    fn visit_interface_node(&mut self, node: &InterfaceNode) {
        let location = node.location();
        let (Ok(mut start), Ok(end)) = (
            usize::try_from(location.start()),
            usize::try_from(location.end()),
        ) else {
            return;
        };

        // An annotation is written before the keyword and is *not* inside the node's own span,
        // so taking the span alone leaves `%a{deprecated: …}` attached to nothing and the file
        // stops parsing. The comment above a block needs no such care: `#` lines are legal
        // anywhere.
        for annotation in node.annotations().iter() {
            if let Node::Annotation(annotation) = annotation
                && let Ok(begins) = usize::try_from(annotation.location().start())
            {
                start = start.min(begins);
            }
        }

        self.0.push((start, end));
        // Deliberately not recursing. Nothing inside is wanted, and RBS does not allow a class or
        // a module in there for the walk to have missed.
    }
}

/// Replace every byte of each span with a space, keeping the newlines.
///
/// Bytes rather than characters: a multi-byte character inside a span becomes that many spaces,
/// so the length is identical and every later offset in the file still lands where it did. Keeping
/// the newlines is what holds the line numbers.
fn blank(source: &str, spans: &[(usize, usize)]) -> Option<String> {
    let mut bytes = source.as_bytes().to_vec();
    for (start, end) in spans {
        let slice = bytes.get_mut(*start..*end)?;
        for byte in slice {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    // Whole characters were replaced by ASCII, so this holds; it is checked rather than asserted
    // because a wrong answer here would be a corrupted signature file rather than a panic.
    String::from_utf8(bytes).ok()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    const NESTED: &str = "\
class Array
  interface _Rand
    def rand: (Integer max) -> Integer
  end

  def sample: () -> Integer
end

interface _Reader
  def read: () -> String
end
";

    #[test]
    fn a_file_without_an_interface_is_left_alone() {
        assert_eq!(
            without_interfaces("class Foo\n  def bar: () -> void\nend\n"),
            None
        );
    }

    #[test]
    fn both_a_nested_and_a_top_level_interface_go() {
        // Both shapes appear in one real file: `core/array.rbs` declares `_Rand` inside
        // `class Array` and again at the top level, so the members land on `Array` and on
        // `Object` respectively. A rule that only looked at the top level would leave
        // `Array#rand` behind.
        let stripped = without_interfaces(NESTED).expect("two interfaces");
        assert!(!stripped.contains("def rand"), "{stripped}");
        assert!(!stripped.contains("def read"), "{stripped}");
        assert!(stripped.contains("def sample"), "{stripped}");
        assert!(stripped.contains("class Array"), "{stripped}");
    }

    #[test]
    fn every_offset_and_line_survives() {
        // The whole point: the file is edited in place, so what is left has to sit exactly where
        // it sat. Anything else moves the definitions rubydex records for the rest of the file.
        let stripped = without_interfaces(NESTED).expect("two interfaces");
        assert_eq!(stripped.len(), NESTED.len());
        assert_eq!(stripped.lines().count(), NESTED.lines().count());
        assert_eq!(
            stripped.find("def sample"),
            NESTED.find("def sample"),
            "{stripped}"
        );
    }

    #[test]
    fn a_multi_byte_character_inside_one_keeps_the_length() {
        let source = "interface _Foo\n  # åäö\n  def x: () -> void\nend\n\nclass Bar\nend\n";
        let stripped = without_interfaces(source).expect("an interface");
        assert_eq!(stripped.len(), source.len());
        assert_eq!(stripped.find("class Bar"), source.find("class Bar"));
    }

    #[test]
    fn a_signature_that_does_not_parse_is_left_alone() {
        assert_eq!(without_interfaces("interface _Foo\n  def"), None);
    }

    #[test]
    fn an_annotation_goes_with_the_block_it_annotates() {
        // `core/array.rbs` writes exactly this, and the first version of this module left the
        // `%a{…}` behind: rbs then refused the whole file with "cannot start a declaration" and
        // rubydex indexed none of `Array`.
        let source = "\
%a{deprecated: Use Array::_Rand, or make your own}
interface _Rand
  def rand: (Integer max) -> Integer
end

class Keeper
  def kept: () -> void
end
";
        let stripped = without_interfaces(source).expect("an interface");
        assert!(!stripped.contains("deprecated"), "{stripped}");
        assert!(stripped.contains("def kept"), "{stripped}");
        assert_eq!(stripped.len(), source.len());
    }

    #[test]
    fn what_comes_out_is_something_rbs_still_reads() {
        // The property the whole approach rests on, asserted directly rather than inferred from
        // the two tests above.
        let stripped = without_interfaces(NESTED).expect("two interfaces");
        assert!(ruby_rbs::node::parse(&stripped).is_ok(), "{stripped}");
    }
}
