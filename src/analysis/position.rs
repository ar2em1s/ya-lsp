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
/// keep our own copy regardless. It is also what incremental sync and cursor context need.
///
/// # Two texts, one set of offsets
///
/// A document is *read* and it is *addressed*, and for every file but one those are the same
/// string. A template is not: it is read as the blanked Ruby view
/// [`erb::ruby_view`](super::erb::ruby_view) makes of it, and it is addressed as the markup the
/// editor actually has open. The byte offset is common to both — the view preserves length and
/// every line break, which is the whole of why ERB needs no position map — but a *column* is
/// not, because a column is a count of code units and blanking a 3-byte `“` writes three
/// spaces where the client counts one UTF-16 unit. So [`Self::text`] is what is read and
/// [`Self::coordinates`] is what columns are counted in.
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
    /// The two are the same length, byte for byte, with their line breaks in the same places —
    /// [`erb::ruby_view`](super::erb::ruby_view)'s central property, held by a `proptest` in
    /// that module, and the only reason one line index and one byte offset can serve both. What
    /// differs is how many code units a prefix is, so counting a column against the view
    /// displaces the cursor left by (bytes − units) of every non-ASCII character in the markup
    /// before it on the line, and displaces every span this server answers with by the same
    /// amount in the other direction.
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
        // An edited document addresses itself. The buffer an editor edits is never a blanked
        // view — `with_text` builds those per request and hands out a shared reference — but a
        // `source` that outlived an edit would count columns in text this document no longer
        // holds, which is the failure this module exists to make impossible.
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
    /// Out-of-range input is clamped rather than rejected: the LSP spec tells clients to clamp,
    /// but a rejected position here would mean a dropped request, and a panic would take the
    /// server down. A position that splits a multi-byte character resolves to that character's
    /// first byte.
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

    /// The text a position's line and column are counted in: the document itself, unless it is
    /// a template, in which case it is the markup the editor has and not the view.
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
/// **What makes deferring the index answerable at all.** rubydex's offsets index the text the
/// indexer was last given, and `cursor::at` reads the *buffer*. The two are the same string
/// wherever nothing has been typed since the last settle, and nothing has to translate; between
/// a keystroke and the settle that indexes it they are not, and a buffer offset used as a graph
/// key names different text — a **wrong** answer rather than a missing one.
///
/// It is [`TextDocument::blanked`]'s move on a second axis. There a document is *read* as one
/// text and *addressed* as another because blanking replaced markup; here it is because time
/// passed. The
/// shared coordinate is the byte offset, and the translation is a function of the two texts —
/// not of a maintained edit log, which is the version that can drift.
///
/// The map is a common byte prefix and a common byte suffix, so the buffer's
/// `[prefix, buffer_len - suffix)` and the graph's `[prefix, graph_len - suffix)` are the
/// regions that differ. Outside them the translation is exact; **inside, there is no answer and
/// the caller must refuse** — a scattered edit merely widens the refused region, so this
/// degrades safe rather than approximating.
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

    /// Two scans, both early-exit. Boundaries are backed off to char boundaries so a translated
    /// offset can never split a character — the same rule `clamp_to_char_boundary` keeps.
    #[must_use]
    pub fn between(buffer: &str, indexed: &str) -> Self {
        /// Whether a byte can be part of one of the words this map has to keep whole.
        ///
        /// Deliberately wider than Ruby's identifier: `:` keeps a constant *path* together, so
        /// `XY::Person` and `HR::Person` do not share `::Person` as an unchanged suffix, and
        /// every non-ASCII byte counts because an identifier may hold one and a wrong answer
        /// costs more here than a refusal does.
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
        // **Byte identity is not token identity, and every consumer of this map needs the
        // second.** `Alpha.` rewritten to `Gamma.` shares its last two bytes — `a.` — so a
        // purely byte-wise scan leaves that `.` in the common suffix and hands back an offset
        // the graph files `Alpha` under, which is the wrong class rather than no class.
        // `Receiver::Constant` holds the byte after a constant path and `Instance` the byte
        // after the constant it was built from, so an offset stands for a whole word and
        // survives only if that word did. The changed region is therefore widened outward
        // over word bytes at both ends.
        //
        // The condition is two-sided on purpose: a head that ends on `\n` before `class`
        // ends *at* a token boundary, and widening it would refuse offsets in text neither
        // side touched. It widens only where the word really straddles the seam.
        while prefix > 0
            && is_word_byte(b[prefix - 1])
            && (b.get(prefix).copied().is_some_and(is_word_byte)
                || g.get(prefix).copied().is_some_and(is_word_byte))
        {
            prefix -= 1;
        }
        // **No char-boundary back-off, because the rule above already is one.** A prefix that
        // ends in the middle of a character has a continuation byte on both sides of it, every
        // byte of a multi-byte character is `>= 0x80`, and `is_word_byte` calls all of those
        // word bytes — so the loop above walks out of the character before it stops. The same
        // holds at the tail. `every_translated_offset_round_trips_and_lands_on_a_boundary` is
        // what holds that property, and it is the reason the `0x80` arm may not be narrowed
        // without putting an explicit back-off back.
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

    /// The graph offset a buffer offset names, or `None` where the text under it is text the
    /// graph has never been given.
    #[must_use]
    pub fn to_graph(&self, offset: u32) -> Option<u32> {
        self.map(offset, self.buffer_len, self.graph_len)
    }

    /// The region of the graph's text the buffer no longer agrees with, which is the widest a
    /// scope question may be asked over when the cursor itself cannot be translated.
    #[must_use]
    pub fn changed_in_graph(&self) -> (u32, u32) {
        (self.prefix, self.graph_len.saturating_sub(self.suffix))
    }

    /// The inverse: a span the graph handed back, in the buffer's coordinates.
    ///
    /// **Not optional, and the failure it prevents is worse than a slow answer.** `hover` and
    /// `definition` answer with a range `locate` found in the *graph*, and a deferred buffer is
    /// not the text those offsets came from — so without this a jump lands on the line a
    /// declaration has moved off. `None` where the span overlaps what was just typed: it has no
    /// honest position in the new text, and the request settles and asks again rather than
    /// guessing at one.
    #[must_use]
    pub fn to_buffer(self, offset: u32) -> Option<u32> {
        self.map(offset, self.graph_len, self.buffer_len)
    }

    /// The body both directions share, which is why they take their lengths as arguments.
    ///
    /// **One copy on purpose.** The rule below is subtle enough that it has already been wrong
    /// once — the bounds were `<=` and had to become strict — and a second copy is a second
    /// place for the next correction to miss.
    fn map(&self, offset: u32, from_len: u32, to_len: u32) -> Option<u32> {
        if self.is_identity() {
            return (offset <= from_len).then_some(offset);
        }
        // **Strictly** inside the common prefix or the common suffix, and the strictness is a
        // defect this module's own test found rather than caution. A position is a gap between two
        // bytes, and it names the same place in both texts only when *both* of those bytes are
        // unchanged — `offset < prefix` says the byte after it is, and `offset > from_len -
        // suffix` says the byte before it is.
        //
        // With `<=` a pure deletion slips through: `Alpha.` becoming `Alph.` leaves the buffer
        // side of the changed region **empty**, so a boundary offset refused nothing, and the
        // end of the constant the user is halfway through typing landed inside the span of the
        // longer one the graph still holds — a precise answer about `Alpha` for a receiver
        // spelled `Alph`, which is exactly the class of wrong answer this map exists to stop.
        //
        // It also makes the map injective, which `<=` was not: an insertion had two buffer
        // positions straddling it naming one graph position.
        if offset < self.prefix {
            return Some(offset);
        }
        if offset > from_len.saturating_sub(self.suffix) {
            let shifted = i64::from(offset) + i64::from(to_len) - i64::from(from_len);
            return u32::try_from(shifted).ok();
        }
        None
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn an_unedited_document_maps_every_offset_to_itself() {
        let text = "class Story\n  def title\n  end\nend\n";
        let rebase = Rebase::between(text, text);
        assert!(rebase.is_identity());
        for offset in 0..=text.len() as u32 {
            assert_eq!(rebase.to_graph(offset), Some(offset));
            assert_eq!(rebase.to_buffer(offset), Some(offset));
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
        // The two positions straddling the inserted `f` are refused rather than both being
        // called graph 10, which is what makes the map injective and the round trip an
        // equality rather than an idempotence.
        assert_eq!(rebase.to_graph(10), None);
        assert_eq!(rebase.to_graph(11), None);
        for offset in 0..=buffer.len() as u32 {
            if let Some(graph) = rebase.to_graph(offset) {
                assert_eq!(rebase.to_buffer(graph), Some(offset), "at {offset}");
            }
        }
    }

    #[test]
    fn an_offset_inside_the_edit_is_refused_rather_than_approximated() {
        // Mid-typing a constant: the graph has never held the text under this cursor, and the
        // whole point of the map is that it says so instead of naming a neighbour.
        let rebase = Rebase::between("x = Stor\n", "x = Widget\n");
        assert_eq!(rebase.to_graph(3), Some(3));
        // The gap where the two constants start diverging is refused too: the byte after it is
        // `S` in one text and `W` in the other, so it is not the same place.
        assert_eq!(rebase.to_graph(4), None);
        assert_eq!(rebase.to_graph(6), None);
        assert_eq!(rebase.to_graph(7), None);
    }

    #[test]
    fn a_scattered_edit_widens_the_refused_region_rather_than_lying() {
        // Two edits far apart collapse to one region spanning both. Every offset it covers is
        // refused, which is safe; nothing outside it is wrong.
        let rebase = Rebase::between("a = 2\nb = 1\nc = 4\n", "a = 1\nb = 1\nc = 3\n");
        assert_eq!(rebase.to_graph(0), Some(0));
        // Both edges of the region and everything between them — including the whole untouched
        // middle line — are refused; only what has an unchanged byte on either side maps.
        assert_eq!(rebase.to_graph(3), Some(3));
        assert_eq!(rebase.to_graph(4), None);
        assert_eq!(rebase.to_graph(9), None);
        assert_eq!(rebase.to_graph(17), None);
        assert_eq!(rebase.to_graph(18), Some(18));
    }

    #[test]
    fn a_word_the_graph_still_holds_widens_the_seam_even_where_the_buffer_broke_it() {
        // **Why the straddle test is two-sided.** Here the buffer has `-b` where the graph has
        // `ab`: on the buffer's side `b` starts a token of its own, so asking only the buffer
        // would leave `b.x` in the common suffix and hand back the offset the graph files `ab`
        // under. The graph's side is what knows the word was longer.
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
        // The round-trip proptest only ever asks the inverse about offsets the forward map
        // accepted, so it cannot reach the refusal — and an inverse that answered inside the
        // edit would put a graph span onto buffer text that never held it.
        let rebase = Rebase::between("Gamma.\n", "Alpha.\n");
        let (lo, hi) = rebase.changed_in_graph();
        assert!(lo < hi, "the fixture has to leave a region to refuse");
        assert_eq!(rebase.to_buffer(lo + 1), None);
    }

    #[test]
    fn a_word_that_changed_is_never_mapped_onto_the_word_that_replaced_it() {
        // **The map is about tokens and not about bytes**, and a same-length replacement is
        // what tells the two apart. `Alpha.` and `Gamma.` share `a.`, so a byte-wise scan puts
        // the `.` — which is exactly the offset `Receiver::Constant` holds — in the common
        // suffix and maps a cursor on one constant onto the other. Every offset the two words
        // occupy, and the `.` after them that `Receiver::Constant` actually holds, has to be
        // refused. The newline past that is common to both texts and no constant is filed
        // under it, so it maps and should.
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
        // unchanged suffix — the reference the graph holds is filed under the whole path, and
        // its end offset means a different class in the two texts.
        let rebase = Rebase::between("XY::Person.\n", "HR::Person.\n");
        let end = "XY::Person".len() as u32;
        assert_eq!(rebase.to_graph(end), None);
    }

    #[test]
    fn an_edit_that_ends_on_a_token_boundary_widens_nothing() {
        // The other side of the two-sided condition. `# pad!\n` inserted in front of `class`
        // ends *at* a boundary, so the word after it did not change and refusing offsets in it
        // would cost the map most of its value.
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
        // The char-boundary rule again: a translated offset that is not one would index
        // into the middle of a character and panic the moment anybody slices with it.
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
                    prop_assert_eq!(rebase.to_buffer(graph), Some(offset));
                }
            }
        }
    }

    #[test]
    fn length_is_bytes_and_agrees_with_emptiness() {
        // Offsets are byte offsets everywhere in this crate — `offset_at` clamps to `len` —
        // so a multi-byte character must count as its bytes rather than as one character.
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
    /// The `offset < len` guard is not defensive: `0..=len` is the range every caller here
    /// walks, and the end of the buffer is a perfectly ordinary offset to ask about. It was
    /// missing for as long as no fixture ended in a bare `\r`, which is exactly the kind of
    /// hole a hand-written corpus leaves and `PIECES` does not — the properties below found it
    /// on their first run.
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

    /// A blanked document is addressed exactly as the text the client has.
    ///
    /// The whole of what [`TextDocument::blanked`] is for, stated as an equality: for a template
    /// and the Ruby view of it, every conversion in this module must answer what it answers for
    /// the template alone. `text()` is the *only* thing a view is allowed to change. It fails on
    /// any of the four places the arithmetic could reach for `self.text` instead.
    ///
    /// The view is made by `erb::ruby_view` rather than by a hand-written analogue, because the
    /// property this rests on is that function's and asserting it against a local imitation
    /// would be asserting the imitation.
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

        // Non-vacuous, and the defect itself: the view addressed as its own text. The second
        // line's markup is seven units short of its bytes —
        // one for the accent, two for the emoji, four for the two CJK characters — so the caret
        // the client put on `story` arrives seven bytes to the left of it.
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

    // -----------------------------------------------------------------------
    // Properties
    //
    // Everything above is a fixture, and this file was already at 100% of lines and branches
    // without any of what follows — which says every line ran, not that every *sequence* of
    // edits produces the right buffer. There is no fixture for that: the input is a list whose
    // every element is interpreted against the text the one before it left behind, so what
    // needs enumerating is not a string but a history. The failure mode is the worst this crate
    // has, worse than a wrong answer — a buffer that quietly stops matching the file the user
    // is typing in, and every span computed from it thereafter pointing at the wrong bytes.
    // -----------------------------------------------------------------------

    /// Pieces a generated buffer is built from.
    ///
    /// The alphabet is the point rather than the length: an accent (2 bytes, 1 UTF-16 unit), CJK
    /// (3/1), an emoji (4/2 — a surrogate pair), a combining mark (a character that is not a
    /// grapheme), and the three line terminators including a lone `\r`, which is what puts a
    /// `\r\n` next to text that is not one. Concatenating pieces rather than generating
    /// arbitrary `String`s is also what makes a failure legible: proptest shrinks the *list*,
    /// so a counterexample arrives as the few pieces that still reproduce it.
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

    /// The nearest offset at or before `at` that an LSP position can actually name.
    ///
    /// Two things disqualify one: not being a character boundary, and being the `\n` of a
    /// `\r\n` — the one byte in a buffer that belongs to no line's content, so `position_at`
    /// answers with the end of the line it terminates and the trip back lands somewhere else.
    /// `offset_inside_a_crlf_reports_the_end_of_the_line_it_terminates` is that case pinned;
    /// here it is excluded, because a model that spliced at an offset with no position would be
    /// asserting the disagreement rather than the conversion.
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
        /// This is the whole of incremental sync, stated once. The subject converts byte spans
        /// out to `Range`s and the client's `Range`s back to byte spans, over a line index it
        /// rebuilds after every edit; the model does `replace_range` and knows nothing about
        /// lines, columns or encodings. They are allowed to agree only if every one of those
        /// conversions is exact — and they have to keep agreeing as the text underneath them
        /// changes, which is what a list buys over a single edit.
        #[test]
        fn a_change_list_lands_where_plain_string_splices_land(
            start in text(),
            encoding in encoding(),
            changes in prop::collection::vec((any::<usize>(), any::<usize>(), text()), 0..8),
        ) {
            let mut model = start.clone();
            let mut doc = TextDocument::new(start, encoding);

            for (first, second, replacement) in changes {
                // Chosen against the text as it stands *now*, which is what makes this a
                // history rather than a batch: an index generated up front would address a
                // buffer that the previous change has already moved.
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
        /// Positions arrive from outside and are not required to be sane — a line past the end
        /// of the buffer, a column in the middle of an emoji, `u32::MAX`. Every one of them has
        /// to come back in range and on a character boundary, because the next thing that
        /// happens to the answer is `replace_range`, and `String` panics on either mistake.
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
        /// The same property `round_trips_every_addressable_char_boundary` asserts over eleven
        /// hand-written strings, over generated ones instead — and exhaustively within each, so
        /// a generated buffer is a whole family of assertions rather than one.
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
    }
}
