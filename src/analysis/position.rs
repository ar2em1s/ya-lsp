//! LSP position <-> byte offset conversion.
//!
//! # Why this is ours and not rubydex's
//!
//! `rubydex::offset::Offset::to_location` always returns UTF-8 columns.
//! `rubydex::model::encoding::Encoding::to_wide()` exists but nothing in rubydex calls it, so
//! `Graph::set_encoding` does not change the numbers that come out. Editors negotiate UTF-16 by
//! default, so delegating would misplace every span on a line with an emoji, an accent or CJK text.
//!
//! `line_index` is used only for line boundaries (its scanner is rustc's SIMD one). The column
//! arithmetic is done here, because `LineIndex::offset` does not clamp: a column past the end of a
//! line silently returns an offset inside the *next* line.

use line_index::{LineCol, LineIndex, TextRange, TextSize, WideEncoding};
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

/// A byte span as an LSP range, counted from a line index alone.
///
/// [`TextDocument::range_at`] needs the text, because it measures a line's prefix in code units. A
/// [`LineIndex`] records where each line's non-ASCII characters are *when it is built*, so the same
/// arithmetic works from the index alone. **rubydex has already built one for every document in the
/// graph**, so a span in a document nobody has open is placed without opening, reading or scanning
/// the file. Reading those files was a measurable share of the analysis thread's time on wide
/// answers, and remembering files already read per request would have saved little: the cost is how
/// many *distinct* files an answer reaches.
///
/// `None` for an offset the index cannot place: past the end of the text, or inside a multi-byte
/// character. A caller that gets `None` reads the text, as it would have anyway.
///
/// **Its coordinates are the indexed text's**, so do not use it for a document whose text differs
/// from the client's: a buffer being typed into ([`Rebase`]), or a template, which is *read* as the
/// blanked view and *addressed* as the markup ([`TextDocument::blanked`]).
///
/// One offset comes out differently than from [`TextDocument::position_at`]: an offset strictly
/// between a `\r` and its `\n` resolves to the column *after* the line's last character, because an
/// index does not remember how a line ends. Both are legal LSP positions, and no span this is asked
/// about can reach that offset: Prism ends no node inside a line terminator.
#[must_use]
pub fn range_in(
    index: &LineIndex,
    encoding: PositionEncoding,
    start: u32,
    end: u32,
) -> Option<Range> {
    Some(Range {
        start: position_in(index, encoding, start)?,
        end: position_in(index, encoding, end.max(start))?,
    })
}

/// One byte offset as an LSP position, counted in a line index alone. See [`range_in`].
#[must_use]
pub fn position_in(index: &LineIndex, encoding: PositionEncoding, offset: u32) -> Option<Position> {
    let line_col = index.try_line_col(TextSize::from(offset))?;
    let character = match encoding {
        // rubydex speaks byte offsets and a UTF-8 column is one, so there is nothing to convert.
        PositionEncoding::Utf8 => line_col.col,
        PositionEncoding::Utf16 => wide_column(index, WideEncoding::Utf16, line_col),
        PositionEncoding::Utf32 => wide_column(index, WideEncoding::Utf32, line_col),
    };
    Some(Position {
        line: line_col.line,
        character,
    })
}

/// A UTF-8 column re-counted in `encoding`'s code units, from what the index remembers.
///
/// `LineIndex::to_wide` answers `None` only when a `checked_sub` over the wide characters *before*
/// the column underflows, which a column this index produced cannot do. So the fallback is the
/// UTF-8 column: right for every line without a wide character, and the only answer available if
/// the index ever disagreed about one.
fn wide_column(index: &LineIndex, encoding: WideEncoding, line_col: LineCol) -> u32 {
    index
        .to_wide(encoding, line_col)
        .map_or(line_col.col, |wide| wide.col)
}

/// An open buffer: source text plus its line index, kept in sync by construction.
///
/// rubydex's `Document` exposes a `line_index()` but *not* the source text, so this keeps its own
/// copy. Incremental sync and cursor context need it anyway.
///
/// # Two texts, one set of offsets
///
/// A document is *read* and *addressed*, and for every file but one those are the same string. A
/// template is not: it is read as the blanked Ruby view [`erb::ruby_view`](super::erb::ruby_view)
/// makes of it, and addressed as the markup the editor has open. Byte offsets are shared, because
/// the view keeps the length and every line break (which is why ERB needs no position map). Columns
/// are not: a column counts code units, and blanking a 3-byte `“` writes three spaces where the
/// client counts one UTF-16 unit. So [`Self::text`] is what is read, and [`Self::coordinates`] is
/// what columns are counted in.
#[derive(Debug)]
pub struct TextDocument {
    text: String,
    /// The text the client's columns are counted in, when that is not [`Self::text`] itself.
    ///
    /// Only [`Self::blanked`] sets it, and only a template goes through that.
    source: Option<String>,
    index: LineIndex,
    encoding: PositionEncoding,
}

impl TextDocument {
    #[must_use]
    pub fn new(text: String, encoding: PositionEncoding) -> Self {
        let index = LineIndex::new(&text);
        Self {
            text,
            source: None,
            index,
            encoding,
        }
    }

    /// A document that is *read* as `view` and *addressed* as `source`.
    ///
    /// The two are the same length, byte for byte, with line breaks in the same places. That is
    /// [`erb::ruby_view`](super::erb::ruby_view)'s central property, held by a `proptest` there,
    /// and the only reason one line index and one byte offset serve both. What differs is how many
    /// code units a prefix is: counting columns against the view would shift the cursor left by
    /// (bytes − units) of every non-ASCII character before it in the markup, and shift every
    /// returned span right by the same amount.
    #[must_use]
    pub fn blanked(source: String, view: String, encoding: PositionEncoding) -> Self {
        let index = LineIndex::new(&source);
        Self {
            text: view,
            source: Some(source),
            index,
            encoding,
        }
    }

    /// Replace the whole buffer (full text sync).
    pub fn set_text(&mut self, text: String) {
        self.index = LineIndex::new(&text);
        self.text = text;
        self.source = None;
    }

    /// Apply one `textDocument/didChange` content change.
    ///
    /// A `None` range means the client sent the whole buffer. Otherwise the range is in the
    /// document *as it stands now*, which is why a batch of changes must be applied one at a time,
    /// in order.
    ///
    /// The line index is rebuilt on every change, not patched. That is O(file) per keystroke, but
    /// rubydex reparses the whole buffer right after, so it is nowhere near the bottleneck, and an
    /// incrementally maintained index is a classic source of silent off-by-one corruption.
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
        // An edited document addresses itself. The buffer an editor edits is never a blanked view
        // (`with_text` builds those per request and hands out a shared reference), but a `source`
        // that outlived an edit would count columns in text this document no longer holds.
        self.source = None;
    }

    #[must_use]
    pub fn len(&self) -> u32 {
        self.text.len() as u32
    }

    /// Kept beside `len` because a public `len` without one is a lint and a bad API alike.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Convert an LSP position to a byte offset.
    ///
    /// Out-of-range input is clamped, not rejected: the LSP spec tells clients to clamp, a rejected
    /// position would drop the request, and a panic would take the server down. A position that
    /// splits a multi-byte character resolves to that character's first byte.
    #[must_use]
    pub fn offset_at(&self, position: Position) -> u32 {
        let Some(line_range) = self.index.line(position.line) else {
            // Line past the end of the buffer.
            return self.coordinates().len() as u32;
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
    /// An offset strictly inside a `\r\n` has no LSP position of its own: a position addresses a
    /// character of a line, and the terminator is not one. It resolves to the end of the line it
    /// terminates, so this is not a total inverse of [`Self::offset_at`]; it is idempotent through
    /// it, which is the property that matters.
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
        let prefix = &self.coordinates()[line_start as usize..(line_start + column) as usize];

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

    /// The text a position's line and column are counted in: the document itself, or, for a
    /// template, the markup the editor has rather than the view.
    fn coordinates(&self) -> &str {
        self.source.as_deref().unwrap_or(&self.text)
    }

    /// A line's text without its terminator, so a column can never address the newline itself.
    fn line_content(&self, range: TextRange) -> &str {
        let line = &self.coordinates()[usize::from(range.start())..usize::from(range.end())];
        match line.strip_suffix('\n') {
            Some(stripped) => stripped.strip_suffix('\r').unwrap_or(stripped),
            None => line,
        }
    }

    fn clamp_to_char_boundary(&self, offset: u32) -> u32 {
        let mut offset = (offset as usize).min(self.coordinates().len());
        while !self.coordinates().is_char_boundary(offset) {
            offset -= 1;
        }
        offset as u32
    }
}

/// How the buffer's byte offsets map onto the ones the graph was indexed with.
///
/// **This is what makes deferring the index workable.** rubydex's offsets index the text the
/// indexer was last given, but `cursor::at` reads the *buffer*. While nothing has been typed since
/// the last settle, they are the same string. Between a keystroke and the settle that indexes it
/// they are not, and a buffer offset used as a graph key names different text: a **wrong** answer,
/// not a missing one.
///
/// It is [`TextDocument::blanked`]'s move on a second axis: there a document is read as one text
/// and addressed as another because blanking replaced markup; here, because time passed. The shared
/// coordinate is the byte offset, and the translation is a function of the two texts, not of a
/// maintained edit log, which is the version that can drift.
///
/// The map is a common byte prefix plus a common byte suffix, so the buffer's
/// `[prefix, buffer_len - suffix)` and the graph's `[prefix, graph_len - suffix)` are the regions
/// that differ. Outside them the translation is exact. **Inside, there is no answer and the caller
/// must refuse.** A scattered edit only widens the refused region, so this degrades safely instead
/// of approximating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rebase {
    prefix: u32,
    suffix: u32,
    buffer_len: u32,
    graph_len: u32,
}

