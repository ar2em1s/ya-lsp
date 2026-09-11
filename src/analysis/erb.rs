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
    use crate::analysis::testing::*;
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

    /// Every request ya-lsp answers, asked at one cursor, drawn as what came back.
    ///
    /// The three that take no cursor take what the cursor produced — the item
    /// `prepareTypeHierarchy` returned and the first row `completion` offered — because asking
    /// them with something from anywhere else would be asking a different question.
    fn answers(
        harness: &mut Harness,
        uri: &DocUri,
        position: &serde_json::Value,
    ) -> Vec<(&'static str, String)> {
        let document = serde_json::json!({ "uri": uri.as_str() });
        let at = serde_json::json!({ "textDocument": document, "position": position });
        let mut drawn = Vec::new();

        for (method, params) in [
            (
                "textDocument/documentSymbol",
                serde_json::json!({ "textDocument": document }),
            ),
            ("textDocument/hover", at.clone()),
            ("textDocument/definition", at.clone()),
            (
                "textDocument/references",
                serde_json::json!({
                    "textDocument": document,
                    "position": position,
                    "context": { "includeDeclaration": false },
                }),
            ),
            ("textDocument/documentHighlight", at.clone()),
            (
                "textDocument/selectionRange",
                serde_json::json!({ "textDocument": document, "positions": [position] }),
            ),
            (
                "textDocument/foldingRange",
                serde_json::json!({ "textDocument": document }),
            ),
            (
                "textDocument/semanticTokens/full",
                serde_json::json!({ "textDocument": document }),
            ),
            ("workspace/symbol", serde_json::json!({ "query": "title" })),
            ("textDocument/signatureHelp", at.clone()),
            ("textDocument/prepareRename", at.clone()),
            (
                "textDocument/rename",
                serde_json::json!({
                    "textDocument": document,
                    "position": position,
                    "newName": "Article",
                }),
            ),
            ("textDocument/completion", at.clone()),
        ] {
            let answer = harness.ask(method, params);
            drawn.push((method, shape(&answer)));
        }

        // The type hierarchy, and the row a completion list would resolve: three requests whose
        // input is another request's output.
        let prepared = harness.ask("textDocument/prepareTypeHierarchy", at.clone());
        drawn.push(("textDocument/prepareTypeHierarchy", shape(&prepared)));
        let item = prepared.as_array().and_then(|items| items.first()).cloned();
        for method in ["typeHierarchy/supertypes", "typeHierarchy/subtypes"] {
            let answer = match &item {
                Some(item) => harness.ask(method, serde_json::json!({ "item": item })),
                None => serde_json::Value::Null,
            };
            drawn.push((method, shape(&answer)));
        }

        let offered = harness.ask("textDocument/completion", at);
        let row = offered["items"].as_array().and_then(|items| items.first());
        let resolved = match row {
            Some(row) => harness.ask("completionItem/resolve", row.clone()),
            None => serde_json::Value::Null,
        };
        drawn.push((
            "completionItem/resolve",
            match resolved {
                serde_json::Value::Null => "\u{2014}".to_owned(),
                _ => "yes".to_owned(),
            },
        ));

        drawn
    }

    /// The whole protocol, asked twice in one template: once inside a tag, once in the markup.
    ///
    /// One table rather than seventeen assertions, for the reason `GALLERY` is one document:
    /// what has to be legible is *where the answers stop*, and a per-request assertion cannot
    /// show it. The markup column is the finding — eight of the nine positional requests need no
    /// template-awareness at all, because blanked markup holds no identifier and they already
    /// answer nothing. Only `completion` needed a gate, and only `foldingRange` is declined.
    #[test]
    fn every_request_asked_inside_a_tag_and_in_the_markup_beside_it() {
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", STORY);
        let view = harness.write("app/views/stories/index.html.erb", VIEW);
        harness.index();

        // Three characters into `Story`, so that `completion` has a half-typed word to
        // complete and the row it offers is a real one — the same caret every other request
        // here is asked at.
        let inside = position_of(VIEW, "ry::TAGLINE");
        let markup = position_of(VIEW, "Stories</h1>");
        let mut table = vec![format!("{:<36}{:>10}{:>10}", "", "in <% %>", "in markup")];
        for ((method, ruby), (_, html)) in answers(&mut harness, &view, &inside)
            .into_iter()
            .zip(answers(&mut harness, &view, &markup))
        {
            table.push(format!("{method:<36}{ruby:>10}{html:>10}"));
        }

        assert_eq!(
            table.join("\n"),
            "                                      in <% %> in markup\n\
             textDocument/documentSymbol                  —         —\n\
             textDocument/hover                         yes         —\n\
             textDocument/definition                      1         —\n\
             textDocument/references                      1         —\n\
             textDocument/documentHighlight               1         —\n\
             textDocument/selectionRange                  1         1\n\
             textDocument/foldingRange                    —         —\n\
             textDocument/semanticTokens/full             4         4\n\
             workspace/symbol                             1         1\n\
             textDocument/signatureHelp                   —         —\n\
             textDocument/prepareRename                 yes         —\n\
             textDocument/rename                        yes         —\n\
             textDocument/completion                      1         —\n\
             textDocument/prepareTypeHierarchy            1         —\n\
             typeHierarchy/supertypes                     —         —\n\
             typeHierarchy/subtypes                       —         —\n\
             completionItem/resolve                     yes         —"
        );
    }

    #[test]
    fn the_walk_indexes_a_template_nobody_opened_and_its_calls_are_references() {
        // The decision this test exists for, and the one that was reversed twice while it was
        // being made. Indexing only the templates the editor has open would pass every other
        // ERB test here and still be wrong: `references` would be complete or incomplete
        // depending on which tabs happened to be open, which is worse than a consistently
        // narrow answer.
        let mut harness = Harness::new();
        let model = harness.write("app/models/story.rb", STORY);
        harness.write("app/views/stories/index.html.erb", VIEW);
        harness.index();

        assert_eq!(
            harness.reference_list(&model, STORY, "title", true),
            ["story.rb:3:6", "index.html.erb:2:15"]
        );
    }

    #[test]
    fn a_template_reaching_the_graph_raw_would_record_no_references_at_all() {
        // Why the blanking is the feature rather than an optimisation. The same template under
        // an extension nothing recognises is read as Ruby, gives up in the first tag, and the
        // call sites simply are not there, which is the whole of what indexing a template
        // buys.
        let mut harness = Harness::new();
        let model = harness.write("app/models/story.rb", STORY);
        harness.write("app/views/stories/index.html.rhubarb", VIEW);
        harness.index();

        assert_eq!(
            harness.reference_list(&model, STORY, "title", true),
            ["story.rb:3:6"]
        );
    }

    #[test]
    fn a_template_changed_on_disk_is_re_read_through_the_same_blanking() {
        // The watcher's route into the graph. `index_buffer` is the hook `didOpen`, `didChange`
        // and `didChangeWatchedFiles` all share, so a template that reached the graph raw
        // through any one of them would replace its own call sites with parse errors — the same
        // rule `.rbs` interfaces are held to, and for the same reason.
        let mut harness = Harness::new();
        let model = harness.write("app/models/story.rb", STORY);
        let view = harness.write("app/views/stories/index.html.erb", "<h1>none</h1>\n");
        harness.index();
        assert_eq!(
            harness.reference_list(&model, STORY, "title", true),
            ["story.rb:3:6"]
        );

        harness.write("app/views/stories/index.html.erb", VIEW);
        harness.watch(&[&view]);
        harness.analysis.settle();

        assert_eq!(
            harness.reference_list(&model, STORY, "title", true),
            ["story.rb:3:6", "index.html.erb:2:15"]
        );
    }

    #[test]
    fn a_template_publishes_no_diagnostics_and_a_ruby_file_beside_it_still_does() {
        // What survives a correct scan is not about anything the user wrote: `<%= yield %>` in
        // a layout, which is legal in the method a template compiles to and refused by a parser
        // reading a file. Two of them over lobsters' 121 templates. A rule that fires on correct
        // input does not earn a squiggle.
        let mut harness = Harness::new();
        let broken = harness.write("app/models/story.rb", "class Story\n  def title\nend\n");
        let view = harness.write(
            "app/views/stories/index.html.erb",
            "<h1>Stories</h1>\n<% end %>\n<%= yield :head %>\n",
        );
        harness.index();

        // One drain, because reading the stream empties it: two `latest` calls would make the
        // second one answer `None` for a document that did publish.
        let published = harness.published();
        let for_uri = |uri: &DocUri| {
            published
                .iter()
                .filter(|(sent, _)| sent == uri.as_str())
                .count()
        };
        assert_eq!(for_uri(&view), 0, "{published:?}");
        assert_eq!(for_uri(&broken), 1, "{published:?}");
    }

    #[test]
    fn folding_is_declined_in_a_template_so_the_editor_keeps_its_own_guess() {
        // The walk sees the Ruby and nothing else, so what it offers is folds for the `<% %>`
        // blocks and none for the markup around them. `ranges.md` wrote the mechanism down
        // before ERB was on the table: a client that has a folding provider stops guessing from
        // indentation, so an empty array takes the fallback away *and* puts nothing in its
        // place, while a `null` can only hand it back.
        let mut harness = Harness::new();
        let view = harness.write("app/views/stories/index.html.erb", VIEW);
        let ruby = harness.write("app/models/story.rb", STORY);
        harness.index();

        let folds = |harness: &mut Harness, uri: &DocUri| {
            harness.ask(
                "textDocument/foldingRange",
                serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
            )
        };
        assert!(folds(&mut harness, &view).is_null());
        // The control, and it is not decoration: the same template's Ruby *does* fold, so this
        // is a decision rather than an absence of anything to offer.
        assert!(!folds(&mut harness, &ruby).is_null());
    }
}
