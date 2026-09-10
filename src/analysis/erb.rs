//! ERB templates: the Ruby view of one, and where in one the Ruby is.
//!
//! A template is half Ruby and half markup, and rubydex indexes Ruby. The technique here is the
//! one [`signatures::without_interfaces`](super::signatures::without_interfaces) and
//! [`Finder::without_the_half_typed_call`](super::cursor) already use: **replace the bytes that
//! are not wanted with spaces, one space per byte, keeping the newlines**. What comes out is the
//! same length as what went in with its newlines in the same places, so every offset rubydex
//! records for the Ruby is an offset into the *template*, and every request that takes a cursor
//! works unchanged.
//!
//! The alternative — extracting the Ruby into a buffer of its own and keeping a position map — is
//! a second coordinate system, and every request would have to be right in both.
//!
//! # Padding is by byte, not by character
//!
//! The natural spelling is `" " * char.length`, in characters, because Ruby strings are
//! character-indexed. ya-lsp is UTF-8-byte-offset end to end (`core-invariants.md`), so padding
//! that way shortens the buffer by one byte per accent and three per emoji in the markup above
//! the cursor, and every Ruby offset below it lands wrong. It fails silently and only for the
//! users who do not write in English, which is the worst direction for a bug to fail in.
//!
//! # Three details of the scanner are load-bearing
//!
//! **A comment tag is blanked whole.** Blanking only the `#` leaves the note as prose at
//! statement position. *Keeping* the `#`, so `<%# … %>` becomes a Ruby comment, is right for the
//! first line and wrong for every line after it — a Ruby comment ends at the newline, so the rest
//! of a multi-line tag is copied back as code, and the errors it reports are English words read
//! as Ruby keywords. So `#` is a sigil, and the sigil means the tag is not Ruby at all.
//! `<% # … %>` is a different thing and stays code: there the `#` is Ruby's own comment marker
//! inside an ordinary tag.
//!
//! **The closer becomes `;`.** Two `<%= %>` on one line are two statements, and with `%>` blanked
//! to spaces they run together into one expression that does not parse. Writing a semicolon over
//! the `%` is still byte-for-byte and is what makes `<%= a %> and <%= b %>` legal. `-%>` and
//! `=%>` need the trailing sigil blanked with it, or the `-` is left as a Ruby operator with
//! nothing after it.
//!
//! **`<%%` opens nothing.** It is ERB's escape for a literal `<%`; read as a tag it swallows the
//! markup up to the next `%>` and turns it into Ruby.
//!
//! # Why there are no diagnostics here
//!
//! What the Ruby view cannot make legal is `<%= yield :subnav %>` in a layout. A compiled Rails
//! template *is* a method body, so `yield` is legal there and Prism — reading a file — is right
//! to refuse it. There is no length-preserving edit that makes it legal, and there does not need
//! to be: it is not something the user wrote wrongly, which is `diagnostics.rs`'s own test for
//! whether a rule earns a squiggle. `Analysis::collect_diagnostics` drops a template's.

use std::{ops::Range, path::Path};

use crate::workspace::uri::DocUri;

/// The extensions that mean ERB.
///
/// `show.html.erb` and `index.js.erb` both end in `erb`; `.rhtml` is what Rails 1 called the same
/// thing and templates named that way are still in the wild.
const EXTENSIONS: [&str; 2] = ["erb", "rhtml"];

/// Whether `path` names an ERB template.
#[must_use]
pub fn is_template(path: &Path) -> bool {
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| EXTENSIONS.contains(&extension))
}

/// The same question asked of a document key.
///
/// Delegates rather than testing the URI's own suffix, for the reason `Workspace::indexes` and
/// `Workspace::discover` share their globs: the walk that blanks a template and the request path
/// that decides one is open must not be able to disagree about which files are templates.
#[must_use]
pub fn is_template_uri(uri: &DocUri) -> bool {
    uri.to_path().is_some_and(|path| is_template(&path))
}

/// The Ruby view of `template`: every byte of markup replaced with a space, every newline kept.
///
/// The same length and the same line breaks as the input, so the offsets rubydex records are the
/// template's own.
#[must_use]
pub fn ruby_view(template: &str) -> String {
    let bytes = template.as_bytes();
    let mut view = String::with_capacity(template.len());
    let mut at = 0;

    for tag in tags(template) {
        // The markup before the tag, plus `<%` and any leading sigil.
        blank(&mut view, &bytes[at..tag.body.start]);
        if tag.comment {
            blank(&mut view, &bytes[tag.body.clone()]);
        } else {
            view.push_str(&template[tag.body.clone()]);
        }
        match tag.closer {
            Some(closer) => {
                // A trailing `-` or `=`, when there was one. It sits outside the body.
                blank(&mut view, &bytes[tag.body.end..closer]);
                view.push(';');
                at = closer + 1;
            }
            // A tag the file ends inside of — which is what a half-typed one looks like.
            None => at = tag.body.end,
        }
    }
    blank(&mut view, &bytes[at..]);

    view
}