impl Rebase {
    /// The map for a document the graph holds exactly as the buffer has it.
    #[must_use]
    pub fn identity(len: u32) -> Self {
        Self {
            prefix: len,
            suffix: 0,
            buffer_len: len,
            graph_len: len,
        }
    }

    /// Two scans, both early-exit. Boundaries are backed off to char boundaries, so a translated
    /// offset never splits a character (the same rule `clamp_to_char_boundary` keeps).
    #[must_use]
    pub fn between(buffer: &str, indexed: &str) -> Self {
        /// Whether a byte can be part of one of the words this map has to keep whole.
        ///
        /// Wider than Ruby's identifier on purpose: `:` keeps a constant *path* together, so
        /// `XY::Person` and `HR::Person` do not share `::Person` as an unchanged suffix. Every
        /// non-ASCII byte counts, because an identifier may hold one and a wrong answer costs more
        /// here than a refusal.
        fn is_word_byte(byte: u8) -> bool {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b':' | b'@' | b'$' | b'?' | b'!')
                || byte >= 0x80
        }

        let (b, g) = (buffer.as_bytes(), indexed.as_bytes());
        let max = b.len().min(g.len());
        let mut prefix = 0;
        while prefix < max && b[prefix] == g[prefix] {
            prefix += 1;
        }
        // **Byte identity is not token identity, and every consumer of this map needs the second.**
        // `Alpha.` rewritten to `Gamma.` shares its last two bytes (`a.`), so a purely byte-wise
        // scan leaves that `.` in the common suffix and hands back an offset the graph files
        // `Alpha` under: the wrong class, not no class. `Receiver::Constant` holds the byte after a
        // constant path and `Instance` the byte after the constant it was built from, so an offset
        // stands for a whole word and survives only if that word did. So the changed region is
        // widened outward over word bytes at both ends.
        //
        // The condition is two-sided on purpose: a head ending on `\n` before `class` ends *at* a
        // token boundary, and widening it would refuse offsets in text neither side touched. It
        // widens only where a word really straddles the seam.
        while prefix > 0
            && is_word_byte(b[prefix - 1])
            && (b.get(prefix).copied().is_some_and(is_word_byte)
                || g.get(prefix).copied().is_some_and(is_word_byte))
        {
            prefix -= 1;
        }
        // **No char-boundary back-off, because the rule above already is one.** A prefix ending
        // mid-character has a continuation byte on both sides, every byte of a multi-byte character
        // is `>= 0x80`, and `is_word_byte` counts all of those as word bytes, so the loop above
        // walks out of the character before it stops. The same holds at the tail.
        // `every_translated_offset_round_trips_and_lands_on_a_boundary` holds this property; do not
        // narrow the `0x80` arm without adding an explicit back-off.
        let mut suffix = 0;
        while suffix < max - prefix && b[b.len() - 1 - suffix] == g[g.len() - 1 - suffix] {
            suffix += 1;
        }
        while suffix > 0 {
            let (at_b, at_g) = (b.len() - suffix, g.len() - suffix);
            let straddles = is_word_byte(b[at_b])
                && (at_b
                    .checked_sub(1)
                    .is_some_and(|before| is_word_byte(b[before]))
                    || at_g
                        .checked_sub(1)
                        .is_some_and(|before| is_word_byte(g[before])));
            if !straddles {
                break;
            }
            suffix -= 1;
        }
        Self {
            prefix: prefix as u32,
            suffix: suffix as u32,
            buffer_len: b.len() as u32,
            graph_len: g.len() as u32,
        }
    }

    /// Whether the two texts are the same, in which case every caller can skip the translation.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.buffer_len == self.graph_len && self.prefix >= self.buffer_len
    }

    /// The graph offset a buffer offset names, or `None` where the text under it is text the graph
    /// has never been given.
    #[must_use]
    pub fn to_graph(&self, offset: u32) -> Option<u32> {
        self.map(offset, self.buffer_len, self.graph_len)
    }

    /// The region of the graph's text the buffer no longer agrees with: the widest a scope question
    /// may be asked over when the cursor itself cannot be translated.
    #[must_use]
    pub fn changed_in_graph(&self) -> (u32, u32) {
        (self.prefix, self.graph_len.saturating_sub(self.suffix))
    }

    /// The inverse: a span the graph handed back, in the buffer's coordinates.
    ///
    /// **Not optional: without it, answers are wrong, not just slow.** `hover` and `definition`
    /// answer with a range `locate` found in the *graph*, and a deferred buffer is not the text
    /// those offsets came from, so a jump would land on the line a declaration has moved off.
    /// `None` where the span overlaps what was just typed: it has no honest position in the new
    /// text, so the request settles and asks again instead of guessing.
    ///
    /// **A span, not two offsets, because the span resolves the ambiguity.** Text inserted at
    /// offset *p* leaves the graph's position *p* naming two buffer places, before and after the
    /// insertion, and [`map`](Self::map) refuses it for that reason. That is right for a caret and
    /// wrong for the ends of a span. A span's start is the first byte it covers and its end the
    /// last, so each leans toward the span's side and the pair comes back in order. A declaration
    /// at the top of its file, with a line being typed above it, keeps its place this way; `map`
    /// would drop it.
    #[must_use]
    pub fn span_to_buffer(self, span: ByteSpan) -> Option<ByteSpan> {
        // **Backwards is a caller's mistake, not a fact about the text.** Refusing is the one
        // answer that cannot be wrong: a refused span settles and asks again, so the cost is a slow
        // answer, not a range over text nobody asked about.
        if span.start > span.end {
            return None;
        }
        // An empty span covers no byte, so there is no side to lean on and nothing here can say
        // which of the two places it means. That is exactly `map`'s question.
        if span.start == span.end {
            let at = self.map(span.start, self.graph_len, self.buffer_len)?;
            return Some(ByteSpan { start: at, end: at });
        }
        Some(ByteSpan {
            start: self.one_sided(span.start, self.graph_len, self.buffer_len, Side::Right)?,
            end: self.one_sided(span.end, self.graph_len, self.buffer_len, Side::Left)?,
        })
    }

    /// Where a position goes when it is defined by the byte on **one** side of it.
    ///
    /// The map is a common prefix and a common suffix, so a byte survives the edit exactly while it
    /// is inside one of them: in the prefix it has not moved, in the suffix it moved by the length
    /// difference. A position is a gap between two bytes, *named* by the byte it leans on:
    /// [`Side::Right`] the one at `offset` (what a span's start covers), [`Side::Left`] the one at
    /// `offset - 1` (what its end covers). The two tests are the same pair of comparisons, one
    /// index apart.
    fn one_sided(&self, offset: u32, from_len: u32, to_len: u32, side: Side) -> Option<u32> {
        if self.is_identity() {
            return (offset <= from_len).then_some(offset);
        }
        let changed_from = from_len.saturating_sub(self.suffix);
        let shifted = || {
            let shifted = i64::from(offset) + i64::from(to_len) - i64::from(from_len);
            u32::try_from(shifted).ok()
        };
        match side {
            Side::Right if offset < self.prefix => Some(offset),
            Side::Right if offset >= changed_from => shifted(),
            Side::Left if offset <= self.prefix => Some(offset),
            Side::Left if offset > changed_from => shifted(),
            _ => None,
        }
    }

    /// The body both directions share, which is why they take their lengths as arguments.
    ///
    /// **One copy on purpose.** The rule below is subtle enough to have been wrong once (the bounds
    /// were `<=` and had to become strict), and a second copy is a second place for the next fix to
    /// miss. It is written as the *agreement* of the two one-sided maps, so the strictness cannot
    /// drift from them.
    ///
    /// **Strictly** inside the common prefix or suffix. A position names the same place in both
    /// texts only when *both* bytes around it are unchanged, which is what a caret means, and every
    /// offset this translates is a caret.
    ///
    /// With `<=`, a pure deletion slips through: `Alpha.` becoming `Alph.` leaves the buffer side
    /// of the changed region **empty**, so a boundary offset refuses nothing, and the end of the
    /// half-typed constant lands inside the span of the longer one the graph still holds. That is a
    /// precise answer about `Alpha` for a receiver spelled `Alph`, exactly the wrong answer this
    /// map exists to stop.
    ///
    /// Strictness also makes the map injective: with `<=`, the two buffer positions straddling an
    /// insertion named one graph position.
    fn map(&self, offset: u32, from_len: u32, to_len: u32) -> Option<u32> {
        let at = self.one_sided(offset, from_len, to_len, Side::Right)?;
        (self.one_sided(offset, from_len, to_len, Side::Left) == Some(at)).then_some(at)
    }
}

/// A byte range of one document, `end` exclusive.
///
/// **Named fields and no positional constructor, because the two ends are not interchangeable.**
/// [`Rebase::span_to_buffer`] leans them opposite ways (a start on the first byte covered, an end
/// on the last), so a pair written the wrong way round comes back shifted by the length of what was
/// typed and looks like an ordinary range. A caller must name both fields, and a span whose `start`
/// is past its `end` is refused, not translated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSpan {
    /// The first byte the span covers.
    pub start: u32,
    /// One past the last byte it covers, which is the position that byte's *right* edge is.
    pub end: u32,
}

