//! LSP position <-> byte offset conversion.
//!
//! # Why this is ours and not rubydex's
//!
//! `rubydex::offset::Offset::to_location` always returns UTF-8 columns.
//! `rubydex::model::encoding::Encoding::to_wide()` is defined but is never called anywhere
//! in the crate, so `Graph::set_encoding` has no effect on the numbers that come back out.
//! Editors negotiate UTF-16 by default, so delegating would misplace every span on a line
//! containing an emoji, an accent, or CJK text.
//!
//! We use `line_index` only for line boundaries (its scanner is the SIMD one from rustc) and
//! do the column arithmetic here, because `LineIndex::offset` does not clamp: a column past
//! the end of a line silently returns an offset inside the *next* line.

use line_index::{LineIndex, TextRange, TextSize};
use lsp_types::{Position, PositionEncodingKind, Range};

/// The position encoding negotiated with the client at `initialize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PositionEncoding {
    /// What LSP mandates when the client advertises nothing.
    #[default]
    Utf16,
    /// Preferred: rubydex offsets are already UTF-8 byte offsets, so the column is the identity.
    Utf8,
    Utf32,
}

impl PositionEncoding {
    /// Best first. UTF-8 leads because rubydex speaks byte offsets natively.
    const PREFERENCE: [PositionEncoding; 3] = [Self::Utf8, Self::Utf32, Self::Utf16];

    /// Pick an encoding from what the client advertises in
    /// `capabilities.general.positionEncodings`.
    ///
    /// A client that omits the field predates the capability and must be served UTF-16.
    #[must_use]
    pub fn negotiate(offered: Option<&[PositionEncodingKind]>) -> Self {
        let Some(offered) = offered else {
            return Self::Utf16;
        };

        Self::PREFERENCE
            .into_iter()
            .find(|candidate| offered.contains(&candidate.to_lsp()))
            .unwrap_or(Self::Utf16)
    }

    #[must_use]
    pub fn to_lsp(self) -> PositionEncodingKind {
        match self {
            Self::Utf8 => PositionEncodingKind::UTF8,
            Self::Utf16 => PositionEncodingKind::UTF16,
            Self::Utf32 => PositionEncodingKind::UTF32,
        }
    }

    /// How many code units this character occupies in this encoding.
    fn units_of(self, ch: char) -> u32 {
        match self {
            Self::Utf8 => ch.len_utf8() as u32,
            Self::Utf16 => ch.len_utf16() as u32,
            Self::Utf32 => 1,
        }
    }

    /// How many code units this string occupies in this encoding.
    fn measure(self, text: &str) -> u32 {
        match self {
            Self::Utf8 => text.len() as u32,
            Self::Utf16 => text.encode_utf16().count() as u32,
            Self::Utf32 => text.chars().count() as u32,
        }
    }
}

/// An open buffer: source text plus its line index, kept in sync by construction.
///
/// Note that rubydex's `Document` exposes a `line_index()` but *not* the source text, so we
/// keep our own copy regardless. It is also what incremental sync (M2) and cursor context
/// (M5) will need.
#[derive(Debug)]
pub struct TextDocument {
    text: String,
    index: LineIndex,
    encoding: PositionEncoding,
}

impl TextDocument {
    #[must_use]
    pub fn new(text: String, encoding: PositionEncoding) -> Self {
        let index = LineIndex::new(&text);
        Self {
            text,
            index,
            encoding,
        }
    }

    /// Replace the whole buffer (full text sync).
    pub fn set_text(&mut self, text: String) {
        self.index = LineIndex::new(&text);
        self.text = text;
    }