/// Whether the byte offset `at` sits in Ruby rather than in markup.
///
/// The one thing the blanked view cannot answer on its own: markup becomes spaces, and Ruby
/// contains spaces too. Both ends are inclusive because a cursor sits *between* bytes — the
/// caret immediately after `<%=` and the one immediately before `%>` are both in the Ruby.
///
/// Exactly one request asks. Eight of the nine other positional requests need nothing: blanked
/// markup holds no identifier, so they already answer `null` there. `completion` is the exception
/// because it does not need a token under the cursor to be meaningful — without this it offers
/// the workspace's constants to someone typing prose in an `<h1>`.
#[must_use]
pub fn in_ruby(template: &str, at: usize) -> bool {
    tags(template).any(|tag| !tag.comment && tag.body.start <= at && at <= tag.body.end)
}

/// One `<% … %>`, located.
struct Tag {
    /// The Ruby: after the opener and its sigils, before the closer and its own.
    body: Range<usize>,
    /// Where `%>` begins, or `None` when the file ends inside the tag.
    closer: Option<usize>,
    /// Whether the sigil was `#`, which makes the whole tag a comment.
    comment: bool,
}

/// The tags of `template`, in order.
///
/// An iterator rather than a `Vec` so [`in_ruby`] can stop at the tag it is looking for, and so
/// there is one scanner rather than two that have to agree.
fn tags(template: &str) -> Tags<'_> {
    Tags {
        bytes: template.as_bytes(),
        at: 0,
    }
}

struct Tags<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Iterator for Tags<'_> {
    type Item = Tag;

    fn next(&mut self) -> Option<Tag> {
        while self.at < self.bytes.len() {
            if !self.bytes[self.at..].starts_with(b"<%") {
                self.at += 1;
                continue;
            }
            // `<%%` is ERB's escape for a literal `<%` and opens nothing. Stepping over the
            // whole escape rather than one byte is what stops the second `%` being read as the
            // start of a tag that runs to the next `%>`, taking the markup between them with it.
            if self.bytes[self.at..].starts_with(b"<%%") {
                self.at += 3;
                continue;
            }

            let mut start = self.at + 2;
            while matches!(self.bytes.get(start), Some(b'=' | b'-')) {
                start += 1;
            }
            // `#` is the third sigil and the only one that changes what the tag *is*: `<%# … %>`
            // is a comment, and none of it is Ruby. `<% # … %>` is not one — there the `#` is
            // Ruby's own comment marker inside an ordinary tag, and everything after the next
            // newline is code again.
            let comment = self.bytes.get(start) == Some(&b'#');
            if comment {
                start += 1;
            }

            let mut end = start;
            while end < self.bytes.len() && !self.bytes[end..].starts_with(b"%>") {
                end += 1;
            }
            if end == self.bytes.len() {
                self.at = end;
                return Some(Tag {
                    body: start..end,
                    closer: None,
                    comment,
                });
            }

            let closer = end;
            if end > start && matches!(self.bytes[end - 1], b'-' | b'=') {
                end -= 1;
            }
            self.at = closer + 2;
            return Some(Tag {
                body: start..end,
                closer: Some(closer),
                comment,
            });
        }
        None
    }
}