/// Which of the two bytes around a position names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// The byte at `offset`: what a span's start covers, and what an insertion pushes down.
    Right,
    /// The byte at `offset - 1`: what a span's end covers, and what stays where it was.
    Left,
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use crate::analysis::indexer;
    use crate::analysis::testing::*;
    use proptest::prelude::*;

    use super::*;

    /// A span for the assertions here, and **only** here.
    ///
    /// `ByteSpan`'s fields are named so production code cannot write one backwards by accident. A
    /// test writing twenty of them longhand would bury what it asserts, and a swap inside an
    /// assertion is visible. `a_span_handed_over_backwards_is_refused_rather_than_translated` holds
    /// the guarantee itself.
    fn span(start: u32, end: u32) -> ByteSpan {
        ByteSpan { start, end }
    }

    #[test]
    fn a_span_handed_over_backwards_is_refused_rather_than_translated() {
        // The one mistake the type cannot prevent, and why it fails closed: a start and an end lean
        // opposite ways, so a swapped pair comes back shifted by the length of what was typed, an
        // ordinary-looking range over text nobody asked about. A refusal settles and asks again
        // instead.
        let rebase = Rebase::between("\nmodule Foo\nend\n", "module Foo\nend\n");
        assert_eq!(rebase.span_to_buffer(span(0, 15)), Some(span(1, 16)));
        assert_eq!(rebase.span_to_buffer(span(15, 0)), None);
    }

    #[test]
    fn an_unedited_document_maps_every_offset_to_itself() {
        let text = "class Story\n  def title\n  end\nend\n";
        let rebase = Rebase::between(text, text);
        assert!(rebase.is_identity());
        for offset in 0..=text.len() as u32 {
            assert_eq!(rebase.to_graph(offset), Some(offset));
            assert_eq!(
                rebase.span_to_buffer(span(0, offset)),
                Some(span(0, offset))
            );
        }
    }

    #[test]
    fn an_insertion_moves_everything_after_it_and_nothing_before_it() {
        // The shape of one keystroke: `Story.f` where the graph still holds `Story.`.
        let indexed = "x = Story.\ny = 1\n";
        let buffer = "x = Story.f\ny = 1\n";
        let rebase = Rebase::between(buffer, indexed);
        assert!(!rebase.is_identity());
        // `Story` is written before the edit, so the receiver every graph lookup needs is exact.
        assert_eq!(rebase.to_graph(4), Some(4));
        assert_eq!(rebase.to_graph(9), Some(9));
        // Everything after the insertion shifts back by its length.
        assert_eq!(rebase.to_graph(12), Some(11));
        assert_eq!(
            rebase.to_graph(buffer.len() as u32),
            Some(indexed.len() as u32)
        );
        // The two positions straddling the inserted `f` are refused instead of both being called
        // graph 10, which makes the map injective and the round trip an equality, not just an
        // idempotence.
        assert_eq!(rebase.to_graph(10), None);
        assert_eq!(rebase.to_graph(11), None);
        for offset in 0..=buffer.len() as u32 {
            if let Some(graph) = rebase.to_graph(offset) {
                // The empty span is the strict map, which is what `to_graph` answered, so this is
                // the round trip itself and not a weaker version.
                assert_eq!(
                    rebase.span_to_buffer(span(graph, graph)),
                    Some(span(offset, offset)),
                    "at {offset}"
                );
            }
        }
    }

    #[test]
    fn an_offset_inside_the_edit_is_refused_rather_than_approximated() {
        // Mid-typing a constant: the graph has never held the text under this cursor, and the map
        // must say so instead of naming a neighbour.
        let rebase = Rebase::between("x = Stor\n", "x = Widget\n");
        assert_eq!(rebase.to_graph(3), Some(3));
        // The gap where the two constants start to diverge is refused too: the byte after it is `S`
        // in one text and `W` in the other, so it is not the same place.
        assert_eq!(rebase.to_graph(4), None);
        assert_eq!(rebase.to_graph(6), None);
        assert_eq!(rebase.to_graph(7), None);
    }

    #[test]
    fn a_scattered_edit_widens_the_refused_region_rather_than_lying() {
        // Two edits far apart collapse to one region spanning both. Every offset in it is refused,
        // which is safe; nothing outside it is wrong.
        let rebase = Rebase::between("a = 2\nb = 1\nc = 4\n", "a = 1\nb = 1\nc = 3\n");
        assert_eq!(rebase.to_graph(0), Some(0));
        // Both edges of the region and everything between (including the untouched middle line) are
        // refused; only offsets with an unchanged byte on either side map.
        assert_eq!(rebase.to_graph(3), Some(3));
        assert_eq!(rebase.to_graph(4), None);
        assert_eq!(rebase.to_graph(9), None);
        assert_eq!(rebase.to_graph(17), None);
        assert_eq!(rebase.to_graph(18), Some(18));
    }

    #[test]
    fn a_word_the_graph_still_holds_widens_the_seam_even_where_the_buffer_broke_it() {
        // **Why the straddle test is two-sided.** The buffer has `-b` where the graph has `ab`. On
        // the buffer's side `b` starts a token of its own, so asking only the buffer would leave
        // `b.x` in the common suffix and return the offset the graph files `ab` under. Only the
        // graph's side knows the word was longer.
        let rebase = Rebase::between("-b.x\n", "ab.x\n");
        assert_eq!(
            rebase.to_graph(2),
            None,
            "the byte after `b` names two different words"
        );
        // The `.x` past it is untouched in both and still maps.
        assert_eq!(rebase.to_graph(3), Some(3));
    }

    #[test]
    fn the_inverse_refuses_an_offset_inside_the_edit_too() {
        // The round-trip proptest only asks the inverse about offsets the forward map accepted, so
        // it cannot reach the refusal. An inverse that answered inside the edit would put a graph
        // span onto buffer text that never held it.
        let rebase = Rebase::between("Gamma.\n", "Alpha.\n");
        let (lo, hi) = rebase.changed_in_graph();
        assert!(lo < hi, "the fixture has to leave a region to refuse");
        assert_eq!(rebase.span_to_buffer(span(lo + 1, hi)), None);
    }

    #[test]
    fn a_span_that_begins_the_document_survives_an_insertion_above_it() {
        // **The smallest case of a real defect.** A newline typed at the top of a file leaves every
        // byte the graph holds intact, so graph offset 0 (where `class` and `module` usually start)
        // must still map. Refusing it loses a file's own declaration's place on the first keystroke
        // above it. Here offset 0 is both the document's first position and the seam.
        let indexed = "module Foo\nend\n";
        let buffer = "\nmodule Foo\nend\n";
        let rebase = Rebase::between(buffer, indexed);
        assert_eq!(
            rebase.span_to_buffer(span(0, indexed.len() as u32)),
            Some(span(1, buffer.len() as u32)),
            "`module` and everything it declares moved down by exactly one byte"
        );
        // The caret at the same place is a different question and is still refused: nothing says
        // what the user meant by a position with new text on one side of it.
        assert_eq!(rebase.to_graph(0), None);
    }

    #[test]
    fn a_span_that_starts_where_the_edit_did_follows_the_text_it_covers() {
        // **The general case; the document's first position is only a corner of it.** A settle puts
        // what was typed into the graph, so the next keystroke leaves a declaration starting *at
        // the seam*, not at offset 0: an unchanged byte on each side, one that moved and one that
        // did not. `map` correctly refuses that for a caret, which names two buffer places (before
        // and after the typing). A span is not two places, so it must keep its place.
        let indexed = "\nmodule Foo\nend\n";
        let buffer = "\n\nmodule Foo\nend\n";
        let rebase = Rebase::between(buffer, indexed);
        assert_eq!(
            rebase.span_to_buffer(span(1, indexed.len() as u32)),
            Some(span(2, buffer.len() as u32))
        );
        assert_eq!(
            rebase.to_graph(1),
            None,
            "the caret is still two places at once"
        );
    }

    #[test]
    fn a_span_that_ends_the_document_survives_an_insertion_below_it() {
        // The end of a span leans the other way, and this shows it: a document's last position has
        // no byte to its right, so only the byte before it can name it. A file with no trailing
        // newline is the one whose last declaration really ends there.
        let indexed = "module Foo\nend";
        let buffer = "module Foo\nend\n\nx = 1\n";
        let rebase = Rebase::between(buffer, indexed);
        assert_eq!(
            rebase.span_to_buffer(span(0, indexed.len() as u32)),
            Some(span(0, indexed.len() as u32))
        );
        assert_eq!(rebase.to_graph(buffer.len() as u32), None);
    }

    #[test]
    fn an_edge_the_edit_itself_reached_is_refused_in_both_directions() {
        // **A lean is not a licence: the byte it leans on must still have survived.** Read
        // backwards, an insertion above the first line is a *deletion* of it, and the span the
        // graph recorded for `Alpha` then covers text that is gone; no buffer offset is where it is
        // now. The caret is refused too, for a stronger reason: answering would show a card for the
        // constant the user just deleted.
        let indexed = "Alpha\nx = 1\n";
        let buffer = "x = 1\n";
        let rebase = Rebase::between(buffer, indexed);
        assert_eq!(rebase.to_graph(0), None);
        assert_eq!(rebase.span_to_buffer(span(0, "Alpha".len() as u32)), None);
    }

    #[test]
    fn a_word_that_changed_is_never_mapped_onto_the_word_that_replaced_it() {
        // **The map is about tokens, not bytes**, and a same-length replacement tells the two
        // apart. `Alpha.` and `Gamma.` share `a.`, so a byte-wise scan puts the `.` (exactly the
        // offset `Receiver::Constant` holds) in the common suffix and maps a cursor on one constant
        // onto the other. Every offset the two words occupy, plus the `.` after them, must be
        // refused. The newline after that is common to both texts and names no constant, so it
        // maps.
        let indexed = "class Alpha\nend\nAlpha.\n";
        let buffer = "class Alpha\nend\nGamma.\n";
        let rebase = Rebase::between(buffer, indexed);
        let word = buffer.rfind("Gamma.").expect("the fixture writes it") as u32;
        for offset in word..word + "Gamma.".len() as u32 {
            assert_eq!(
                rebase.to_graph(offset),
                None,
                "offset {offset} of a word the graph has never held was mapped anyway"
            );
        }
        // And the class above it, which nothing touched, still maps.
        let untouched = buffer.find("Alpha").expect("the fixture writes it") as u32;
        assert_eq!(rebase.to_graph(untouched), Some(untouched));
    }

    #[test]
    fn a_constant_path_is_one_word_for_this_purpose() {
        // `::` is a word byte, so `HR::Person` and `XY::Person` do not share `::Person` as an
        // unchanged suffix: the graph files the reference under the whole path, and its end offset
        // means a different class in each text.
        let rebase = Rebase::between("XY::Person.\n", "HR::Person.\n");
        let end = "XY::Person".len() as u32;
        assert_eq!(rebase.to_graph(end), None);
    }

    #[test]
    fn an_edit_that_ends_on_a_token_boundary_widens_nothing() {
        // The other side of the two-sided condition. `# pad!\n` inserted before `class` ends *at* a
        // boundary, so the word after it did not change, and refusing offsets in it would cost the
        // map most of its value.
        let indexed = "class Alpha\nend\n";
        let buffer = "# pad!\nclass Alpha\nend\n";
        let rebase = Rebase::between(buffer, indexed);
        let at = buffer.find("Alpha").expect("the fixture writes it") as u32;
        assert_eq!(
            rebase.to_graph(at),
            Some(indexed.find("Alpha").unwrap() as u32)
        );
    }

    #[test]
    fn a_multibyte_edit_never_splits_a_character() {
        // The char-boundary rule again: a translated offset that is not on one would index into the
        // middle of a character and panic the moment anybody slices with it.
        let indexed = "x = \"\u{201c}\"\ny\n";
        let buffer = "x = \"\u{201c}\u{201d}\"\ny\n";
        let rebase = Rebase::between(buffer, indexed);
        for offset in 0..=buffer.len() as u32 {
            // Only offsets that address the buffer at all: an offset in the middle of a
            // character is not a cursor and no caller can produce one.
            if !buffer.is_char_boundary(offset as usize) {
                continue;
            }
            if let Some(graph) = rebase.to_graph(offset) {
                assert!(
                    indexed.is_char_boundary(graph as usize),
                    "{offset} -> {graph} splits a character"
                );
            }
        }
    }

    proptest! {
        #[test]
        fn every_translated_offset_round_trips_and_lands_on_a_boundary(
            buffer in "\\PC{0,40}", indexed in "\\PC{0,40}") {
            let rebase = Rebase::between(&buffer, &indexed);
            for offset in 0..=buffer.len() as u32 {
                if !buffer.is_char_boundary(offset as usize) {
                    continue;
                }
                if let Some(graph) = rebase.to_graph(offset) {
                    prop_assert!(graph as usize <= indexed.len());
                    prop_assert!(indexed.is_char_boundary(graph as usize));
                    prop_assert_eq!(rebase.span_to_buffer(span(graph, graph)),
                                    Some(span(offset, offset)));
                }
            }
        }

        /// Each side of a span has a domain the round trip above cannot reach (every position the
        /// strict map refuses and one lean accepts), so the three properties are stated directly on
        /// the one-sided map. Injectivity is the one with teeth: two declarations the graph found
        /// in different places must not come back at the same offset.
        #[test]
        fn every_span_endpoint_lands_in_range_on_a_boundary_and_alone(
            buffer in "\\PC{0,40}", indexed in "\\PC{0,40}") {
            let rebase = Rebase::between(&buffer, &indexed);
            for side in [Side::Left, Side::Right] {
                let mut seen: Vec<(u32, u32)> = Vec::new();
                for offset in 0..=indexed.len() as u32 {
                    if !indexed.is_char_boundary(offset as usize) {
                        continue;
                    }
                    let Some(at) =
                        rebase.one_sided(offset, rebase.graph_len, rebase.buffer_len, side)
                    else {
                        continue;
                    };
                    prop_assert!(at as usize <= buffer.len());
                    prop_assert!(buffer.is_char_boundary(at as usize));
                    prop_assert!(
                        !seen.iter().any(|(already, _)| *already == at),
                        "{side:?}: {offset} and {:?} both came back as {at}",
                        seen.iter().find(|(already, _)| *already == at).map(|(_, was)| *was)
                    );
                    seen.push((at, offset));
                }
            }
        }
    }

    #[test]
    fn length_is_bytes_and_agrees_with_emptiness() {
        // Offsets are byte offsets everywhere in this crate (`offset_at` clamps to `len`), so a
        // multi-byte character must count as its bytes, not as one character.
        let empty = TextDocument::new(String::new(), PositionEncoding::Utf16);
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());

        let text = TextDocument::new("caf\u{e9}".to_owned(), PositionEncoding::Utf16);
        assert_eq!(text.len(), 5, "four characters, five bytes");
        assert!(!text.is_empty());
    }

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
    ///
    /// The `offset < len` guard is not defensive: callers walk `0..=len`, and the end of the buffer
    /// is an ordinary offset to ask about. It was missing until a fixture ended in a bare `\r`, a
    /// hole hand-written fixtures leave and `PIECES` does not.
    fn inside_crlf(text: &str, offset: usize) -> bool {
        offset > 0
            && offset < text.len()
            && text.as_bytes()[offset - 1] == b'\r'
            && text.as_bytes()[offset] == b'\n'
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

    /// The index alone places every offset exactly where the text does.
    ///
    /// **The property [`position_in`] rests on**, and what makes it safe to answer a jump from
    /// rubydex's line index instead of the file. If the two could disagree by one column on any
    /// line, `definition` would answer with a range that is not the declaration, in a document
    /// nobody opened: the hardest kind of wrong answer to notice.
    ///
    /// The `\n` of a `\r\n` is excluded, for a sharper reason than in the round trips above: it is
    /// the one offset where the two are *known* to differ, because a line index does not remember
    /// line endings. `the_one_offset_the_index_alone_spells_differently` pins it separately.
    #[test]
    fn the_index_alone_places_every_offset_where_the_text_does() {
        for text in TRICKY {
            let index = LineIndex::new(text);
            for encoding in ALL {
                let doc = TextDocument::new((*text).to_owned(), encoding);
                for offset in 0..=text.len() {
                    if !text.is_char_boundary(offset) || inside_crlf(text, offset) {
                        continue;
                    }
                    assert_eq!(
                        position_in(&index, encoding, offset as u32),
                        Some(doc.position_at(offset as u32)),
                        "{text:?} at {offset} ({encoding:?})"
                    );
                }
            }
        }
    }

    /// What the index cannot place, it declines — and the caller reads the file.
    #[test]
    fn the_index_alone_refuses_what_it_cannot_place() {
        let text = "x = \u{1f600}\n";
        let index = LineIndex::new(text);
        assert_eq!(
            position_in(&index, PositionEncoding::Utf16, text.len() as u32 + 1),
            None,
            "past the end of the text"
        );
        assert_eq!(
            position_in(&index, PositionEncoding::Utf16, 5),
            None,
            "inside the emoji"
        );
        assert_eq!(
            range_in(&index, PositionEncoding::Utf16, 0, 5),
            None,
            "a range is no better placed than the worse of its two ends"
        );
        assert_eq!(
            range_in(&index, PositionEncoding::Utf16, 5, 9),
            None,
            "either end"
        );
    }

    /// An inverted span is empty at its start, exactly as [`TextDocument::range_at`] makes it.
    #[test]
    fn the_index_alone_makes_an_inverted_span_empty() {
        let text = "caf\u{e9} = 1\n";
        let index = LineIndex::new(text);
        let doc = TextDocument::new(text.to_owned(), PositionEncoding::Utf16);
        assert_eq!(
            range_in(&index, PositionEncoding::Utf16, 8, 3),
            Some(doc.range_at(8, 3))
        );
    }

    /// The one offset a line index cannot spell the way the text does.
    ///
    /// Stated, not worked around: the column *after* the line's last character instead of *at* it.
    /// Both are legal LSP positions on that line, every client clamps the second, and no span this
    /// is asked about can reach the offset: Prism ends no node between a `\r` and its `\n`.
    #[test]
    fn the_one_offset_the_index_alone_spells_differently() {
        let text = "caf\u{e9}\r\nx\n";
        let index = LineIndex::new(text);
        let doc = TextDocument::new(text.to_owned(), PositionEncoding::Utf16);
        let at = text.find('\n').expect("a newline") as u32;
        assert_eq!(
            doc.position_at(at),
            Position {
                line: 0,
                character: 4
            },
            "the text knows the line ends `\\r\\n` and stops at its last character"
        );
        assert_eq!(
            position_in(&index, PositionEncoding::Utf16, at),
            Some(Position {
                line: 0,
                character: 5
            }),
            "the index does not"
        );
    }

    #[test]
    fn position_at_is_idempotent_through_offset_at_everywhere() {
        // Weaker than a round trip, but total: it must hold even for offsets with no exact
        // position, and it guarantees a span never drifts when it makes the trip twice.
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

    /// A blanked document is addressed exactly as the text the client has.
    ///
    /// What [`TextDocument::blanked`] is for, stated as an equality: for a template and its Ruby
    /// view, every conversion in this module must answer what it answers for the template alone.
    /// `text()` is the *only* thing a view may change. This fails if any of the four conversions
    /// reads `self.text` instead.
    ///
    /// The view comes from `erb::ruby_view`, not a hand-written imitation, because the property
    /// belongs to that function; testing an imitation would test the imitation.
    #[test]
    fn a_blanked_document_is_addressed_as_the_text_the_client_has() {
        // A curly quote (3 bytes, 1 UTF-16 unit), an accent (2/1), an emoji (4/2) and CJK
        // (3/1), all in the markup to the *left* of the Ruby on their line.
        let template = "<h1>\u{201c}Stories\u{201d}</h1>\n\
                        <p>caf\u{e9} \u{1f680} \u{65e5}\u{672c} <%= story.title %></p>\n";
        let view = crate::analysis::erb::ruby_view(template);
        assert_eq!(
            view.len(),
            template.len(),
            "the invariant `blanked` rests on"
        );

        for encoding in ALL {
            let plain = TextDocument::new(template.to_owned(), encoding);
            let blanked = TextDocument::blanked(template.to_owned(), view.clone(), encoding);

            assert_eq!(
                blanked.text(),
                view,
                "what is read is the view ({encoding:?})"
            );
            for offset in 0..=template.len() as u32 {
                assert_eq!(
                    blanked.position_at(offset),
                    plain.position_at(offset),
                    "position_at({offset}) ({encoding:?})"
                );
                let position = plain.position_at(offset);
                assert_eq!(
                    blanked.offset_at(position),
                    plain.offset_at(position),
                    "offset_at({position:?}) ({encoding:?})"
                );
            }
        }

        // Non-vacuous, and the defect itself: the view addressed as its own text. The second line's
        // markup is seven units short of its bytes (one for the accent, two for the emoji, four for
        // the two CJK characters), so the client's caret on `story` would arrive seven bytes to its
        // left.
        let at_call = template.find("story.title").expect("the fixture") as u32;
        let position =
            TextDocument::new(template.to_owned(), PositionEncoding::Utf16).position_at(at_call);
        assert_eq!(
            TextDocument::new(view, PositionEncoding::Utf16).offset_at(position),
            at_call - 7
        );
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
        // behind. Applying them against the original text would corrupt every later edit that
        // changes a line's length.
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

    // -----------------------------------------------------------------------
    // Properties
    //
    // Everything above is a fixture. Full line and branch coverage says every line ran, not that
    // every *sequence* of edits produces the right buffer. The input is a list where each element
    // is read against the text the previous one left, so what needs enumerating is a history, not a
    // string. The failure mode is the worst this crate has: a buffer that quietly stops matching
    // the file the user is typing in, so every later span points at the wrong bytes.
    // -----------------------------------------------------------------------

    /// Pieces a generated buffer is built from.
    ///
    /// The alphabet matters more than the length: an accent (2 bytes, 1 UTF-16 unit), CJK (3/1), an
    /// emoji (4/2, a surrogate pair), a combining mark (a character that is not a grapheme), and
    /// all three line terminators including a lone `\r`, which puts a `\r\n` next to text that is
    /// not one. Concatenating pieces instead of generating arbitrary `String`s also makes failures
    /// readable: proptest shrinks the *list*, so a counterexample arrives as the few pieces that
    /// still reproduce it.
    const PIECES: &[&str] = &[
        "a",
        "b",
        " ",
        "x = 1",
        "\t",
        "\n",
        "\r\n",
        "\r",
        "caf\u{e9}",
        "\u{65e5}\u{672c}",
        "\u{1f600}",
        "e\u{301}",
    ];

    fn text() -> impl Strategy<Value = String> {
        prop::collection::vec(prop::sample::select(PIECES), 0..24)
            .prop_map(|pieces| pieces.concat())
    }

    fn encoding() -> impl Strategy<Value = PositionEncoding> {
        prop::sample::select(&ALL[..])
    }

    /// The nearest offset at or before `at` that an LSP position can name.
    ///
    /// Two things disqualify an offset: not being a character boundary, and being the `\n` of a
    /// `\r\n`, the one byte that belongs to no line's content (so `position_at` answers with the
    /// end of the line it terminates, and the trip back lands elsewhere).
    /// `offset_inside_a_crlf_reports_the_end_of_the_line_it_terminates` pins that case; here it is
    /// excluded, because a model splicing at an offset with no position would assert the
    /// disagreement, not the conversion.
    fn addressable(text: &str, at: usize) -> usize {
        let mut at = at.min(text.len());
        while at > 0 && (!text.is_char_boundary(at) || inside_crlf(text, at)) {
            at -= 1;
        }
        at
    }

    proptest! {
        /// A change list applied through LSP positions must land exactly where the same splices
        /// land on a plain `String`.
        ///
        /// This is all of incremental sync, stated once. The subject converts byte spans to
        /// `Range`s and the client's `Range`s back to byte spans, over a line index rebuilt after
        /// every edit; the model does `replace_range` and knows nothing of lines, columns or
        /// encodings. They agree only if every conversion is exact, and they must keep agreeing as
        /// the text changes, which is what a list buys over a single edit.
        #[test]
        fn a_change_list_lands_where_plain_string_splices_land(
            start in text(),
            encoding in encoding(),
            changes in prop::collection::vec((any::<usize>(), any::<usize>(), text()), 0..8),
        ) {
            let mut model = start.clone();
            let mut doc = TextDocument::new(start, encoding);

            for (first, second, replacement) in changes {
                // Chosen against the text as it stands *now*, which makes this a history, not a
                // batch: an index generated up front would address a buffer the previous change
                // already moved.
                let one = addressable(&model, first % (model.len() + 1));
                let two = addressable(&model, second % (model.len() + 1));
                let (from, to) = (one.min(two), one.max(two));

                doc.apply(Some(doc.range_at(from as u32, to as u32)), &replacement);
                model.replace_range(from..to, &replacement);

                prop_assert_eq!(doc.text(), &model);
            }
        }

        /// Whatever a client sends, the offset it resolves to is one this crate can slice at.
        ///
        /// Positions arrive from outside and need not be sane: a line past the end, a column
        /// mid-emoji, `u32::MAX`. Every one must come back in range and on a character boundary,
        /// because the next step is `replace_range`, and `String` panics on either mistake.
        #[test]
        fn any_position_a_client_can_send_resolves_to_a_sliceable_offset(
            text in text(),
            encoding in encoding(),
            line in any::<u32>(),
            character in any::<u32>(),
        ) {
            let doc = TextDocument::new(text.clone(), encoding);
            let offset = doc.offset_at(Position { line, character }) as usize;

            prop_assert!(offset <= text.len(), "{offset} is past {:?}", text.len());
            prop_assert!(text.is_char_boundary(offset), "{offset} splits a character in {text:?}");
        }

        /// Every offset a position can name survives the trip out and back.
        ///
        /// `round_trips_every_addressable_char_boundary`'s property over generated strings instead
        /// of hand-written ones, exhaustively within each, so one generated buffer is a whole
        /// family of assertions.
        #[test]
        fn every_addressable_offset_round_trips(text in text(), encoding in encoding()) {
            let doc = TextDocument::new(text.clone(), encoding);

            for offset in 0..=text.len() {
                if !text.is_char_boundary(offset) || inside_crlf(&text, offset) {
                    continue;
                }
                let position = doc.position_at(offset as u32);
                prop_assert_eq!(
                    doc.offset_at(position) as usize,
                    offset,
                    "{:?} at {} came back as {:?}",
                    text,
                    offset,
                    position
                );
            }
        }

        /// The index alone places every *generated* offset where the text does.
        ///
        /// `the_index_alone_places_every_offset_where_the_text_does`, over generated strings and
        /// exhaustively within each. The alphabet is what matters: an accent, CJK, a surrogate
        /// pair, a combining mark and all three line terminators, in every order concatenation can
        /// produce.
        #[test]
        fn the_index_alone_places_every_generated_offset_where_the_text_does(
            text in text(),
            encoding in encoding(),
        ) {
            let index = LineIndex::new(&text);
            let doc = TextDocument::new(text.clone(), encoding);

            for offset in 0..=text.len() {
                if !text.is_char_boundary(offset) || inside_crlf(&text, offset) {
                    continue;
                }
                prop_assert_eq!(
                    position_in(&index, encoding, offset as u32),
                    Some(doc.position_at(offset as u32)),
                    "{:?} at {} ({:?})",
                    text,
                    offset,
                    encoding
                );
            }
        }
    }

    /// The fixture the coordinate bug was found in, laid out so the collision is exact.
    ///
    /// `Alpha.` and `Gamma.` are on consecutive lines, so the two constant references are **exactly
    /// `"Alpha.\n".len()` apart** (seven bytes). Insert seven bytes above them and a buffer offset
    /// naming `Alpha` names `Gamma` in the graph: the other class's reference, byte for byte. That
    /// makes the two tests below a pair: one asserts the right answer, the other that the wrong one
    /// is what you get without the map.
    const SHIFTED: &str = "class Alpha\n  def self.alpha_only\n  end\nend\n\n\
                           class Gamma\n  def self.gamma_only\n  end\nend\n\n\
                           Alpha.\nGamma.\n";

    /// Seven bytes, which is the distance between the two references.
    const PAD: &str = "# pad!\n";

    /// A constant reference that really is one, and a declaration *below* where the pad goes.
    ///
    /// `SHIFTED` cannot serve the jump test: `Alpha.\nGamma.` is one chained call in Ruby, so its
    /// `Gamma` is a method name and resolves to nothing even with no deferral.
    const JUMPABLE: &str = "class Alpha\nend\n\nclass Gamma\nend\n\nGamma\n";

    /// Open `SHIFTED`, index it, then defer and insert `PAD` at the top without indexing.
    fn deferred_after_a_shift() -> (Harness, DocUri) {
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", SHIFTED);
        harness.index();
        harness.open(&uri, SHIFTED);
        // From here the graph is frozen: `didChange` records the edit and nothing indexes it. That
        // is the whole deferred design.
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(0, 0),
                    end: lsp_types::Position::new(0, 0),
                }),
                text: PAD.to_owned(),
            }],
        );
        (harness, uri)
    }

    /// The cursor just after the `.` of `Alpha.`, in the buffer's coordinates.
    fn after_alpha_dot() -> serde_json::Value {
        serde_json::json!({ "line": 11, "character": 6 })
    }

    /// That the request really was answered from the graph as it stood, not by falling back.
    ///
    /// **The fallback is why this is needed.** A refused map settles and asks again, so the
    /// *answer* is right either way, and asserting on it proves nothing about the translation. Only
    /// a truly deferred answer leaves behind a graph that never saw the edit.
    fn assert_deferred(harness: &Harness, uri: &DocUri, indexed: &str) {
        assert_eq!(
            harness.analysis.indexed_text.get(uri).map(String::as_str),
            Some(indexed),
            "the request fell back and indexed the buffer, so the map was never exercised"
        );
    }

    #[test]
    fn a_deferred_completion_is_answered_in_the_graph_s_coordinates_and_not_the_buffer_s() {
        let (mut harness, uri) = deferred_after_a_shift();

        let offered_answer = harness.ask(
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": after_alpha_dot(),
            }),
        );
        let (labels, precise) = offered(&offered_answer);

        // The receiver the *buffer* has is `Alpha`, and nothing was indexed after the edit.
        assert!(
            precise,
            "the deferred answer fell through to the name-based list: {labels:?}"
        );
        assert_eq!(
            labels,
            vec!["alpha_only".to_owned()],
            "the deferred answer is not the receiver the buffer has"
        );
    }

    #[test]
    fn without_the_map_the_same_deferred_completion_answers_the_wrong_class() {
        // Delete the mechanism and watch it break. `rebase_for` falls back to the identity when it
        // has no record of what the document was indexed as, so clearing the record is exactly
        // "defer the index and keep using buffer offsets as graph keys": the configuration this
        // design exists to avoid, where `Alpha.new.` offers `Gamma`'s members.
        let (mut harness, uri) = deferred_after_a_shift();
        harness.analysis.indexed_text.clear();

        let answer = harness.ask(
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": after_alpha_dot(),
            }),
        );
        let (labels, precise) = offered(&answer);

        // What the buffer offset names in the older text is `Gamma`'s reference, and the call's own
        // method reference is narrower, so `locate` keeps the call, `constant_at` finds no
        // constant, and the receiver types as nothing. The result degrades to the **name-based
        // list**, not a confident answer about `Gamma`, and that list matches every method in the
        // project: it includes `gamma_only`, which the correct answer above never offers.
        assert!(
            !precise,
            "without the map the receiver resolved, which this test cannot then tell apart"
        );
        assert!(
            labels.iter().any(|label| label == "gamma_only"),
            "the bug the map exists to remove did not reproduce: {labels:?}"
        );
    }

    #[test]
    fn a_receiver_inside_what_was_just_typed_is_refused_rather_than_guessed_at() {
        // The other half of the map's contract. Here the *receiver itself* is being typed, so no
        // graph offset names it: the map refuses, the request settles and asks again, and `Alph` is
        // a constant nothing declares either way. What must not appear, on either side of that
        // fallback, is `Alpha`'s members: they sit at this offset in the older text, and are wrong
        // twice over.
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", SHIFTED);
        harness.index();
        harness.open(&uri, SHIFTED);
        // Rewrite the `Alpha.` line into `Alph.`: the constant under the cursor is now text the
        // graph has never held.
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(10, 0),
                    end: lsp_types::Position::new(10, 6),
                }),
                text: "Alph.".to_owned(),
            }],
        );

        let answer = harness.ask(
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 10, "character": 5 },
            }),
        );
        let (labels, precise) = offered(&answer);

        assert!(
            !precise,
            "a receiver the graph has never held was typed anyway: {labels:?}"
        );
    }

    /// Two methods with a scope boundary between them, and a `self.` that can tell the class
    /// side from the instance side by which name comes back.
    const TWO_SCOPES: &str = "class Alpha\n  def self.klass_only\n  end\n\n                                def inst_only\n  end\n\n  def first\n    x = 1\n  end\n\n                                def second\n    y = 2\n  end\nend\n";

    #[test]
    fn an_edit_that_swallows_a_scope_boundary_does_not_answer_from_the_wrong_scope() {
        // **The case `changed_in_graph` cannot serve and the fallback cannot catch.** The edit runs
        // from inside `first` to inside `second`, so the region the graph disagrees with spans an
        // `end` and a `def`. The narrowest graph scope containing all of it is the *class body*,
        // where `self` is the class object, so `self.` would offer the singleton while the caret is
        // plainly inside an instance method. That answer is wrong, not empty, so no retry happens.
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", TWO_SCOPES);
        harness.index();
        harness.open(&uri, TWO_SCOPES);
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(8, 4),
                    end: lsp_types::Position::new(12, 9),
                }),
                text: "self.".to_owned(),
            }],
        );

        let answer = harness.ask(
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 8, "character": 9 },
            }),
        );
        let (labels, _precise) = offered(&answer);

        assert!(
            labels.iter().any(|label| label == "inst_only"),
            "the caret is inside an instance method and was offered {labels:?}"
        );
        assert!(
            !labels.iter().any(|label| label == "klass_only"),
            "answered from the class body's scope: {labels:?}"
        );
    }

    #[test]
    fn a_member_typed_after_a_settled_receiver_is_answered_without_indexing_it() {
        // **The path the whole design is for**, which the other two tests do not reach. The *caret*
        // is inside text the graph has never seen while the receiver is not: `to_graph` refuses the
        // cursor, the scope question is asked over the changed region instead, and
        // `Receiver::Constant` still maps because it sits in the common prefix. Every keystroke of
        // a member name goes this way.
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", SHIFTED);
        harness.index();
        harness.open(&uri, SHIFTED);
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(10, 6),
                    end: lsp_types::Position::new(10, 6),
                }),
                text: "al".to_owned(),
            }],
        );

        let answer = harness.ask(
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 10, "character": 8 },
            }),
        );
        let (labels, precise) = offered(&answer);

        assert!(
            precise && labels.iter().any(|label| label == "alpha_only"),
            "a member typed on a settled receiver answered {labels:?}"
        );
        // And it was answered *deferred*: the fallback would have indexed the buffer, so the
        // indexer's last text still being the file on disk proves the graph was never touched.
        // Without this, the assertion above passes either way.
        assert_eq!(
            harness.analysis.indexed_text.get(&uri).map(String::as_str),
            Some(SHIFTED),
            "the deferred path indexed the buffer after all"
        );
    }

    #[test]
    fn a_deferred_hover_cards_the_constant_under_the_caret_and_not_the_one_below_it() {
        // `PAD` is the distance between the two references on purpose, so an offset handed to the
        // graph unmapped lands exactly on the *other* class: a precise, confident card about the
        // wrong constant. The range must come back through the map too, or the highlight sits a
        // line above the word.
        let (mut harness, uri) = deferred_after_a_shift();

        let answer = harness.ask(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 11, "character": 2 },
            }),
        );

        let card = answer["contents"]["value"].as_str().unwrap_or_default();
        assert!(
            card.contains("Alpha") && !card.contains("Gamma"),
            "the caret is on Alpha and the card said: {card}"
        );
        assert_eq!(
            answer["range"]["start"]["line"], 11,
            "the span came back in the graph's coordinates rather than the buffer's"
        );
        assert_deferred(&harness, &uri, SHIFTED);
    }

    #[test]
    fn a_deferred_jump_lands_where_the_declaration_is_now_and_not_where_it_was() {
        // The inverse map's own test, with both halves of the map in it: the caret is below the
        // edit and so is the class it names. `class Gamma` is on line 3 of the graph's text and
        // line 4 of the buffer, so a jump answered in the graph's coordinates lands a line above
        // the class: right file, wrong line, and nothing in the answer says so.
        //
        // **The pad goes in the middle**, so the declaration is clear of the seam and both ends of
        // its span shift whichever way they lean. A declaration starting *at* the seam has its own
        // test. `assert_deferred` keeps both honest: a refused map settles and re-asks, so the
        // answer is right whether or not the translation ran.
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", JUMPABLE);
        harness.index();
        harness.open(&uri, JUMPABLE);
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(2, 0),
                    end: lsp_types::Position::new(2, 0),
                }),
                text: PAD.to_owned(),
            }],
        );

        let answer = harness.ask(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 7, "character": 2 },
            }),
        );

        assert_eq!(
            answer[0]["targetSelectionRange"]["start"]["line"],
            serde_json::json!(4),
            "the jump answered {answer} instead of `class Gamma` on line 4"
        );
        assert_eq!(
            answer[0]["originSelectionRange"]["start"]["line"],
            serde_json::json!(7),
            "the origin came back in the graph's coordinates rather than the buffer's"
        );
        assert_deferred(&harness, &uri, JUMPABLE);
    }

    /// One class opened in two files: the shape that kept the loss silent.
    ///
    /// A constant with many declarations is ordinary Ruby and routine Rails. `link` maps four
    /// offsets per place and drops the *place* when any of them refuses, so losing one makes the
    /// response shorter, not empty: `answered_nothing` is false, the settle-and-retry never fires,
    /// and nothing says a place is missing.
    const REOPENED_ONE: &str = "class Alpha\n  def one\n  end\nend\n";
    const REOPENED_TWO: &str = "class Alpha\n  def two\n  end\nend\n";

    #[test]
    fn a_deferred_jump_keeps_a_place_whose_declaration_begins_its_file() {
        // The pad goes at the very top of `lib/one.rb`, so `class Alpha` there starts at graph
        // offset 0, the one offset with no byte to its left. Without `at_an_untouched_edge`, that
        // place is dropped and the jump returns one location instead of two, silently and with no
        // retry.
        let mut harness = Harness::new();
        let one = harness.write("lib/one.rb", REOPENED_ONE);
        let two = harness.write("lib/two.rb", REOPENED_TWO);
        let main = harness.write("lib/main.rb", "Alpha\n");
        harness.index();
        harness.open(&one, REOPENED_ONE);
        harness.edit_without_indexing(
            &one,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(0, 0),
                    end: lsp_types::Position::new(0, 0),
                }),
                text: PAD.to_owned(),
            }],
        );

        let answer = harness.ask(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": main.as_str() },
                "position": { "line": 0, "character": 2 },
            }),
        );
        let places: Vec<(&str, i64)> = answer
            .as_array()
            .expect("definition answers an array of links")
            .iter()
            .map(|link| {
                (
                    link["targetUri"].as_str().unwrap_or_default(),
                    link["targetSelectionRange"]["start"]["line"]
                        .as_i64()
                        .unwrap_or(-1),
                )
            })
            .collect();

        assert!(
            places
                .iter()
                .any(|(uri, line)| *uri == one.as_str() && *line == 1),
            "the place in the edited file was dropped or misplaced: {places:?}"
        );
        assert!(
            places
                .iter()
                .any(|(uri, line)| *uri == two.as_str() && *line == 0),
            "the untouched file's place is gone too: {places:?}"
        );
        assert_deferred(&harness, &one, REOPENED_ONE);
    }

    /// A typed instance variable and a call on it: the pair of paths the map reaches.
    ///
    /// `@thing` is typed by an assignment above the cursor, and that assignment yields a `Receiver`
    /// holding the **offset of `Alpha`**, a graph key parsed out of the buffer. Both cards below go
    /// through `types::method_receiver` on it: one asks what the variable is, the other what a
    /// member on it resolves to. Without translation, an unindexed keystroke anywhere above them
    /// loses the assignment, and the card falls to the variable's own name.
    const TYPED_IVAR: &str = "class Alpha\n  def only_alpha\n  end\nend\n\n\
                              class Holder\n  def run\n    @thing = Alpha.new\n\
                              \u{20}   @thing.only_alpha\n  end\nend\n";

    /// Open `TYPED_IVAR`, index it, then defer and insert `PAD` at the top without indexing.
    ///
    /// The pad is above everything, so every offset the two cards need moves by exactly its length
    /// and nothing the user pointed at changed: the edit a deferred answer must survive completely,
    /// and the one the audit makes at every position it samples.
    fn deferred_after_a_typed_ivar() -> (Harness, DocUri) {
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", TYPED_IVAR);
        harness.index();
        harness.open(&uri, TYPED_IVAR);
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(0, 0),
                    end: lsp_types::Position::new(0, 0),
                }),
                text: PAD.to_owned(),
            }],
        );
        (harness, uri)
    }

    #[test]
    fn a_deferred_card_on_an_instance_variable_keeps_the_type_its_assignment_gives_it() {
        // `locator::resolve_variable` reads the buffer and keys the graph, and the hinge is the
        // constant inside the assignment: `Alpha` sits at one offset in the buffer and another in
        // the graph's text. Untranslated, it resolves to nothing, the chain collapses, and only the
        // variable's spelling is left: a *guessed* card where a derived one stood a keystroke
        // earlier.
        let (mut harness, uri) = deferred_after_a_typed_ivar();

        let answer = harness.ask(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 9, "character": 6 },
            }),
        );
        let card = answer["contents"]["value"].as_str().unwrap_or_default();

        assert!(
            card.contains(": Alpha"),
            "the assignment stopped typing the variable: {card}"
        );
        assert!(
            !card.contains("Guessed from name alone"),
            "the deferred card fell to the variable's own name: {card}"
        );
        assert_deferred(&harness, &uri, TYPED_IVAR);
    }

    #[test]
    fn a_deferred_call_on_an_instance_variable_is_typed_in_the_graph_s_coordinates() {
        // The same receiver one token to the right, reached by a different function:
        // `locator::typed`, which `hover` and `definition` share. The failure is quieter here:
        // `only_alpha` is declared once in the fixture, so the name-based list finds it anyway and
        // *the answer looks right*. The footnote tells them apart: a name-matched list says so,
        // while a really typed receiver says where the type came from.
        let (mut harness, uri) = deferred_after_a_typed_ivar();

        let answer = harness.ask(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 9, "character": 14 },
            }),
        );
        let card = answer["contents"]["value"].as_str().unwrap_or_default();

        assert!(
            card.contains("Alpha#only_alpha"),
            "the member did not resolve at all: {card}"
        );
        assert!(
            !card.contains("Guessed from name alone"),
            "the receiver was not typed and the name list answered instead: {card}"
        );
        assert!(
            !card.contains("Guessed from name alone"),
            "the type stopped coming from the assignment: {card}"
        );
        assert_deferred(&harness, &uri, TYPED_IVAR);
    }

    #[test]
    fn a_templates_variable_survives_an_unindexed_edit_in_the_controller_that_types_it() {
        // **The cross-document half: the cursor is not in the edited file at all.** The
        // view↔renderer rung reads the controller's *buffer* on purpose (an unsaved controller
        // should type the template it renders), and that is exactly when the controller's offsets
        // stop naming the graph's text. So the map to apply is the controller's, fetched with its
        // text, not derived from the request's document, whose map is the only one the template's
        // request knows.
        let mut harness = Harness::new();
        harness.write("sig/nil.rbs", "class NilClass\nend\n");
        harness.write("app/models/story.rb", STORY);
        let controller = harness.write("app/controllers/stories_controller.rb", CONTROLLER);
        let view = harness.write(
            "app/views/stories/show.html.erb",
            "<h1><%= @story.title %></h1>\n",
        );
        harness.index();
        harness.open(&controller, CONTROLLER);
        harness.edit_without_indexing(
            &controller,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(0, 0),
                    end: lsp_types::Position::new(0, 0),
                }),
                text: PAD.to_owned(),
            }],
        );

        let answer = harness.ask(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": view.as_str() },
                "position": { "line": 0, "character": 15 },
            }),
        );
        let card = answer["contents"]["value"].as_str().unwrap_or_default();

        assert!(
            card.contains("Story#title"),
            "an edit in another document lost the template's type: {card}"
        );
        // The whole rung, and the assertion that fires without the map: `Story#title` is *also*
        // what the name guess answers, because the variable is spelled after its class. The right
        // answer and the lucky one differ only in the footnote.
        assert!(
            !card.contains("Guessed from name alone"),
            "the convention gave way to the name guess: {card}"
        );
        // Line 4, not line 3: the lookup follows the graph and the line a reader is sent to follows
        // the buffer. This is the one card that can tell them apart.
        assert!(
            card.contains("Story#title") && !card.contains("Guessed from name alone"),
            "the controller stopped typing the variable: {card}"
        );
        assert_deferred(&harness, &controller, CONTROLLER);
    }

    #[test]
    fn an_assignment_being_typed_is_refused_rather_than_read_at_the_old_offsets() {
        // The other half of the map's contract, on the rung above: the edit is *inside* the
        // constant the assignment names, so no graph offset stands for it. What must not appear is
        // `Alpha`: it sits at that offset in the older text, and a card naming it would be
        // confident, precise and about a class the buffer no longer mentions. Falling to the
        // variable's own name is the intended degradation.
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", TYPED_IVAR);
        harness.index();
        harness.open(&uri, TYPED_IVAR);
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(7, 13),
                    end: lsp_types::Position::new(7, 18),
                }),
                text: "Alph".to_owned(),
            }],
        );

        let answer = harness.ask(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 8, "character": 6 },
            }),
        );
        let card = answer["contents"]["value"].as_str().unwrap_or_default();

        assert!(
            !card.contains("class Alpha"),
            "a receiver the graph has never held was typed anyway: {card}"
        );
    }

    #[test]
    fn a_controller_assignment_being_typed_leaves_the_template_the_rung_below() {
        // The same refusal one document over, and why it is a `continue`, not a decline: the
        // request is about the *template*, which nobody touched, so nothing here would trigger a
        // settle-and-retry. The rung says it cannot answer, the name guess answers instead, and the
        // card says which. That is all that is owed while somebody is mid-word in another file.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        let controller = harness.write("app/controllers/stories_controller.rb", CONTROLLER);
        let view = harness.write(
            "app/views/stories/show.html.erb",
            "<h1><%= @story.title %></h1>\n",
        );
        harness.index();
        harness.open(&controller, CONTROLLER);
        harness.edit_without_indexing(
            &controller,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(2, 13),
                    end: lsp_types::Position::new(2, 18),
                }),
                text: "Stor".to_owned(),
            }],
        );

        let answer = harness.ask(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": view.as_str() },
                "position": { "line": 0, "character": 15 },
            }),
        );
        let card = answer["contents"]["value"].as_str().unwrap_or_default();

        assert!(
            !card.contains("Type taken from `StoriesController`"),
            "the convention answered from an assignment the graph no longer holds: {card}"
        );
        assert_deferred(&harness, &controller, CONTROLLER);
    }

    #[test]
    fn an_index_that_crashed_does_not_leave_the_map_claiming_the_graph_caught_up() {
        // **The bulkhead meets the map, and the failure is silent both ways.** A contained panic
        // costs the document its update: the graph keeps its previous version. If `indexed_text`
        // recorded the text that *failed* to go in, the map would compare two equal strings, answer
        // `identity`, and hand a buffer offset to a graph some edits behind, with no refusal, so
        // nothing falls back.
        //
        // On *this* fixture the caret would land seven bytes along, on the `Gamma` of
        // `Alpha.\nGamma.` (one chained call in Ruby, so a method name that resolves to nothing),
        // and the symptom is silence, not a wrong class. It is the same defect either way: the
        // offset went to a graph that never got the edit. The assertion is that the answer is
        // right, which covers both.
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", SHIFTED);
        harness.index();
        harness.open(&uri, SHIFTED);
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(0, 0),
                    end: lsp_types::Position::new(0, 0),
                }),
                text: PAD.to_owned(),
            }],
        );
        // Armed, not written into the text: the sentinel is 33 bytes and this fixture depends on a
        // *seven*-byte shift. A sentinel in the buffer would destroy the common suffix, the map
        // would refuse everything, and the test would pass on a refusal instead of on the map being
        // right.
        indexer::SOURCE_INDEXES_TO_CRASH.with(|counter| counter.set(1));
        // Something that is not deferred forces the index, which panics and is contained.
        harness.analysis.settle();

        let answer = harness.ask(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 11, "character": 2 },
            }),
        );

        let card = answer["contents"]["value"].as_str().unwrap_or_default();
        assert!(
            card.contains("Alpha") && !card.contains("Gamma"),
            "the map trusted an index that never happened and said: {card}"
        );
        assert_deferred(&harness, &uri, SHIFTED);
    }

    #[test]
    fn a_refused_receiver_is_answered_by_indexing_rather_than_by_answering_nothing() {
        // **The map is an optimization, not a filter**, and this test says so. Rewriting `Alpha.`
        // into `Gamma.` puts a constant the graph knows well at an offset the graph has never seen;
        // the refusal is about the *offset*, not the name, so the deferred attempt has nothing to
        // say. Answering nothing would trade a correct answer for a fast empty list, so the request
        // settles and asks again.
        let mut harness = Harness::new();
        let uri = harness.write("lib/main.rb", SHIFTED);
        harness.index();
        harness.open(&uri, SHIFTED);
        harness.edit_without_indexing(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position::new(10, 0),
                    end: lsp_types::Position::new(10, 6),
                }),
                text: "Gamma.".to_owned(),
            }],
        );

        let answer = harness.ask(
            "textDocument/completion",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 10, "character": 6 },
            }),
        );
        let (labels, precise) = offered(&answer);

        assert!(
            precise && labels.iter().any(|label| label == "gamma_only"),
            "a refused deferral answered {labels:?} instead of indexing and answering Gamma's"
        );
    }

    #[test]
    fn navigation_sees_an_incremental_edit_immediately() {
        // The buffer the editor is typing into shadows the file on disk, and every answer has
        // to come from the buffer — including one assembled from range edits.
        let mut harness = Harness::new();
        let source = "class Person\n  def shout; end\nend\n";
        let uri = harness.write("lib/person.rb", source);
        harness.index();
        harness.open(&uri, source);

        harness.edit(
            &uri,
            vec![TextChange {
                range: Some(lsp_types::Range {
                    start: lsp_types::Position {
                        line: 1,
                        character: 6,
                    },
                    end: lsp_types::Position {
                        line: 1,
                        character: 11,
                    },
                }),
                text: "whisper".to_owned(),
            }],
        );

        let edited = "class Person\n  def whisper; end\nend\n";
        let markdown = harness.hover_at(&uri, edited, "whisper")["contents"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned();
        assert!(markdown.contains("Person#whisper"), "{markdown}");

        // And the old name is gone from the index rather than merely shadowed by it.
        let outline = harness.outline(&uri);
        assert_eq!(outline[0]["children"][0]["name"], "whisper", "{outline}");
        assert_eq!(
            outline[0]["children"].as_array().unwrap().len(),
            1,
            "{outline}"
        );
    }

    #[test]
    fn navigation_ranges_are_in_the_negotiated_encoding() {
        // This failure is invisible on ASCII and silent everywhere else: a range built from byte
        // offsets sends the editor to the wrong column on every line with an emoji, an accent or
        // CJK text.
        let source = "x = \"\u{1f600}\u{1f600}\"; class Person; end\n";
        for (encoding, expected) in [
            (PositionEncoding::Utf8, 22),  // two 4-byte emoji
            (PositionEncoding::Utf16, 18), // two surrogate pairs
            (PositionEncoding::Utf32, 16), // two characters
        ] {
            let mut harness = Harness::with_encoding(encoding);
            let uri = harness.write("lib/person.rb", source);
            harness.index();

            let outline = harness.outline(&uri);
            assert_eq!(
                outline[0]["selectionRange"]["start"]["character"], expected,
                "{encoding:?}: {outline}"
            );

            // And the request side agrees: a position expressed in the same units has to come
            // back to the same byte offset, or hover would land one construct off.
            let hover = harness.ask(
                "textDocument/hover",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": 0, "character": expected },
                }),
            );
            assert!(
                hover["contents"]["value"]
                    .as_str()
                    .is_some_and(|markdown| markdown.contains("class Person")),
                "{encoding:?}: {hover}"
            );
        }
    }

    /// The same call under three prefixes, one of them prose in English.
    ///
    /// A line of markup is a line of a *document*, and a client counts its columns in UTF-16 units
    /// of its own text. Only the first and last lines here agree with the bytes: the middle line's
    /// quotes, accent and emoji are seven units short of their eighteen bytes.
    const WIDE: &str = "\
<p>plain <%= Story::TAGLINE %></p>
<p>\u{201c}curly\u{201d} caf\u{e9} \u{1f680} <%= Story::TAGLINE %></p>
<p><%= Story::TAGLINE %></p>
";

    /// The markup left of a cursor must not move it, whatever the markup is made of.
    ///
    /// An LSP position counts UTF-16 units of the text the **client** has. Converting it against
    /// the blanked view, where a 3-byte `\u{201c}` has become three spaces, shifts the cursor left
    /// by (bytes − units) of every non-ASCII character before it on the line, and every returned
    /// range right by the same amount. On a dense line the shifted cursor lands on a different
    /// identifier: a *wrong* answer, not just a missing one.
    ///
    /// Both directions are one bug, and this checks both at once. `definition` reads a position the
    /// client sent, and its `originSelectionRange` is a position the client will use. The middle
    /// row is the assertion: the cursor arrives at column 30, and the span comes back as columns 30
    /// to 37, not the bytes 37 to 44 it was found at.
    ///
    /// It reproduces the bug: with the conversion pointed back at the view, the middle row answers
    /// **`story.rb:0`**, because the seven-unit shift puts the cursor inside `Story::` and the jump
    /// lands on `class Story`. The other two rows do not move, which is why only people whose
    /// markup is not English would ever see it.
    #[test]
    fn a_cursor_in_a_template_is_where_the_editor_put_it() {
        /// The LSP position of `needle` on `line`, counted the way a client counts.
        fn at(source: &str, line: usize, needle: &str) -> serde_json::Value {
            let text = source.lines().nth(line).expect("the line");
            let column = text.find(needle).expect("the needle");
            serde_json::json!({
                "line": line,
                "character": text[..column].encode_utf16().count(),
            })
        }

        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        let view = harness.write("app/views/stories/index.html.erb", WIDE);
        harness.index();

        let mut drawn = vec![format!(
            "{:<8}{:>14}{:>16}",
            "prefix", "asked at", "jumps to"
        )];
        for (line, prefix) in ["ascii", "wide", "none"].iter().enumerate() {
            let asked = at(WIDE, line, "TAGLINE");
            let defined = harness.ask(
                "textDocument/definition",
                serde_json::json!({
                    "textDocument": { "uri": view.as_str() },
                    "position": asked.clone(),
                }),
            );
            let link = defined
                .as_array()
                .and_then(|links| links.first())
                .cloned()
                .unwrap_or_default();
            let origin = &link["originSelectionRange"];
            let span = match origin["start"]["character"].as_u64() {
                Some(_) => format!(
                    "{}-{}",
                    origin["start"]["character"], origin["end"]["character"]
                ),
                None => "\u{2014}".to_owned(),
            };
            let target = link["targetUri"].as_str().map_or_else(String::new, |uri| {
                uri.rsplit('/').next().unwrap_or_default().to_owned()
            });
            let jump = match link["targetSelectionRange"]["start"]["line"].as_u64() {
                Some(at) => format!("{target}:{at}"),
                None => "\u{2014}".to_owned(),
            };
            // The caret the editor placed, drawn beside the span it gets back: the two agree
            // only if the same text was counted on both trips.
            drawn.push(format!(
                "{prefix:<8}{:>14}{jump:>16}",
                format!("{}:{}", asked["character"], span)
            ));
        }

        assert_eq!(
            drawn.join("\n"),
            "prefix        asked at        jumps to\n\
             ascii         20:20-27      story.rb:1\n\
             wide          30:30-37      story.rb:1\n\
             none          14:14-21      story.rb:1"
        );
    }

    #[test]
    fn a_non_ascii_line_of_markup_above_the_cursor_does_not_move_the_answer() {
        // Why the ERB scanner pads by byte, not by character: padding by character would let the
        // emoji on the line above shorten the buffer by three bytes, and every offset below would
        // be wrong, silently, and only for people who do not write markup in English.
        let wide = VIEW.replace(
            "<h1>Stories</h1>",
            "<h1>\u{413}\u{43e}\u{440}\u{44f}\u{447}\u{438}\u{435} \u{1f525}</h1>",
        );
        let mut harness = Harness::new();
        let model = harness.write("app/models/story.rb", STORY);
        harness.write("app/views/stories/index.html.erb", &wide);
        harness.index();

        // The same line and column as the ASCII fixture above: the markup grew by fourteen bytes
        // and the Ruby did not move.
        assert_eq!(
            harness.reference_list(&model, STORY, "title", true),
            ["story.rb:3:6", "index.html.erb:2:15"]
        );
    }
}