    /// Apply one `textDocument/didChange` content change.
    ///
    /// A `None` range means the client sent the whole buffer. Otherwise the range is in the
    /// document *as it stands now*, which is why a batch of changes has to be applied one at a
    /// time, in the order the client sent them.
    ///
    /// The line index is rebuilt on every change rather than patched. That is O(file) per
    /// keystroke, but rubydex reparses the whole buffer immediately afterwards, so it is not
    /// remotely the bottleneck — and an incrementally maintained index is a well-known source
    /// of silent off-by-one corruption.
    pub fn apply(&mut self, range: Option<Range>, replacement: &str) {
        let Some(range) = range else {
            self.set_text(replacement.to_owned());
            return;
        };

        let start = self.offset_at(range.start);
        // An inverted range is malformed, but it must not panic: `replace_range` would.
        let end = self.offset_at(range.end).max(start);
        self.text
            .replace_range(start as usize..end as usize, replacement);
        self.index = LineIndex::new(&self.text);
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn encoding(&self) -> PositionEncoding {
        self.encoding
    }

    /// Convert an LSP position to a byte offset.
    ///
    /// Out-of-range input is clamped rather than rejected: the LSP spec tells clients to clamp,
    /// but a rejected position here would mean a dropped request, and a panic would take the
    /// server down. A position that splits a multi-byte character resolves to that character's
    /// first byte.
    #[must_use]
    pub fn offset_at(&self, position: Position) -> u32 {
        let Some(line_range) = self.index.line(position.line) else {
            // Line past the end of the buffer.
            return self.len();
        };

        let line_start = u32::from(line_range.start());
        let content = self.line_content(line_range);

        let mut units = 0u32;
        for (byte_offset, ch) in content.char_indices() {
            if units + self.encoding.units_of(ch) > position.character {
                return line_start + byte_offset as u32;
            }
            units += self.encoding.units_of(ch);
        }

        // Column past the end of the line: clamp to the last character, never past the newline.
        line_start + content.len() as u32
    }

    /// Convert a byte offset to an LSP position.
    ///
    /// An offset strictly inside a `\r\n` has no LSP position of its own — a position
    /// addresses a character of a line, and the terminator is not one. Such an offset resolves
    /// to the end of the line it terminates, so this is not a total inverse of
    /// [`Self::offset_at`]; it is idempotent through it, which is the property that matters.
    #[must_use]
    pub fn position_at(&self, offset: u32) -> Position {
        let offset = self.clamp_to_char_boundary(offset);
        // Safe: `clamp_to_char_boundary` guarantees in-range and on a boundary, the two
        // conditions under which `try_line_col` returns `None`.
        let line_col = self.index.line_col(TextSize::from(offset));

        let line_start = offset - line_col.col;
        let content_len = self
            .index
            .line(line_col.line)
            .map_or(line_col.col, |range| self.line_content(range).len() as u32);
        let column = line_col.col.min(content_len);
        // Both bounds sit on character boundaries: `column` came from `line_col`, and a line's
        // content ends just before `\r` or `\n`, which are ASCII.
        let prefix = &self.text[line_start as usize..(line_start + column) as usize];

        Position {
            line: line_col.line,
            character: self.encoding.measure(prefix),
        }
    }

    /// Convert a byte span (as rubydex reports them) to an LSP range.
    #[must_use]
    pub fn range_at(&self, start: u32, end: u32) -> Range {
        Range {
            start: self.position_at(start),
            end: self.position_at(end.max(start)),
        }
    }

    #[must_use]
    pub fn len(&self) -> u32 {
        self.text.len() as u32
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// A line's text without its terminator, so a column can never address the newline itself.
    fn line_content(&self, range: TextRange) -> &str {
        let line = &self.text[usize::from(range.start())..usize::from(range.end())];
        match line.strip_suffix('\n') {
            Some(stripped) => stripped.strip_suffix('\r').unwrap_or(stripped),
            None => line,
        }
    }

    fn clamp_to_char_boundary(&self, offset: u32) -> u32 {
        let mut offset = (offset as usize).min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [PositionEncoding; 3] = [
        PositionEncoding::Utf8,
        PositionEncoding::Utf16,
        PositionEncoding::Utf32,
    ];

    /// Text chosen so each encoding disagrees with the others:
    /// ASCII (1/1/1), accent (2/1/1), CJK (3/1/1), emoji (4/2/1), combining mark (2/1/1).
    const TRICKY: &[&str] = &[
        "",
        "\n",
        "plain ascii\n",
        "caf\u{e9} au lait\n",
        "e\u{301}gal",                       // combining acute accent
        "\u{65e5}\u{672c}\u{8a9e} = ruby\n", // CJK
        "x = \u{1f600}\u{1f680}\ny = 1\n",   // emoji (surrogate pairs in UTF-16)
        "a\r\nb\r\n",                        // CRLF
        "no trailing newline",
        "\u{1f600}", // emoji only, no newline
        "class Caf\u{e9}\n  def \u{1f600}!\n  end\nend\n",
    ];

    /// The `\n` of a `\r\n` is the one byte offset an LSP position cannot name.
    fn inside_crlf(text: &str, offset: usize) -> bool {
        offset > 0 && text.as_bytes()[offset - 1] == b'\r' && text.as_bytes()[offset] == b'\n'
    }

    #[test]
    fn round_trips_every_addressable_char_boundary() {
        for text in TRICKY {
            for encoding in ALL {
                let doc = TextDocument::new((*text).to_owned(), encoding);
                for offset in 0..=text.len() {
                    if !text.is_char_boundary(offset) || inside_crlf(text, offset) {
                        continue;
                    }
                    let offset = offset as u32;
                    let position = doc.position_at(offset);
                    assert_eq!(
                        doc.offset_at(position),
                        offset,
                        "round trip failed at {offset} in {text:?} ({encoding:?}); \
                         position was {position:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn position_at_is_idempotent_through_offset_at_everywhere() {
        // Weaker than a round trip, but total: it must hold even for the offsets that have no
        // exact position, and it is what guarantees a span never drifts when it makes the trip
        // twice.
        for text in TRICKY {
            for encoding in ALL {
                let doc = TextDocument::new((*text).to_owned(), encoding);
                for offset in 0..=text.len() {
                    if !text.is_char_boundary(offset) {
                        continue;
                    }
                    let once = doc.position_at(offset as u32);
                    let twice = doc.position_at(doc.offset_at(once));
                    assert_eq!(once, twice, "at {offset} in {text:?} ({encoding:?})");
                }
            }
        }
    }

    #[test]
    fn offset_inside_a_crlf_reports_the_end_of_the_line_it_terminates() {
        // "a\r\nb": offset 2 is the `\n`, which is not a character of any line.
        let doc = TextDocument::new("a\r\nb".to_owned(), PositionEncoding::Utf16);
        assert_eq!(
            doc.position_at(2),
            Position {
                line: 0,
                character: 1
            }
        );
        // The start of the next line remains exactly addressable.
        assert_eq!(
            doc.position_at(3),
            Position {
                line: 1,
                character: 0
            }
        );
    }

    #[test]
    fn columns_are_measured_in_the_negotiated_units() {
        let text = "\u{1f600}b\n".to_owned(); // emoji then 'b'
        let after_emoji = 4; // byte offset of 'b'

        assert_eq!(
            TextDocument::new(text.clone(), PositionEncoding::Utf8)
                .position_at(after_emoji)
                .character,
            4
        );
        assert_eq!(
            TextDocument::new(text.clone(), PositionEncoding::Utf16)
                .position_at(after_emoji)
                .character,
            2 // surrogate pair
        );
        assert_eq!(
            TextDocument::new(text, PositionEncoding::Utf32)
                .position_at(after_emoji)
                .character,
            1
        );
    }

    #[test]
    fn column_past_end_of_line_clamps_to_line_end_not_next_line() {
        // This is the `LineIndex::offset` trap: it would happily return an offset on line 1.
        let doc = TextDocument::new("ab\ncd\n".to_owned(), PositionEncoding::Utf16);
        let offset = doc.offset_at(Position {
            line: 0,
            character: 99,
        });
        assert_eq!(offset, 2, "should stop before the newline");
    }

    #[test]
    fn line_past_end_of_buffer_clamps_to_end() {
        let doc = TextDocument::new("ab\n".to_owned(), PositionEncoding::Utf16);
        assert_eq!(
            doc.offset_at(Position {
                line: 99,
                character: 0
            }),
            3
        );
    }

    #[test]
    fn crlf_column_stops_before_the_carriage_return() {
        let doc = TextDocument::new("ab\r\ncd\r\n".to_owned(), PositionEncoding::Utf16);
        assert_eq!(
            doc.offset_at(Position {
                line: 0,
                character: 50
            }),
            2
        );
        // Line 1 starts after "\r\n".
        assert_eq!(
            doc.offset_at(Position {
                line: 1,
                character: 0
            }),
            4
        );
    }

    #[test]
    fn position_inside_a_multi_byte_char_snaps_to_its_start() {
        let doc = TextDocument::new("\u{1f600}x".to_owned(), PositionEncoding::Utf16);
        // Column 1 is the low surrogate: inside the emoji.
        assert_eq!(
            doc.offset_at(Position {
                line: 0,
                character: 1
            }),
            0
        );
        // A byte offset in the middle of the emoji snaps back to its first byte.
        assert_eq!(doc.position_at(2).character, 0);
    }

    #[test]
    fn negotiation_prefers_utf8_and_defaults_to_utf16() {
        assert_eq!(PositionEncoding::negotiate(None), PositionEncoding::Utf16);
        assert_eq!(
            PositionEncoding::negotiate(Some(&[])),
            PositionEncoding::Utf16
        );
        assert_eq!(
            PositionEncoding::negotiate(Some(&[
                PositionEncodingKind::UTF16,
                PositionEncodingKind::UTF8
            ])),
            PositionEncoding::Utf8
        );
        assert_eq!(
            PositionEncoding::negotiate(Some(&[PositionEncodingKind::UTF16])),
            PositionEncoding::Utf16
        );
        assert_eq!(
            PositionEncoding::negotiate(Some(&[PositionEncodingKind::UTF32])),
            PositionEncoding::Utf32
        );
    }

    fn position(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn range(start: (u32, u32), end: (u32, u32)) -> Range {
        Range {
            start: position(start.0, start.1),
            end: position(end.0, end.1),
        }
    }

    #[test]
    fn an_incremental_change_edits_only_its_range() {
        let mut doc = TextDocument::new(
            "class Foo\n  def bar\n  end\nend\n".to_owned(),
            PositionEncoding::Utf16,
        );
        doc.apply(Some(range((1, 6), (1, 9))), "baz");
        assert_eq!(doc.text(), "class Foo\n  def baz\n  end\nend\n");
    }

    #[test]
    fn a_change_with_no_range_replaces_the_buffer() {
        let mut doc = TextDocument::new("old".to_owned(), PositionEncoding::Utf8);
        doc.apply(None, "brand new");
        assert_eq!(doc.text(), "brand new");
        assert_eq!(doc.position_at(9), position(0, 9));
    }

    #[test]
    fn changes_apply_one_after_another_against_the_updated_text() {
        // The LSP contract: each range is expressed in the document the previous change left
        // behind. Applying them against the original text would corrupt every edit after the
        // first that changes a line's length.
        let mut doc = TextDocument::new("a\nb\n".to_owned(), PositionEncoding::Utf16);
        doc.apply(Some(range((0, 0), (0, 1))), "hello");
        doc.apply(Some(range((0, 5), (0, 5))), " world");
        assert_eq!(doc.text(), "hello world\nb\n");
    }

    #[test]
    fn an_edit_after_an_emoji_lands_where_the_client_meant() {
        // The whole reason `apply` goes through `offset_at`: in UTF-16 the emoji is two code
        // units and four bytes, so a naive byte-for-column edit would splice mid-character.
        for encoding in ALL {
            let mut doc = TextDocument::new("x = \u{1f600}\u{1f680} + 1\n".to_owned(), encoding);
            let column = |ch: &str| {
                doc.position_at(doc.text().find(ch).unwrap() as u32)
                    .character
            };
            let plus = column("+");
            doc.apply(Some(range((0, plus), (0, plus + 1))), "-");
            assert_eq!(doc.text(), "x = \u{1f600}\u{1f680} - 1\n", "{encoding:?}");
        }
    }

    #[test]
    fn deleting_a_whole_line_keeps_the_line_index_consistent() {
        let mut doc = TextDocument::new("one\ntwo\nthree\n".to_owned(), PositionEncoding::Utf8);
        doc.apply(Some(range((1, 0), (2, 0))), "");
        assert_eq!(doc.text(), "one\nthree\n");
        // Stale line boundaries would put this on the wrong line.
        assert_eq!(doc.offset_at(position(1, 0)), 4);
    }

    #[test]
    fn a_malformed_range_is_survivable() {
        // Inverted, and past the end of the buffer. `replace_range` panics on either; a panic
        // in the analysis thread takes down the editor's connection.
        let mut doc = TextDocument::new("abc".to_owned(), PositionEncoding::Utf8);
        doc.apply(Some(range((0, 3), (0, 0))), "!");
        assert_eq!(doc.text(), "abc!");
        doc.apply(Some(range((9, 9), (9, 9))), "?");
        assert_eq!(doc.text(), "abc!?");
    }
}