/// One space per byte, except newlines, which are what hold the line numbers.
///
/// Pushed a byte at a time rather than built as a `Vec<u8>` and converted: everything appended
/// here is ASCII and everything else appended is a whole slice of the input, so the result is
/// valid UTF-8 by construction and there is no error arm that cannot happen.
fn blank(view: &mut String, bytes: &[u8]) {
    for byte in bytes {
        view.push(if *byte == b'\n' { '\n' } else { ' ' });
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// The shapes a real template is made of, in one file.
    const TEMPLATE: &str = "\
<h1>Stories</h1>
<% @stories.each do |story| %>
  <p><%= story.title %> by <%= story.user.username %></p>
  <%# a note, and it must not eat the line %>
  <%- if story.hot? -%>
    <span>hot</span>
  <% end %>
<% end %>
";

    #[test]
    fn every_extension_that_means_erb_and_nothing_else() {
        for name in [
            "show.html.erb",
            "index.js.erb",
            "mailer.erb",
            "legacy.rhtml",
        ] {
            assert!(is_template(Path::new(name)), "{name}");
        }
        for name in ["story.rb", "core.rbs", "erb", "views/erb/story.rb"] {
            assert!(!is_template(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn a_document_key_answers_the_same_question_as_the_path_it_came_from() {
        let path = std::path::PathBuf::from("/tmp/app/views/stories/show.html.erb");
        let uri = DocUri::from_path(&path).expect("an absolute path");
        assert!(is_template_uri(&uri));

        let ruby = DocUri::from_path(Path::new("/tmp/app/models/story.rb")).expect("absolute");
        assert!(!is_template_uri(&ruby));
    }

    #[test]
    fn the_markup_goes_and_the_ruby_stays_where_it_was() {
        let view = ruby_view(TEMPLATE);

        assert!(!view.contains("Stories</h1>"), "{view}");
        assert!(!view.contains("<span>"), "{view}");
        for kept in [
            "@stories.each do |story|",
            "story.title",
            "story.user.username",
        ] {
            assert_eq!(view.find(kept), TEMPLATE.find(kept), "{kept}\n{view}");
        }
    }

    #[test]
    fn a_comment_tag_is_blanked_whole_and_a_comment_inside_a_tag_is_not() {
        // The line after the first is what decides this. Keeping the `#` makes the note a Ruby
        // comment, which ends at the newline — so `for the webmentions support` on the second
        // line of a four-line `<%# … %>` is parsed as a `for` loop with no `in`. One such partial
        // accounts for most of the parse errors that spelling produces over a real application.
        let view = ruby_view("<%# a note\nfor the webmentions support\n%>\n");
        assert!(!view.contains("note"), "{view}");
        assert!(!view.contains("for the"), "{view}");
        assert!(prism_parses(&view), "{view}");

        // `<% # … %>` is not a comment tag. The `#` is Ruby's, inside an ordinary tag, and what
        // follows the newline is code again — so nothing here may be blanked.
        let view = ruby_view("<%\n  # a note\n  x = 1\n%>\n");
        assert!(view.contains("x = 1"), "{view}");
    }

    #[test]
    fn a_comment_tag_is_not_a_place_to_offer_completions() {
        const NOTE: &str = "<%# a note %>";
        assert!(!in_ruby(NOTE, NOTE.find("note").expect("the note")));
    }

    #[test]
    fn two_expressions_on_one_line_are_two_statements() {
        let view = ruby_view("<%= a %> and <%= b %>");
        assert_eq!(view, "    a ;          b ; ");
        assert!(prism_parses(&view), "{view}");
    }

    #[test]
    fn a_trailing_trim_sigil_is_blanked_with_the_closer() {
        // Left behind, the `-` is a Ruby operator with nothing after it: "expected an expression
        // after the operator", three times over the corpus.
        let view = ruby_view("<%- if a -%>\n<% end %>\n");
        assert!(prism_parses(&view), "{view}");
        assert_eq!(view.len(), "<%- if a -%>\n<% end %>\n".len());
    }

    #[test]
    fn the_escape_for_a_literal_opener_opens_nothing() {
        // `<%%` renders a literal `<%`. Read as a tag, it swallows the markup up to the next
        // `%>` — here, the whole of `= wrong %>`.
        let view = ruby_view("<%% not ruby %> <%= right %>");
        assert!(!view.contains("not ruby"), "{view}");
        assert_eq!(
            view.find("right"),
            "<%% not ruby %> <%= right %>".find("right")
        );
    }

    #[test]
    fn a_tag_the_file_ends_inside_of_still_yields_its_ruby() {
        // What a half-typed tag looks like, and completion has to work in it.
        let view = ruby_view("<p><%= story.");
        assert_eq!(view, "       story.");
        assert!(in_ruby("<p><%= story.", "<p><%= story.".len()));
    }

    #[test]
    fn an_empty_tag_is_not_a_special_case() {
        assert_eq!(ruby_view("<% %>"), "   ; ");
        assert_eq!(ruby_view("<%=%>"), "   ; ");
    }

    #[test]
    fn where_the_ruby_is_and_where_it_stops() {
        const LINE: &str = "<p><%= story.title %></p>";
        let at = |needle: &str| LINE.find(needle).expect(needle);

        assert!(!in_ruby(LINE, at("<p>")), "the markup before");
        assert!(!in_ruby(LINE, at("<%=")), "the opener");
        // Immediately after the sigil, and immediately before the closer: both are carets a user
        // can put down inside the tag.
        assert!(in_ruby(LINE, at("<%=") + 3));
        assert!(in_ruby(LINE, at(" %>")));
        // The caret between the space and the `%` is still in the tag — it is where someone who
        // has just typed `story.title` is standing.
        assert!(in_ruby(LINE, at(" %>") + 1));
        assert!(!in_ruby(LINE, at(" %>") + 2), "inside the closer");
        assert!(!in_ruby(LINE, at("</p>")), "the markup after");
        assert!(!in_ruby(LINE, LINE.len()), "past the end");
    }

    #[test]
    fn a_multi_byte_character_above_the_cursor_does_not_move_it() {
        // The failure a character-padded port has and this does not. Both templates hold the
        // same Ruby; only the markup above it differs.
        const ASCII: &str = "<h1>hot</h1>\n<%= story.title %>\n";
        const WIDE: &str = "<h1>\u{433}\u{43e}\u{440}\u{44f}\u{447}\u{438}\u{439} \u{1f525}</h1>\n<%= story.title %>\n";

        assert_eq!(ruby_view(ASCII).len(), ASCII.len());
        assert_eq!(ruby_view(WIDE).len(), WIDE.len());
        assert_eq!(
            ruby_view(WIDE).find("story.title"),
            WIDE.find("story.title")
        );
        assert_eq!(
            ruby_view(ASCII).find("story.title"),
            ASCII.find("story.title")
        );
    }

    #[test]
    fn what_comes_out_is_something_prism_still_reads() {
        assert!(
            prism_parses(&ruby_view(TEMPLATE)),
            "{}",
            ruby_view(TEMPLATE)
        );
    }

    fn prism_parses(source: &str) -> bool {
        ruby_prism::parse(source.as_bytes()).errors().count() == 0
    }

    // -----------------------------------------------------------------------
    // The property, for the reason `position.rs` has properties: what needs enumerating is not
    // a template but the space of them, and every failure this can have is invisible in ASCII.
    // -----------------------------------------------------------------------

    /// The pieces a generated template is built from.
    ///
    /// `position.rs`'s alphabet — an accent, CJK, an emoji, a combining mark, and all three line
    /// terminators — plus the ERB punctuation, so that a shrunk counterexample arrives as the few
    /// pieces that still reproduce it rather than as a wall of markup.
    const PIECES: &[&str] = &[
        "<%",
        "%>",
        "<%=",
        "<%-",
        "<%#",
        "<%%",
        "-%>",
        "a",
        " ",
        "\n",
        "\r\n",
        "\r",
        "<h1>",
        "</h1>",
        "x = 1",
        "caf\u{e9}",
        "\u{65e5}\u{672c}",
        "\u{1f600}",
        "e\u{301}",
    ];

    fn template() -> impl Strategy<Value = String> {
        prop::collection::vec(prop::sample::select(PIECES), 0..24)
            .prop_map(|pieces| pieces.concat())
    }

    proptest! {
        /// The whole technique, stated once: same bytes, same lines, same places.
        ///
        /// Every offset rubydex records for a template is an offset into the file the editor has
        /// open. That holds only if blanking moves nothing — so the view has to be the same
        /// length as the source and have its newlines at exactly the same byte offsets. A
        /// character-padded port fails the first of these on any template with a non-ASCII
        /// character in its markup, and nothing else in the suite would see it.
        #[test]
        fn blanking_moves_no_byte_and_no_line(template in template()) {
            let view = ruby_view(&template);

            prop_assert_eq!(view.len(), template.len());
            prop_assert_eq!(
                view.match_indices('\n').map(|(at, _)| at).collect::<Vec<_>>(),
                template.match_indices('\n').map(|(at, _)| at).collect::<Vec<_>>()
            );
        }

        /// Every byte the view kept is a byte the template had, at that offset.
        ///
        /// The complement of the length property, and what makes "the Ruby is where it was" true
        /// rather than merely plausible: the only bytes this may write are a space, a newline the
        /// source already had there, and the `;` over a closer's `%`.
        #[test]
        fn the_view_only_ever_blanks(template in template()) {
            let view = ruby_view(&template);
            for (at, (kept, original)) in view.bytes().zip(template.bytes()).enumerate() {
                prop_assert!(
                    kept == original || kept == b' ' || (kept == b';' && original == b'%'),
                    "byte {at}: {kept:?} for {original:?}"
                );
            }
        }

        /// A cursor in Ruby is a cursor the view did not blank.
        ///
        /// The two answers have to be one answer: `completion` decides from [`in_ruby`] and then
        /// completes against the text [`ruby_view`] produced, and a disagreement between them is
        /// a completion computed from a receiver that is not there.
        #[test]
        fn in_ruby_agrees_with_what_the_view_kept(template in template()) {
            let view = ruby_view(&template);
            for (at, (kept, original)) in view.bytes().zip(template.bytes()).enumerate() {
                if kept == original && original != b' ' && original != b'\n' {
                    prop_assert!(in_ruby(&template, at), "byte {at} of {template:?}");
                }
            }
        }
    }
}
