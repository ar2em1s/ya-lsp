//! The LSP request layer: one handler per method, and the table that picks it.
//!
//! Every handler opens the same four lines — [`parse_params`], the position, `DocUri::from_lsp`,
//! and one of [`Analysis::with_text`] / [`Analysis::read_of`] — then calls one analysis module and
//! shapes what comes back into the type `lsp-types` names. Nothing here decides anything about
//! Ruby; everything here decides what a client is allowed to be told. That uniformity is what made
//! this a move rather than a redesign, and no signature changed in it.
//!
//! Those four lines are deliberately **not** a macro and not a trait. They differ in the params
//! type, and a handler that reads top to bottom is worth more than four lines saved twenty-four
//! times.
//!
//! [`Analysis::serve`] is the entry point and the only item the thread calls. It carries the
//! bulkhead, the settle policy and the deferred retry, because all three are properties of
//! answering *a request* rather than of any one method.
//!
//! The rule the whole file obeys is [`reply`]: an absent answer is `null`, never an empty list.

use std::{collections::HashMap, time::Instant};

use lsp_server::{ErrorCode, Request, RequestId, Response};
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, CodeAction,
    CodeActionKind, CodeActionOrCommand, CompletionItem, CompletionItemKind, CompletionItemTag,
    CompletionList, CompletionResponse, CompletionTextEdit, DocumentHighlight, DocumentLink,
    DocumentSymbolResponse, Documentation, FoldingRange, GotoDefinitionResponse, Hover,
    HoverContents, InlayHint, InlayHintKind, InlayHintLabel, InlayHintTooltip, Location,
    LocationLink, MarkupContent, MarkupKind, OneOf, OptionalVersionedTextDocumentIdentifier,
    PrepareRenameResponse, SelectionRange, SemanticToken, SemanticTokens, SignatureHelp,
    SymbolInformation, TextDocumentEdit, TextEdit, TypeHierarchyItem, WorkspaceEdit,
    WorkspaceSymbolResponse,
};
use rubydex::model::ids::{DeclarationId, UriId};

use super::{
    Analysis, MAX_COMPLETION_ITEMS, MAX_INCOMING_CALLS, MAX_REFERENCES, MAX_SUBTYPES,
    MAX_UNTYPED_CANDIDATES, MAX_UNTYPED_COMPLETION_ITEMS, MAX_WORKSPACE_SYMBOLS, code_actions,
    completion, cursor, environment, erb, hierarchy, highlight, hints, hover, locator,
    locator::Site,
    position::{ByteSpan, TextDocument},
    ranges, references, rename, requires, search, signature_help, symbols, tokens, types,
};
use crate::messages;
use crate::workspace::DocUri;

impl Analysis {
    pub(super) fn serve(&mut self, request: Request) {
        // **The one line that says a request arrived**, and it is the reason this module logs at
        // all. Twenty-one of the twenty-four methods used to answer in complete silence, so a
        // report saying "hover does nothing in this file" could not be told apart from one where
        // the client never sent a hover — which is what a document selector one folder too narrow
        // actually does, and what cost the diagnosis that found it.
        //
        // Written here rather than per method because `serve` is the one door all 24 go through.
        // Fields, not prose: a reader greps `method=textDocument/hover` and gets the pair.
        let arrived = Instant::now();
        let (document, position) = asked_about(&request.params);
        tracing::debug!(
            method = request.method,
            id = %request.id,
            document,
            position,
            dirty = self.dirty,
            "request"
        );

        if self.cancellations.take(&request.id) {
            // Answered in the same shape as every other outcome rather than in a sentence of its
            // own: a reader following one id through the log gets the same two lines whatever
            // happened to it, and `cancelled` is a normal thing for a client to do.
            let elapsed = format!("{:.2?}", arrived.elapsed());
            tracing::debug!(
                method = request.method,
                id = %request.id,
                outcome = "cancelled",
                settled = false,
                retried = false,
                elapsed,
                "answered"
            );
            self.respond(Response::new_err(
                request.id,
                ErrorCode::RequestCanceled as i32,
                "request cancelled by the client".to_owned(),
            ));
            return;
        }

        let id = request.id.clone();
        let method = request.method.clone();
        // The bulkhead's third seam, on the *request* path: eight lines of ordinary Ruby — a
        // class reopened under a constant that aliases it — reach an unwrap in
        // `find_self_receiver_declaration` from `textDocument/definition`, and neither the
        // indexing bulkhead nor `resolve`'s guard covers that path.
        //
        // The failure unit here is the request: it answers nothing and says so. `settle` is
        // inside the guard rather than in front of it because everything it does is reachable
        // the same way — a request that arrives dirty pays for the pass and the link, and only
        // one of the three has a guard of its own.
        //
        // A settle that crashes has already cleared `dirty`, so the next request answers against
        // the graph as it stands rather than settling again. That is deliberate and it is the
        // policy `recovering` lands on one level down: retrying per request would settle, crash
        // and re-arm on every keystroke. Something that changes the workspace arms it again.
        let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Answer against a settled graph: a stale answer is worse than a slightly slower
            // one. This is `dirty`, not `resolve_at`, because background gem indexing
            // deliberately does not arm the timer — but its files are still unlinked until
            // something resolves them.
            //
            // Two requests are exempt, because their answer does not come from the graph at
            // all; waiting for a resolve they never read is a cost paid on every keystroke, and
            // on a large file it is two orders of magnitude.
            //
            // The third is **deferred rather than exempt**, and the retry below is the whole
            // difference between the two words.
            let deferred = self.dirty && defers(&method);
            let mut settled = false;
            if self.dirty && needs_the_graph(&method) && !defers(&method) {
                self.settle();
                settled = true;
            }
            let again = deferred.then(|| request.params.clone());
            let response = self.dispatch(&id, request);
            // **A `Rebase` is an optimization and not a filter.** It refuses an offset that
            // lands inside text the graph has not been given — a constant just typed is very
            // nearly the whole of that set, because `self`, a local and an instance variable
            // are all resolved against the buffer — and from here a refusal cannot be told
            // apart from a cursor with nothing to complete. Both are repaired the same way:
            // settle, and ask again with the two coordinate systems back in step.
            //
            // So the deferred path answers *sooner* than the eager one and never answers
            // *less*, which is the property that makes it safe to leave on. Without this it
            // trades a 45 ms answer for a 1 ms empty list, which is not a trade anybody wants.
            match again {
                Some(params) if answered_nothing(&response) => {
                    self.settle();
                    let response = self.dispatch(
                        &id,
                        Request {
                            id: id.clone(),
                            method: method.clone(),
                            params,
                        },
                    );
                    (response, true, true)
                }
                _ => (response, settled, false),
            }
        }));

        let (response, settled, retried) = match served {
            Ok(answered) => answered,
            Err(_) => {
                // Not a `messages::` sentence: this is addressed to the client and answers one
                // request, which is the boundary `messages.md` draws. The panic hook has
                // already put rubydex's file and line on stderr.
                tracing::error!("answering {method} crashed; that request answers nothing");
                (
                    Response::new_err(
                        id.clone(),
                        ErrorCode::InternalError as i32,
                        format!("ya-lsp crashed while answering {method}"),
                    ),
                    false,
                    false,
                )
            }
        };

        // The other half of the pair, and the four fields that answer *what happened to it*: how
        // long, whether the graph had to be linked first, whether this is the deferred retry
        // rather than the first attempt, and which of the five things the client was told.
        // `nothing` against `empty` is kept apart deliberately — one is a cursor on nothing and
        // the other is a list that matched nothing, and they are repaired in different places.
        let outcome = outcome(&response);
        let elapsed = format!("{:.2?}", arrived.elapsed());
        tracing::debug!(
            method = method,
            id = %id,
            outcome,
            settled,
            retried,
            elapsed,
            "answered"
        );

        self.cancellations.forget(&id);
        self.respond(response);
    }

    /// Which handler answers `request`. Split out of [`Analysis::serve`] only so that the
    /// bulkhead there is one expression rather than a closure wrapped around a `match` this
    /// long.
    fn dispatch(&mut self, id: &RequestId, request: Request) -> Response {
        #[cfg(test)]
        super::crash_the_next_request_if_asked();
        match request.method.as_str() {
            "textDocument/documentSymbol" => reply(id, self.document_symbols(request.params)),
            "textDocument/hover" => reply(id, self.hover(request.params)),
            "textDocument/definition" => reply(id, self.goto_definition(request.params)),
            "textDocument/references" => reply(id, self.references(request.params)),
            "textDocument/documentHighlight" => reply(id, self.document_highlights(request.params)),
            "textDocument/selectionRange" => reply(id, self.selection_ranges(request.params)),
            "textDocument/foldingRange" => reply(id, self.folding_ranges(request.params)),
            "textDocument/documentLink" => reply(id, self.document_links(request.params)),
            "textDocument/semanticTokens/full" => reply(id, self.semantic_tokens(request.params)),
            "workspace/symbol" => reply(id, self.workspace_symbols(request.params)),
            "textDocument/prepareTypeHierarchy" => {
                reply(id, self.prepare_type_hierarchy(request.params))
            }
            "typeHierarchy/supertypes" => reply(id, self.supertypes(request.params)),
            "typeHierarchy/subtypes" => reply(id, self.subtypes(request.params)),
            "textDocument/prepareCallHierarchy" => {
                reply(id, self.prepare_call_hierarchy(request.params))
            }
            "callHierarchy/incomingCalls" => reply(id, self.incoming_calls(request.params)),
            "callHierarchy/outgoingCalls" => reply(id, self.outgoing_calls(request.params)),
            "textDocument/inlayHint" => reply(id, self.inlay_hints(request.params)),
            "inlayHint/resolve" => reply(id, self.resolve_inlay_hint(request.params)),
            "textDocument/signatureHelp" => reply(id, self.signature_help(request.params)),
            "textDocument/codeAction" => reply(id, self.code_actions(request.params)),
            "textDocument/prepareRename" => reply(id, self.prepare_rename(request.params)),
            "textDocument/rename" => reply(id, self.rename(request.params)),
            "textDocument/completion" => reply(id, self.completion(request.params)),
            "completionItem/resolve" => reply(id, self.resolve_completion(request.params)),
            method => Response::new_err(
                id.clone(),
                ErrorCode::MethodNotFound as i32,
                format!("ya-lsp does not handle {method} yet"),
            ),
        }
    }

    // -----------------------------------------------------------------------
    // Navigation
    // -----------------------------------------------------------------------

    /// `textDocument/documentSymbol`.
    fn document_symbols(&self, params: serde_json::Value) -> Option<DocumentSymbolResponse> {
        let params: lsp_types::DocumentSymbolParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;
        // One reread of this one document at most, and only if it declares a private method —
        // the outline prints the word and rubydex's record is wrong for one shape of it.
        let read = |uri: &str| self.read_of(uri);
        let modifiers = locator::Modifiers::new(&read);
        let symbols = self.with_text(&uri, |text| {
            symbols::document_symbols(&self.graph, &modifiers, UriId::from(uri.as_str()), text)
        })?;

        if symbols.is_empty() {
            return None;
        }
        Some(if self.client.hierarchical_symbols {
            DocumentSymbolResponse::Nested(symbols)
        } else {
            DocumentSymbolResponse::Flat(symbols::flatten(&symbols, &params.text_document.uri))
        })
    }

    /// `textDocument/hover`.
    fn hover(&self, params: serde_json::Value) -> Option<Hover> {
        let params: lsp_types::HoverParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let read = |uri: &str| self.read_of(uri);
        let sources = self.sources(&read);
        let modifiers = locator::Modifiers::new(&read);
        self.with_text(&uri, |text| {
            let offset = text.offset_at(position);
            // The graph may be a keystroke behind the buffer, so the cursor goes *in*
            // through the map and every span comes back *out* through it.
            let rebase = self.rebase_for(&uri, text.text());
            let at = rebase.to_graph(offset)?;
            let uri_id = UriId::from(uri.as_str());
            // The scope walk first, the same order `definition` and `documentHighlight` ask
            // it in. What an instance variable *is* is what ya-lsp derived it to be, and the
            // footnote naming the assignment is the other half of that answer.
            //
            // It falls through where nothing could be derived rather than declining, which is
            // the same rule as the `find_map` below: at a write the graph still has the
            // declaration rubydex filed for the assignment, and `Shelf::Book#@title` is a
            // better card than none.
            if let Some((variable, resolution)) =
                locator::resolve_variable(&sources, uri_id, text.text(), offset, at, &rebase)
                && let Some(markdown) = hover::markdown(
                    &self.graph,
                    &self.synthesized,
                    self.layout(),
                    &modifiers,
                    &resolution,
                    resolution
                        .derivation
                        .assignment
                        .map(|at| text.position_at(at).line + 1),
                    Some(uri.as_str()),
                )
            {
                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: markdown,
                    }),
                    range: Some(text.range_at(variable.start, variable.end)),
                });
            }
            // More than one target can share the narrowest span; take the first that has
            // something to say rather than the first that exists.
            let found = locator::locate(&self.graph, uri_id, at)
                .into_iter()
                .find_map(|located| {
                    let found = rebase.span_to_buffer(ByteSpan {
                        start: located.start,
                        end: located.end,
                    })?;
                    let (start, end) = (found.start, found.end);
                    let resolution = locator::resolve_typed(
                        &sources,
                        uri_id,
                        text.text(),
                        &located,
                        start,
                        &rebase,
                    )?;
                    // The card names a line and this is where the text is; see
                    // `hover::markdown`. The offset is the buffer's already — it is provenance
                    // `cursor` read out of the buffer, not a graph key — so it is not mapped.
                    let line = resolution
                        .derivation
                        .assignment
                        .map(|at| text.position_at(at).line + 1);
                    let markdown = hover::markdown(
                        &self.graph,
                        &self.synthesized,
                        self.layout(),
                        &modifiers,
                        &resolution,
                        line,
                        Some(uri.as_str()),
                    )?;
                    Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: markdown,
                        }),
                        range: Some(text.range_at(start, end)),
                    })
                });
            // A macro's `:symbol`, after the graph and not before it — `definition` asks in the
            // same order and for the same reason. The card is the declaration's own, with one
            // more footnote: which macro is the evidence that this symbol is a name at all.
            found.or_else(|| {
                let (symbol, resolution) =
                    locator::resolve_symbol(&self.graph, uri_id, text.text(), offset, at)?;
                let markdown = hover::markdown(
                    &self.graph,
                    &self.synthesized,
                    self.layout(),
                    &modifiers,
                    &resolution,
                    None,
                    Some(uri.as_str()),
                )?;
                Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: markdown,
                    }),
                    range: Some(text.range_at(symbol.start, symbol.end)),
                })
            })
        })?
    }

    /// `textDocument/signatureHelp`.
    ///
    /// Answered from the buffer rather than from the graph's copy of it, like completion and
    /// for the same reason: the call under the cursor is half-written by definition, and the
    /// argument the user is on is a fact about the text as it stands this keystroke.
    fn signature_help(&self, params: serde_json::Value) -> Option<SignatureHelp> {
        let params: lsp_types::SignatureHelpParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let read = |uri: &str| self.read_of(uri);
        let modifiers = locator::Modifiers::new(&read);
        self.with_text(&uri, |text| {
            let call = cursor::call_at(text.text(), text.offset_at(position))?;
            let method = locator::precise_call(
                &self.graph,
                UriId::from(uri.as_str()),
                call.name,
                self.layout(),
                // Read off the call node the same parse produced, so the card and the jump
                // cannot disagree about one cursor: a private method is not a signature to draw
                // where Ruby would refuse the call. See `cursor::Call::allows_private`.
                locator::Privacy::written(call.allows_private, &modifiers),
            )?;
            signature_help::help(&self.graph, method, &call.active)
        })?
    }

    /// `textDocument/documentHighlight`.
    ///
    /// Answered from the buffer rather than from the graph's copy of it, like completion and
    /// signature help: the half of the answer that comes from `scopes` is a fact about the text
    /// as it stands this keystroke, and a highlight drawn over stale offsets lands on the wrong
    /// words rather than on none.
    fn document_highlights(&self, params: serde_json::Value) -> Option<Vec<DocumentHighlight>> {
        let params: lsp_types::DocumentHighlightParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let found = self.with_text(&uri, |text| {
            let found = highlight::find(
                &self.graph,
                &self.synthesized,
                UriId::from(uri.as_str()),
                text.text(),
                text.offset_at(position),
            );
            found
                .into_iter()
                .map(|at| DocumentHighlight {
                    range: text.range_at(at.start, at.end),
                    kind: Some(at.kind),
                })
                .collect::<Vec<_>>()
        })?;

        // `null` rather than `[]`, for the same reason completion answers one inside a comment:
        // it is what tells the client nothing was known here, so it may fall back to matching
        // words itself.
        (!found.is_empty()).then_some(found)
    }

    /// `textDocument/selectionRange`.
    ///
    /// One chain per position asked about, in the order asked: the protocol pairs the two arrays
    /// by index and has no spelling for "not this one", so every position answers — with the
    /// buffer itself where there was nothing else to say.
    fn selection_ranges(&self, params: serde_json::Value) -> Option<Vec<SelectionRange>> {
        let params: lsp_types::SelectionRangeParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;

        let found = self.with_text(&uri, |text| {
            params
                .positions
                .iter()
                .map(|&position| ranges::selection_range(text, text.offset_at(position)))
                .collect::<Vec<_>>()
        })?;

        (!found.is_empty()).then_some(found)
    }

    /// `textDocument/foldingRange`.
    ///
    /// `null` rather than `[]`, and here it matters more than anywhere else: a client that has a
    /// folding provider stops guessing from indentation, so an empty array would take away the
    /// fallback *and* put nothing in its place. A `null` can only give it back.
    fn folding_ranges(&self, params: serde_json::Value) -> Option<Vec<FoldingRange>> {
        let params: lsp_types::FoldingRangeParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;
        // Declined in a template, and the `null` above is exactly why it can be. The walk sees
        // the Ruby and nothing else: over a template with a five-line `<div>` in it, it offers
        // two folds for the `<% %>` blocks and none for the markup. Handing the editor's
        // indentation guess back gets the whole file folded, including those two.
        if erb::is_template_uri(&uri) {
            return None;
        }
        let found = self.with_text(&uri, ranges::folds)?;

        (!found.is_empty()).then_some(found)
    }

    /// `textDocument/semanticTokens/full`.
    ///
    /// The whole document, every time, and no delta — see [`tokens`] for why. The relative
    /// encoding is the protocol's, not a choice: each token is a delta from the one before it,
    /// which is why [`tokens::of`] sorts.
    fn semantic_tokens(&self, params: serde_json::Value) -> Option<SemanticTokens> {
        let params: lsp_types::SemanticTokensParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;

        self.with_text(&uri, |text| {
            let mut data: Vec<SemanticToken> = Vec::new();
            let (mut line, mut start) = (0, 0);
            for token in tokens::of(text.text()) {
                let at = text.position_at(token.start);
                // A token never spans a line — every one of them is an identifier — so the
                // length is the difference between two characters on the same line, in whatever
                // unit the client negotiated. Taking `end - start` in bytes would be wrong for
                // every non-ASCII name, and `имя` is a legal local.
                let length = text
                    .position_at(token.end)
                    .character
                    .saturating_sub(at.character);
                // Saturating, all three of them. The list is sorted, so none of these can go
                // backwards — and a subtraction that could underflow sits on the analysis
                // thread, where a panic is not a wrong colour but a server that stops
                // answering anything at all.
                data.push(SemanticToken {
                    delta_line: at.line.saturating_sub(line),
                    delta_start: if at.line == line {
                        at.character.saturating_sub(start)
                    } else {
                        at.character
                    },
                    length,
                    token_type: token.kind as u32,
                    token_modifiers_bitset: 0,
                });
                line = at.line;
                start = at.character;
            }
            SemanticTokens {
                result_id: None,
                data,
            }
        })
    }

    /// `textDocument/definition`.
    fn goto_definition(&self, params: serde_json::Value) -> Option<GotoDefinitionResponse> {
        let params: lsp_types::GotoDefinitionParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let read = |uri: &str| self.read_of(uri);
        let sources = self.sources(&read);

        // The scope walk first, for the reason `highlight::find` asks it first: an instance
        // variable is the one ordinary thing to point at that the graph does not model, and
        // `@name = 1` — the single span both halves can answer for — resolves in the graph to
        // the one place the variable is written and none of the places it is read.
        //
        // **Nothing here is rebased and nothing here may be.** The writes were read out of the
        // buffer the client is holding, in the file the cursor is already in, so they are
        // already in the coordinates the answer is sent in. `documentHighlight` answers from
        // the buffer for the same reason and it is the same walk.
        //
        // The name comes back beside the links because the fall-through needs it and this is
        // the one place the buffer is already open: a template's writes are in another document
        // by construction, and re-reading this one to spell `@story` again would be a second
        // blanking of the whole file for six bytes.
        if let Some((origin, name, links)) = self
            .with_text(&uri, |text| {
                let offset = text.offset_at(position);
                let variable = locator::variable_at(text.text(), offset)?;
                let origin = text.range_at(variable.start, variable.end);
                let name = text
                    .text()
                    .get(variable.start as usize..variable.end as usize)?
                    .to_owned();
                let links: Vec<LocationLink> = variable
                    .writes
                    .iter()
                    .map(|&(start, end)| LocationLink {
                        origin_selection_range: Some(origin),
                        target_uri: params
                            .text_document_position_params
                            .text_document
                            .uri
                            .clone(),
                        // The name and nothing around it: an instance variable's whole
                        // construct *is* its name, and the value assigned to it is not the
                        // thing that was navigated to.
                        target_range: text.range_at(start, end),
                        target_selection_range: text.range_at(start, end),
                    })
                    .collect();
                Some((origin, name, links))
            })
            .flatten()
        {
            // A file that only ever reads `@foo` has nothing of its own to answer with — a
            // superclass assigned it, or a controller did — and falls through rather than
            // declining, the same rule the loop below keeps for a target with no site.
            if !links.is_empty() {
                return self.definition_response(links);
            }
            // The same walk, continued into the one document a template's writes can be in, and
            // still before the graph: a *read* of `@story` is a span rubydex files nothing
            // under, so there is no answer here to be displaced.
            if let Some(links) = self.template_variable_links(&uri, origin, &name, &sources) {
                return self.definition_response(links);
            }
        }

        let (origin, sites) = self.with_text(&uri, |text| {
            let offset = text.offset_at(position);
            let rebase = self.rebase_for(&uri, text.text());
            let at = rebase.to_graph(offset)?;

            let uri_id = UriId::from(uri.as_str());
            for located in locator::locate(&self.graph, uri_id, at) {
                let found = rebase.span_to_buffer(ByteSpan {
                    start: located.start,
                    end: located.end,
                })?;
                let (start, end) = (found.start, found.end);
                // The same rung hover reads. A jump and a card that disagreed about what
                // `person.` is would be worse than either being absent.
                let resolved = locator::resolve_typed(
                    &sources,
                    uri_id,
                    text.text(),
                    &located,
                    start,
                    &rebase,
                )?
                .declarations;
                let sites: Vec<Site> = locator::all_places(
                    &self.graph,
                    &self.synthesized,
                    self.layout(),
                    resolved,
                    Some(uri.as_str()),
                );
                if !sites.is_empty() {
                    return Some((text.range_at(start, end), sites));
                }
            }

            // Nothing in the graph covers the cursor, and the two ordinary things it never
            // covers are both *arguments*: rubydex records a call and not what is written
            // inside it. A macro's `:symbol` is one and a `require`'s path is the other, and
            // they are asked here rather than ahead of the loop because — unlike the scope
            // walk above — the graph is not wrong about either of them, it is silent.
            //
            // **These sites are rebased and the writes above are not.** What a symbol names is
            // a declaration in the graph, which may be in another file entirely; `link` moves
            // each into its own document's coordinates, exactly as it does for a constant.
            if let Some((symbol, resolution)) =
                locator::resolve_symbol(&self.graph, uri_id, text.text(), offset, at)
            {
                let sites: Vec<Site> = locator::all_places(
                    &self.graph,
                    &self.synthesized,
                    self.layout(),
                    resolution.declarations,
                    Some(uri.as_str()),
                );
                if !sites.is_empty() {
                    return Some((text.range_at(symbol.start, symbol.end), sites));
                }
            }

            let require = requires::at(text.text(), offset)?;
            let site = self.require_site(&uri, &require)?;
            Some((text.range_at(require.start, require.end), vec![site]))
        })??;

        self.definition_response(
            sites
                .into_iter()
                .filter_map(|site| self.link(origin, &site))
                .collect(),
        )
    }

    /// Where a *template's* instance variable is written, which is never the file the cursor is
    /// in.
    ///
    /// The second half of the scope walk above and not a second mechanism. A template has no
    /// enclosing class, so `@story`'s writes are in another document by construction, and
    /// [`locator::variable_at`] searching only the buffer the cursor is in is why `definition`
    /// answered **4** of the 1,301 template reads drawn from the six corpora while the card at
    /// the same cursors answered 759. On that same draw it is **870** now, against the card's
    /// 788 — the two moved apart again when the rung learned a mailer's views, and they moved
    /// through one function.
    ///
    /// **The same rung the card takes, so the two cannot disagree.** `types::renderer_writes`
    /// and the card's own reader go through one function for the class and its documents — the
    /// controller a template's directory names, or the mailer where there is no controller — so
    /// where the convention names nothing, `shared/_header.html.erb` and the several
    /// controllers that render it, both answer nothing rather than picking one. The tier does
    /// not move either: the class a path implies is a *Derived* answer, and a jump built on it
    /// is not promoted for having a `Location`.
    ///
    /// **Nothing here is rebased and nothing here may be**, which is the walk's rule with two
    /// texts instead of one: the origin span is measured against the template's buffer and each
    /// target span against the renderer's, each in the text it was read out of.
    ///
    /// The renderer is read twice — once for the writes and once for the encoding this turns
    /// them into positions with — and both reads go through `with_text`, so both see the same
    /// string. It is one file, on a path an ordinary Ruby document never takes.
    fn template_variable_links(
        &self,
        uri: &DocUri,
        origin: lsp_types::Range,
        name: &str,
        sources: &types::Sources<'_>,
    ) -> Option<Vec<LocationLink>> {
        // A path test before anything else. This is asked of every `definition` at an instance
        // variable the file itself never writes, which includes a subclass reading one its
        // superclass assigned, and an ordinary Ruby file must pay no more than a look at its
        // own name for a rung it can never reach.
        if !erb::is_template_uri(uri) {
            return None;
        }
        let uri_id = UriId::from(uri.as_str());
        let links: Vec<LocationLink> = types::renderer_writes(sources, uri_id, name)
            .into_iter()
            .filter_map(|(target, writes)| {
                let target = DocUri::from_uri_str(&target)?;
                let target_uri = target.to_lsp().ok()?;
                self.with_text(&target, |text| {
                    writes
                        .iter()
                        .map(|&(start, end)| LocationLink {
                            origin_selection_range: Some(origin),
                            target_uri: target_uri.clone(),
                            // The name and nothing around it, exactly as the in-file walk
                            // answers: what was navigated to is where the variable is written
                            // and not the expression it was written from.
                            target_range: text.range_at(start, end),
                            target_selection_range: text.range_at(start, end),
                        })
                        .collect::<Vec<_>>()
                })
            })
            .flatten()
            .collect();
        (!links.is_empty()).then_some(links)
    }

    /// The two shapes `definition` answers in, decided in one place.
    ///
    /// Two paths reach it — the scope walk and the graph — and which spelling the client asked
    /// for is a property of the client rather than of either.
    fn definition_response(&self, links: Vec<LocationLink>) -> Option<GotoDefinitionResponse> {
        if links.is_empty() {
            return None;
        }
        Some(if self.client.definition_links {
            GotoDefinitionResponse::Link(links)
        } else {
            GotoDefinitionResponse::Array(
                links
                    .into_iter()
                    .map(|link| Location {
                        uri: link.target_uri,
                        // The name, not the whole body: it is where the editor parks the
                        // cursor, and landing on `class` is landing in the right place.
                        range: link.target_selection_range,
                    })
                    .collect(),
            )
        })
    }

    /// `textDocument/documentLink`.
    ///
    /// The require half of `definition`, asked about the whole file at once and without a cursor
    /// having to be anywhere near it. Nothing else in a Ruby file is a link: a constant is
    /// navigation rather than a resource, and the editor already has ctrl-click for it.
    ///
    /// **A require that resolves nowhere produces no link.** The protocol lets a link carry no
    /// `target` and be resolved later, and that spelling is wrong here twice over — there is
    /// nothing to resolve later, and an underlined path that goes nowhere when clicked is worse
    /// than an un-underlined one. `require "json"` in a project with no indexed stdlib is
    /// exactly that case, and it is common.
    fn document_links(&self, params: serde_json::Value) -> Option<Vec<DocumentLink>> {
        let params: lsp_types::DocumentLinkParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;

        // The buffer is parsed and the buffer is measured, so no rebase: both halves of every
        // link come out of the same text, and the graph is only asked about a path string —
        // which no edit can move.
        let links = self.with_text(&uri, |text| {
            requires::all(text.text())
                .into_iter()
                .filter_map(|require| {
                    let site = self.require_site(&uri, &require)?;
                    Some(DocumentLink {
                        range: text.range_at(require.start, require.end),
                        target: Some(DocUri::from_uri_str(&site.uri)?.to_lsp().ok()?),
                        tooltip: None,
                        data: None,
                    })
                })
                .collect::<Vec<_>>()
        })?;

        (!links.is_empty()).then_some(links)
    }

    // -----------------------------------------------------------------------
    // Project-wide search
    // -----------------------------------------------------------------------

    /// `textDocument/references`.
    ///
    /// Answers only with the user's own code — see `analysis::references` for why, and for the
    /// difference in precision between a constant and a method.
    fn references(&self, params: serde_json::Value) -> Option<Vec<Location>> {
        let params: lsp_types::ReferenceParams = parse_params(params)?;
        let position = params.text_document_position.position;
        let uri = DocUri::from_lsp(&params.text_document_position.text_document.uri)?;
        let include_declaration = params.context.include_declaration;
        let scope = self.own_documents();

        let mut found = self.with_text(&uri, |text| {
            let offset = text.offset_at(position);
            // As in goto-definition: several targets can share the narrowest span, so take the
            // first that has something to say rather than the first that exists.
            locator::locate(&self.graph, UriId::from(uri.as_str()), offset)
                .into_iter()
                .find_map(|located| {
                    let resolution = locator::resolve(&self.graph, &located);
                    let found = references::find(
                        &self.graph,
                        &self.synthesized,
                        &located,
                        &resolution,
                        &scope,
                        include_declaration,
                    );
                    (!found.is_empty()).then_some(found)
                })
        })??;

        if found.len() > MAX_REFERENCES {
            // Said out loud, not just logged. A truncated "find all references" is a wrong
            // answer wearing the shape of a right one, and the user is the only one who can
            // decide what to do about it. It takes a workspace of tens of thousands of files to
            // reach — measured: `.new` across 17,557 files finds 35,733 — so this is rare
            // enough that a message is information rather than noise.
            let message = messages::references_truncated(found.len(), MAX_REFERENCES);
            tracing::warn!("{message}");
            self.show_warning(&message);
            found.truncate(MAX_REFERENCES);
        }

        let mut ranges = Ranges::new(self);
        let locations: Vec<Location> = found
            .into_iter()
            .filter_map(|reference| {
                let uri = DocUri::from_uri_str(&reference.uri)?;
                Some(Location {
                    range: ranges.at(&uri, reference.start, reference.end)?,
                    uri: uri.to_lsp().ok()?,
                })
            })
            .collect();
        (!locations.is_empty()).then_some(locations)
    }

    /// `workspace/symbol`.
    fn workspace_symbols(&self, params: serde_json::Value) -> Option<WorkspaceSymbolResponse> {
        let params: lsp_types::WorkspaceSymbolParams = parse_params(params)?;
        let started = Instant::now();
        let hits = search::search(
            &self.graph,
            &self.synthesized,
            params.query.trim(),
            MAX_WORKSPACE_SYMBOLS,
            &self.own_documents(),
            self.layout().names,
        );
        tracing::debug!(
            "workspace/symbol {:?} -> {} hits in {:.2?}",
            params.query,
            hits.len(),
            started.elapsed()
        );

        let mut ranges = Ranges::new(self);
        let symbols: Vec<SymbolInformation> = hits
            .into_iter()
            .filter_map(|hit| {
                let uri = DocUri::from_uri_str(&hit.site.uri)?;
                // The name span, not the whole construct: a client reveals `location.range`
                // selected, and selecting a 400-line class body to show where it starts is
                // not what anyone asked for.
                let range = ranges.at(&uri, hit.site.selection.0, hit.site.selection.1)?;
                #[allow(deprecated)] // Required field, superseded by `tags`.
                Some(SymbolInformation {
                    name: hit.name,
                    kind: hit.kind,
                    tags: hit.tags,
                    deprecated: None,
                    location: Location {
                        uri: uri.to_lsp().ok()?,
                        range,
                    },
                    container_name: hit.container,
                })
            })
            .collect();

        // `null` rather than `[]` for nothing found, the same as every other handler: an empty
        // array is a claim that the project has no such symbol, which is only true by accident.
        (!symbols.is_empty()).then_some(WorkspaceSymbolResponse::Flat(symbols))
    }

    // -----------------------------------------------------------------------
    // Type hierarchy
    // -----------------------------------------------------------------------

    /// `textDocument/prepareTypeHierarchy`.
    ///
    /// The item this hands back is what the two follow-ups arrive holding, so it carries the
    /// declaration in `data` — as a decimal string, for `completionItem/resolve`'s reason: a
    /// `DeclarationId` is a 64-bit hash and JSON numbers are doubles. Unlike a completion item
    /// this one survives a config reload, because the hash is of the *name*: the graph the client
    /// was looking at can be gone and the id still finds the class.
    fn prepare_type_hierarchy(&self, params: serde_json::Value) -> Option<Vec<TypeHierarchyItem>> {
        let params: lsp_types::TypeHierarchyPrepareParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let items = self.with_text(&uri, |text| {
            hierarchy::prepare(
                &self.graph,
                &self.synthesized,
                UriId::from(uri.as_str()),
                text.offset_at(position),
                &self.own_documents(),
                self.layout().names,
            )
        })?;
        self.hierarchy_items(items)
    }

    /// `typeHierarchy/supertypes`.
    fn supertypes(&self, params: serde_json::Value) -> Option<Vec<TypeHierarchyItem>> {
        let params: lsp_types::TypeHierarchySupertypesParams = parse_params(params)?;
        let declaration = declaration_in(params.item.data.as_ref())?;
        let items = hierarchy::supertypes(
            &self.graph,
            &self.synthesized,
            declaration,
            &self.own_documents(),
            self.layout().names,
        );
        self.hierarchy_items(items)
    }

    /// `typeHierarchy/subtypes`.
    fn subtypes(&self, params: serde_json::Value) -> Option<Vec<TypeHierarchyItem>> {
        let params: lsp_types::TypeHierarchySubtypesParams = parse_params(params)?;
        let declaration = declaration_in(params.item.data.as_ref())?;
        let found = hierarchy::subtypes(
            &self.graph,
            &self.synthesized,
            declaration,
            MAX_SUBTYPES,
            &self.own_documents(),
            self.layout().names,
        );

        if found.found > MAX_SUBTYPES {
            // Said out loud, as a truncated `references` is: a short list of subtypes is
            // indistinguishable from a complete one, and the user is the only one who can decide
            // what to do about it. It takes asking about something near the root of the object
            // model to reach, which is a deliberate click rather than something that happens
            // while typing, so a message here is information rather than noise.
            let message = messages::subtypes_truncated(found.found, MAX_SUBTYPES);
            tracing::warn!("{message}");
            self.show_warning(&message);
        }
        self.hierarchy_items(found.items)
    }

    /// Turn hierarchy rows into the wire shape, reading each file at most once.
    ///
    /// A row whose file cannot be read is dropped rather than sent with a made-up range —
    /// rubydex's synthetic built-in document is the one that reaches here, and `DocUri` rejects
    /// it for every request alike. `null` for an empty result, never `[]`: an empty array is a
    /// claim that a class has no ancestors, which is not true of anything in Ruby.
    fn hierarchy_items(&self, items: Vec<hierarchy::Item>) -> Option<Vec<TypeHierarchyItem>> {
        let mut ranges = Ranges::new(self);
        let items: Vec<TypeHierarchyItem> = items
            .into_iter()
            .filter_map(|item| {
                let uri = DocUri::from_uri_str(&item.site.uri)?;
                Some(TypeHierarchyItem {
                    name: item.name,
                    kind: item.kind,
                    tags: None,
                    detail: Some(item.detail),
                    range: ranges.at(&uri, item.site.full.0, item.site.full.1)?,
                    selection_range: ranges.at(
                        &uri,
                        item.site.selection.0,
                        item.site.selection.1,
                    )?,
                    uri: uri.to_lsp().ok()?,
                    data: item
                        .declaration
                        .map(|id| serde_json::Value::String(id.get().to_string())),
                })
            })
            .collect();
        (!items.is_empty()).then_some(items)
    }

    // -----------------------------------------------------------------------
    // Call hierarchy
    // -----------------------------------------------------------------------

    /// `textDocument/prepareCallHierarchy`.
    ///
    /// The same `data` the type hierarchy carries, for the same reason: the follow-ups arrive
    /// holding this item and nothing else, and a `DeclarationId` is a 64-bit hash that JSON's
    /// doubles would round. `outgoingCalls` reads the item's *position* as well — see
    /// `hierarchy::outgoing` for why one of the two directions cannot use the declaration.
    fn prepare_call_hierarchy(&self, params: serde_json::Value) -> Option<Vec<CallHierarchyItem>> {
        let params: lsp_types::CallHierarchyPrepareParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let items = self.with_text(&uri, |text| {
            hierarchy::prepare_calls(
                &self.graph,
                &self.synthesized,
                UriId::from(uri.as_str()),
                text.offset_at(position),
                &self.own_documents(),
                self.layout().names,
            )
        })?;
        self.call_items(items)
    }

    /// `callHierarchy/incomingCalls`.
    ///
    /// Every row is a name match, because rubydex links no method reference to a declaration —
    /// `references` makes the same trade and says so in its own comment. Here it is said in the
    /// row: the detail column of every caller ends in "by name", because a tree implies an
    /// exactness that this answer does not have.
    fn incoming_calls(&self, params: serde_json::Value) -> Option<Vec<CallHierarchyIncomingCall>> {
        let params: lsp_types::CallHierarchyIncomingCallsParams = parse_params(params)?;
        let declaration = declaration_in(params.item.data.as_ref())?;
        let found = hierarchy::incoming(
            &self.graph,
            &self.synthesized,
            declaration,
            MAX_INCOMING_CALLS,
            &self.own_documents(),
        );

        if found.found > MAX_INCOMING_CALLS {
            // Said out loud, as a truncated `references` and a truncated subtype list are: a
            // short list of callers is indistinguishable from a complete one, and a call
            // hierarchy is read as if it were complete.
            let message = messages::incoming_calls_truncated(found.found, MAX_INCOMING_CALLS);
            tracing::warn!("{message}");
            self.show_warning(&message);
        }

        let mut ranges = Ranges::new(self);
        let calls: Vec<CallHierarchyIncomingCall> = found
            .calls
            .into_iter()
            .filter_map(|call| {
                // The ranges are in the caller's own file, which is this row's file.
                let uri = DocUri::from_uri_str(&call.item.site.uri)?;
                let from_ranges: Vec<lsp_types::Range> = call
                    .ranges
                    .iter()
                    .filter_map(|(start, end)| ranges.at(&uri, *start, *end))
                    .collect();
                // A row whose calls could not be placed is dropped rather than sent empty: a
                // caller with nothing to jump to is worse than no caller.
                (!from_ranges.is_empty()).then_some(())?;
                Some(CallHierarchyIncomingCall {
                    from: self.call_item(&mut ranges, call.item)?,
                    from_ranges,
                })
            })
            .collect();
        (!calls.is_empty()).then_some(calls)
    }

    /// `callHierarchy/outgoingCalls`.
    ///
    /// Addressed by the item's position rather than by its declaration — the body the client is
    /// expanding is the one whose calls were asked about, and a reopened class has more than one.
    fn outgoing_calls(&self, params: serde_json::Value) -> Option<Vec<CallHierarchyOutgoingCall>> {
        let params: lsp_types::CallHierarchyOutgoingCallsParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.item.uri)?;
        let position = params.item.selection_range.start;

        let calls = self.with_text(&uri, |text| {
            hierarchy::outgoing(
                &self.graph,
                &self.synthesized,
                UriId::from(uri.as_str()),
                text.offset_at(position),
                &self.own_documents(),
                self.layout().names,
                self.layout(),
            )
        })?;

        let mut ranges = Ranges::new(self);
        let calls: Vec<CallHierarchyOutgoingCall> = calls
            .into_iter()
            .filter_map(|call| {
                // Unlike an incoming call, the ranges are in the file being *expanded* rather
                // than in the row's own file: an outgoing call is written where the caller is.
                let from_ranges: Vec<lsp_types::Range> = call
                    .ranges
                    .iter()
                    .filter_map(|(start, end)| ranges.at(&uri, *start, *end))
                    .collect();
                (!from_ranges.is_empty()).then_some(())?;
                Some(CallHierarchyOutgoingCall {
                    to: self.call_item(&mut ranges, call.item)?,
                    from_ranges,
                })
            })
            .collect();
        (!calls.is_empty()).then_some(calls)
    }

    /// Call hierarchy rows in the wire shape. `null` for an empty result, never `[]`.
    fn call_items(&self, items: Vec<hierarchy::Item>) -> Option<Vec<CallHierarchyItem>> {
        let mut ranges = Ranges::new(self);
        let items: Vec<CallHierarchyItem> = items
            .into_iter()
            .filter_map(|item| self.call_item(&mut ranges, item))
            .collect();
        (!items.is_empty()).then_some(items)
    }

    /// One row, placed in its file.
    ///
    /// `None` where the file cannot be read or the URI is not one an editor can open — the drop
    /// `hierarchy_items` makes, for the same reason. Two functions rather than one generic one
    /// because a `TypeHierarchyItem` and a `CallHierarchyItem` are two structs with the same
    /// fields and no relation in the type system.
    fn call_item(&self, ranges: &mut Ranges, item: hierarchy::Item) -> Option<CallHierarchyItem> {
        let uri = DocUri::from_uri_str(&item.site.uri)?;
        Some(CallHierarchyItem {
            name: item.name,
            kind: item.kind,
            tags: None,
            detail: Some(item.detail),
            range: ranges.at(&uri, item.site.full.0, item.site.full.1)?,
            selection_range: ranges.at(&uri, item.site.selection.0, item.site.selection.1)?,
            uri: uri.to_lsp().ok()?,
            data: item
                .declaration
                .map(|id| serde_json::Value::String(id.get().to_string())),
        })
    }

    // -----------------------------------------------------------------------
    // Inlay hints
    // -----------------------------------------------------------------------

    /// `textDocument/inlayHint`.
    ///
    /// The one request that shows an answer nobody asked for, which is why the tier decides what
    /// may be drawn rather than only how it is labelled — see [`hints`]. The range is the
    /// editor's visible window and is handed straight down: it bounds the work, not the answer.
    fn inlay_hints(&self, params: serde_json::Value) -> Option<Vec<InlayHint>> {
        let params: lsp_types::InlayHintParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;

        let read = |uri: &str| self.read_of(uri);
        let sources = self.sources(&read);
        let found = self.with_text(&uri, |text| {
            let within = (
                text.offset_at(params.range.start),
                text.offset_at(params.range.end),
            );
            hints::of(
                &sources,
                &uri,
                text.text(),
                within,
                &self.workspace.config().hints,
            )
            .into_iter()
            .map(|hint| self.inlay_hint(&uri, text, &hint))
            .collect::<Vec<_>>()
        })?;

        (!found.is_empty()).then_some(found)
    }

    /// `inlayHint/resolve` — the footnote the label's marker promised.
    ///
    /// The hint is recomputed for the one position it sits at rather than carried across the two
    /// requests, which is what a `data` big enough to hold a derivation would have meant: the
    /// tooltip would then ship eagerly inside every hint, which is the cost this request exists
    /// to avoid. A resolved hint carries no `data` at all and never arrives here.
    fn resolve_inlay_hint(&self, params: serde_json::Value) -> Option<InlayHint> {
        let mut hint: InlayHint = parse_params(params)?;
        let data = hint.data.clone()?;
        let at = u32::try_from(data.get("at")?.as_u64()?).ok()?;
        let uri = DocUri::from_uri_str(data.get("uri")?.as_str()?)?;

        let read = |uri: &str| self.read_of(uri);
        let sources = self.sources(&read);
        hint.tooltip = self.with_text(&uri, |text| {
            let found = hints::of(
                &sources,
                &uri,
                text.text(),
                (at, at),
                &self.workspace.config().hints,
            );
            let found = found.iter().find(|found| found.at == at)?;
            let line = found.assignment().map(|at| text.position_at(at).line + 1);
            Some(InlayHintTooltip::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: found.note(line)?,
            }))
        })?;
        Some(hint)
    }

    /// One hint in the wire shape.
    ///
    /// `data` on every one of them, because every hint that is drawn is derived and so has a
    /// footnote to fetch — see [`hints`] for why the resolved tier cannot occur here. What the
    /// resolve step buys is that the sentence is built for the hint somebody pointed at rather
    /// than for every line on screen, every scroll.
    ///
    /// `TYPE` for all three families — the protocol's `PARAMETER` is its name for argument
    /// labels at a *call site*, which is a different feature.
    fn inlay_hint(&self, uri: &DocUri, text: &TextDocument, hint: &hints::Hint) -> InlayHint {
        InlayHint {
            position: text.position_at(hint.at),
            label: InlayHintLabel::String(hint.label.clone()),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: None,
            data: Some(serde_json::json!({ "uri": uri.as_str(), "at": hint.at })),
        }
    }

    // -----------------------------------------------------------------------
    // Rename
    // -----------------------------------------------------------------------

    /// `textDocument/prepareRename`.
    ///
    /// `textDocument/codeAction`.
    ///
    /// The second request that writes, and it goes through the same two gates the first one
    /// does: nothing is proposed for a file that is not the user's own, and nothing at all is
    /// proposed inside a gem. It is a *silent* refusal, unlike a rename's — the user pressed no
    /// key asking for this one, so an action absent from a menu is the whole of what needs
    /// saying.
    ///
    /// Declined in a template, and for a reason no other request has: every action here writes a
    /// **line**, and in a template a line belongs to the markup. `erb::ruby_view` keeps offsets
    /// so that everything which reads answers unchanged, and there is nothing it can do about a
    /// line that starts with `<td>`.
    fn code_actions(&self, params: serde_json::Value) -> Option<Vec<CodeActionOrCommand>> {
        let params: lsp_types::CodeActionParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;
        if !self.is_own_code(uri.as_str()) || erb::is_template_uri(&uri) {
            return None;
        }
        let actions = self.with_text(&uri, |text| {
            let start = text.offset_at(params.range.start);
            let end = text.offset_at(params.range.end);
            code_actions::at(text.text(), start, end)
                .into_iter()
                .map(|action| {
                    // Converted here, once, from the same read that produced the offsets — the
                    // reason `rename::Replacement` carries both, one layer down.
                    let edits = action
                        .edits
                        .iter()
                        .map(|edit| TextEdit {
                            range: text.range_at(edit.start, edit.end),
                            new_text: edit.text.clone(),
                        })
                        .collect();
                    CodeActionOrCommand::CodeAction(CodeAction {
                        title: action.title,
                        kind: Some(match action.kind {
                            code_actions::Kind::Extract => CodeActionKind::REFACTOR_EXTRACT,
                            code_actions::Kind::Rewrite => CodeActionKind::REFACTOR_REWRITE,
                        }),
                        edit: Some(self.workspace_edit(vec![(uri.clone(), edits)])),
                        ..CodeAction::default()
                    })
                })
                .collect::<Vec<_>>()
        })?;

        (!actions.is_empty()).then_some(actions)
    }

    /// Answering with a range is a *promise* that the rename will go through, so this runs the
    /// whole plan and reads every span back before it says yes. That costs a file read per file
    /// the name is written in, once, on a key the user pressed deliberately — and it is the
    /// only way the promise is honest. A prepare that said yes and a rename that then refused
    /// would put the refusal after the user had typed the new name.
    fn prepare_rename(&self, params: serde_json::Value) -> Option<PrepareRenameResponse> {
        let params: lsp_types::TextDocumentPositionParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;
        let offset = self.with_text(&uri, |text| text.offset_at(params.position))?;
        let renaming = self.renaming(&uri, offset)?;

        // The one replacement the cursor is actually in, out of everything the rename would
        // make: the editor puts its rename box over exactly this range and pre-fills it with
        // the text inside. It is not always the span the cursor was *located* in — the name of
        // `Failure = Class.new(StandardError)` is located as the whole assignment and narrowed
        // to the word — so a cursor on the `=` of that finds nothing and answers `null`.
        let here = renaming
            .files
            .iter()
            .filter(|(at, _)| *at == uri)
            .flat_map(|(_, replacements)| replacements)
            // A range rather than a pair of comparisons, which is the same test without the
            // short-circuit arm a `&&` would put in a file this one is measured with.
            .find(|at| (at.start..=at.end).contains(&offset))?;
        Some(PrepareRenameResponse::Range(here.range))
    }

    /// `textDocument/rename`.
    fn rename(&self, params: serde_json::Value) -> Option<WorkspaceEdit> {
        let params: lsp_types::RenameParams = parse_params(params)?;
        let position = params.text_document_position;
        let uri = DocUri::from_lsp(&position.text_document.uri)?;
        let offset = self.with_text(&uri, |text| text.offset_at(position.position))?;
        // The plan is made again rather than remembered from the prepare: `prepareSupport` is a
        // client capability, and a client without it sends this request on its own, so every
        // refusal has to be reachable from here too.
        let renaming = self.renaming(&uri, offset)?;

        if !rename::is_name(&params.new_name, renaming.constant) {
            let message = messages::rename_needs_a_ruby_name(&params.new_name, renaming.constant);
            tracing::info!("{message}");
            self.show_warning(&message);
            return None;
        }

        // Nothing left to decide: every range was converted when the plan was confirmed, from
        // the same read that checked the bytes under it, so there is no second conversion here
        // to disagree with the first one or to fail on its own.
        let files: Vec<(DocUri, Vec<TextEdit>)> = renaming
            .files
            .iter()
            .map(|(at, replacements)| {
                let edits = replacements
                    .iter()
                    .map(|at| TextEdit {
                        range: at.range,
                        new_text: params.new_name.clone(),
                    })
                    .collect();
                (at.clone(), edits)
            })
            .collect();
        // Counted into locals first: an argument on its own line inside a `tracing::debug!` is
        // evaluated only when that level is on, so it reads as a line no test ever ran.
        let edited: usize = files.iter().map(|(_, edits)| edits.len()).sum();
        let (touched, from, to) = (files.len(), &renaming.name, &params.new_name);
        tracing::debug!("rename {from:?} -> {to:?}: {edited} edits across {touched} files");
        Some(self.workspace_edit(files))
    }

    /// The rename at a position, with every span read back and checked against the name it is
    /// about to replace.
    ///
    /// Both requests go through here, and a refusal is said out loud rather than merely
    /// answered with `null`: the user pressed a key asking for this one, and an editor's own
    /// "this cannot be renamed" does not say which of the reasons applies or what to do next.
    /// That is the one place ya-lsp raises a `window/showMessage` for a single request rather
    /// than for the state of the workspace, and pressing the key is what earns it.
    fn renaming(&self, uri: &DocUri, offset: u32) -> Option<Renaming> {
        // ya-lsp never proposes an edit to a file that is not the user's own. Silently, as
        // every other request inside a bundle is: a gem is opened to be read, and nobody
        // pressing rename in one is expecting it to work.
        if !self.is_own_code(uri.as_str()) {
            return None;
        }
        let own = self.own_documents();
        let plan = self.with_text(&uri.clone(), |text| {
            rename::plan(
                &self.graph,
                &self.synthesized,
                uri.as_str(),
                text.text(),
                offset,
                &own,
            )
        })?;
        let (name, constant, edits) = match plan {
            rename::Plan::Nothing => return None,
            rename::Plan::Refused(message) => {
                tracing::info!("{message}");
                self.show_warning(&message);
                return None;
            }
            rename::Plan::Edits {
                name,
                constant,
                edits,
            } => (name, constant, edits),
        };

        // Every span, read back from the text as it stands and confirmed to hold only the name.
        // This is what stops `Error = Class.new(StandardError)` — whose name span rubydex
        // records as the entire assignment — from being replaced wholesale, and it is why a
        // refusal here is whole: a rename that changed most of the places a name is written
        // would leave code that no longer runs.
        let mut ranges = Ranges::new(self);
        let mut files: HashMap<DocUri, Vec<Replacement>> = HashMap::new();
        for edit in edits {
            let at = DocUri::from_uri_str(&edit.uri)?;
            let confirmed = ranges
                .text_at(&at, edit.start, edit.end)
                .and_then(|written| rename::narrow(&written, &name));
            let Some((from, to)) = confirmed else {
                let message = messages::rename_could_not_confirm(&name, &file_name(&at));
                tracing::warn!("{message}");
                self.show_warning(&message);
                return None;
            };
            let (start, end) = (edit.start + from, edit.start + to);
            files.entry(at.clone()).or_default().push(Replacement {
                start,
                end,
                // Converted here, from the read that just confirmed the bytes: the offsets and
                // the range are two views of one span, and deriving them apart is how they come
                // to disagree.
                range: ranges.at(&at, start, end)?,
            });
        }

        // Sorted by URI so that the same rename produces the same edit twice running; the spans
        // inside a file arrive in order already, from `references` and from the scope walk
        // alike.
        let mut files: Vec<(DocUri, Vec<Replacement>)> = files.into_iter().collect();
        files.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
        Some(Renaming {
            name,
            constant,
            files,
        })
    }

    /// The edit in whichever of the two shapes the client said it takes.
    ///
    /// `documentChanges` is worth negotiating for rather than always sending the older `changes`
    /// map, because it carries the version each file's edit was computed against — so a client
    /// can reject a rename the user has typed past instead of applying it to text that has
    /// moved. The version is the one from `didOpen`/`didChange` where there is a buffer, and
    /// `null` for a file only on disk, which is what the protocol's *optional* version means.
    fn workspace_edit(&self, files: Vec<(DocUri, Vec<TextEdit>)>) -> WorkspaceEdit {
        if !self.client.versioned_edits {
            return WorkspaceEdit {
                changes: Some(
                    files
                        .into_iter()
                        .filter_map(|(uri, edits)| Some((uri.to_lsp().ok()?, edits)))
                        .collect(),
                ),
                ..WorkspaceEdit::default()
            };
        }
        WorkspaceEdit {
            document_changes: Some(lsp_types::DocumentChanges::Edits(
                files
                    .into_iter()
                    .filter_map(|(uri, edits)| {
                        Some(TextDocumentEdit {
                            text_document: OptionalVersionedTextDocumentIdentifier {
                                version: self.open.get(&uri).and_then(|open| open.version),
                                uri: uri.to_lsp().ok()?,
                            },
                            edits: edits.into_iter().map(OneOf::Left).collect(),
                        })
                    })
                    .collect(),
            )),
            ..WorkspaceEdit::default()
        }
    }

    // -----------------------------------------------------------------------
    // Completion
    // -----------------------------------------------------------------------

    /// `textDocument/completion`.
    ///
    /// The list is `isIncomplete` when the cap dropped rows, which is the only way it can fail
    /// to hold something a longer prefix would reach — every filter behind it is a subsequence
    /// match. Below the cap the client narrows what it already has and sends nothing, which is
    /// a whole request per keystroke. See `analysis::completion` for that argument in full, and
    /// for what is exact here and what is a guess.
    fn completion(&self, params: serde_json::Value) -> Option<CompletionResponse> {
        let params: lsp_types::CompletionParams = parse_params(params)?;
        let position = params.text_document_position.position;
        let uri = DocUri::from_lsp(&params.text_document_position.text_document.uri)?;
        let started = Instant::now();

        // The one request that has to know a template from a Ruby file, and it has to ask the
        // *source* rather than the blanked view, because the view is where the markup went. A
        // half-typed word in an `<h1>` is spaces by the time completion sees it, so most of the
        // time this changes nothing — but a caret in markup that happens to sit after a run of
        // Ruby-looking bytes is a list of the workspace's constants offered to someone writing
        // prose, and this is the only request that can produce one without a token under it.
        if erb::is_template_uri(&uri)
            && !self
                .with_source(&uri, |source| {
                    let offset =
                        TextDocument::new(source.to_owned(), self.encoding).offset_at(position);
                    erb::in_ruby(source, offset as usize)
                })
                .unwrap_or(false)
        {
            return None;
        }

        let read = |uri: &str| self.read_of(uri);
        let sources = self.sources(&read);
        let (completion, range) = self.with_text(&uri, |text| {
            let offset = text.offset_at(position);
            // How this buffer's offsets relate to the graph's. The identity unless the index
            // was deferred, in which case it is the whole reason this answer can be trusted.
            let rebase = self.rebase_for(&uri, text.text());
            if !rebase.is_identity() {
                tracing::debug!("answering completion from a graph {rebase:?} behind the buffer");
            }
            let completion = completion::complete(
                &sources,
                UriId::from(uri.as_str()),
                text.text(),
                offset,
                completion::Limits {
                    items: MAX_COMPLETION_ITEMS,
                    untyped: MAX_UNTYPED_COMPLETION_ITEMS,
                    untyped_candidates: MAX_UNTYPED_CANDIDATES,
                },
                &self.own_documents(),
                &rebase,
            )?;
            let range = text.range_at(completion.start, completion.end);
            Some((completion, range))
        })??;

        tracing::debug!(
            "completion -> {} items in {:.2?}",
            completion.items.len(),
            started.elapsed()
        );

        let items: Vec<CompletionItem> = completion
            .items
            .into_iter()
            .enumerate()
            .map(|(index, item)| CompletionItem {
                kind: Some(completion_kind(item.kind)),
                detail: item.detail,
                documentation: item.documentation.map(|value| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value,
                    })
                }),
                tags: item.deprecated.then(|| vec![CompletionItemTag::DEPRECATED]),
                // Clients sort by their own fuzzy score first and fall back to this, so the
                // ranking survives as the tiebreak. Zero-padded because it is compared as text.
                sort_text: Some(format!("{index:05}")),
                // The span the cursor's half-typed word occupies, so that accepting `empty?`
                // over `emp` replaces it rather than appending to it.
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: item.label.clone(),
                })),
                // Only what `completionItem/resolve` needs to find the declaration again. A
                // string, not a number: a `DeclarationId` is a 64-bit hash and JSON numbers are
                // doubles, so the round trip through a client would corrupt it.
                data: item
                    .declaration
                    .map(|id| completion_data(id, completion.precise, completion.guess.as_deref())),
                label: item.label,
                ..CompletionItem::default()
            })
            .collect();

        Some(CompletionResponse::List(CompletionList {
            is_incomplete: completion.incomplete,
            items,
        }))
    }

    /// `completionItem/resolve` — the documentation for the one row the user is looking at.
    ///
    /// Reading a declaration's comments means reaching into its definitions, and a list of five
    /// hundred rows is five hundred of those for a single one anybody reads. LSP exists to avoid
    /// exactly that, so the list ships without documentation and this fills it in.
    fn resolve_completion(&self, params: serde_json::Value) -> Option<CompletionItem> {
        let mut item: CompletionItem = parse_params(params)?;
        let read = |uri: &str| self.read_of(uri);
        let modifiers = locator::Modifiers::new(&read);

        let markdown =
            completion_target(item.data.as_ref()).and_then(|(declaration, precise, guess)| {
                hover::markdown(
                    &self.graph,
                    &self.synthesized,
                    self.layout(),
                    &modifiers,
                    &locator::Resolution {
                        declarations: vec![declaration],
                        // What the list was built from, carried back on the item: a row off the
                        // name-based list is a guess and its card has to say so. It said `true`
                        // here for two releases, which made every guessed row's card read as
                        // certain.
                        precise,
                        redirected: false,
                        // The other guess, and a different one: the rows are a real class's
                        // members and the class itself was read off the receiver's name.
                        derivation: types::Derivation {
                            guess,
                            ..types::Derivation::default()
                        },
                        // A completion row is not a member lookup that failed: the list was
                        // built *from* the receiver, so there is no class the name under it is
                        // missing from.
                        missed: None,
                    },
                    None,
                    // `completionItem/resolve` is handed an item and not a position, so there
                    // is no cursor to read a test tree off. `None` is unfenced, which is the
                    // direction that can only keep a place and never invent one — and the row
                    // being described was already ranked by a list that did ask.
                    None,
                )
            });

        if let Some(value) = markdown {
            item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }));
        }
        // Always the item, never null: the client sent one and the protocol says it gets one
        // back, enriched or not.
        Some(item)
    }

    /// The documents that are the user's own code, as a set the reference scan can test.
    /// Where this project's own files are and what `require` can name, as the one value every
    /// surface that fences on a test tree takes.
    ///
    /// Built per call rather than held, for the reason [`Analysis::sources`] is: it borrows two
    /// fields that the next settle rewrites, and a held copy would be a third place they could
    /// come apart.
    pub(super) fn layout(&self) -> environment::Layout<'_> {
        environment::Layout {
            root: &self.workspace_prefix,
            load: &self.load_prefixes,
            names: environment::Names::of(&self.workspace.config().trees),
        }
    }

    pub(super) fn own_documents(&self) -> std::collections::HashSet<UriId> {
        self.graph
            .documents()
            .iter()
            .filter(|(_, document)| self.is_own_code(document.uri()))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Turn a graph site into a link, reading the target file to place its ranges.
    fn link(&self, origin: lsp_types::Range, site: &Site) -> Option<LocationLink> {
        let target = DocUri::from_uri_str(&site.uri)?;
        let target_uri = target.to_lsp().ok()?;
        // **The target's map and not the requester's.** A jump can land in any document, and any
        // open buffer may be a keystroke ahead of the graph — so the span is moved into *that*
        // file's coordinates. A document nobody is typing in has the identity, which is every
        // document nobody has typed in since the last settle.
        let (target_range, target_selection_range) = self.with_text(&target, |text| {
            let rebase = self.rebase_for(&target, text.text());
            let full = rebase.span_to_buffer(ByteSpan {
                start: site.full.0,
                end: site.full.1,
            })?;
            let selection = rebase.span_to_buffer(ByteSpan {
                start: site.selection.0,
                end: site.selection.1,
            })?;
            Some((
                text.range_at(full.start, full.end),
                text.range_at(selection.start, selection.end),
            ))
        })??;
        Some(LocationLink {
            origin_selection_range: Some(origin),
            target_uri,
            target_range,
            target_selection_range,
        })
    }

    /// Where a `require` points, if the graph has that file.
    ///
    /// `require_relative` resolves against the requiring file's own directory; a plain
    /// `require` resolves against the configured load paths, exactly as Ruby walks
    /// `$LOAD_PATH`.
    fn require_site(&self, from: &DocUri, require: &requires::Require) -> Option<Site> {
        let load_paths = if require.relative {
            vec![from.to_path()?.parent()?.to_path_buf()]
        } else {
            self.workspace.load_paths()
        };
        locator::require_site(&self.graph, &require.path, &load_paths)
    }
}

/// A rename that has been checked against the bytes it would replace.
///
/// Byte spans rather than ranges, because the two things done with them are a containment test
/// against the cursor and a conversion — and the first is arithmetic on offsets where it is a
/// two-field comparison on positions.
#[derive(Debug)]
struct Renaming {
    /// The name every one of these spans currently holds.
    name: String,
    /// Whether the replacement has to be a constant name rather than a variable's.
    constant: bool,
    /// What to replace, by document, each file's own in order and the files by URI.
    files: Vec<(DocUri, Vec<Replacement>)>,
}

/// One confirmed replacement: where it is in bytes, and the range the client is sent.
///
/// Both, out of the one read. The offsets are what a cursor position is compared against and
/// the range is what goes on the wire, and converting the second from the first a second time —
/// in the other handler, from a file possibly read again — is two chances for them to disagree
/// about the same span.
#[derive(Debug)]
struct Replacement {
    start: u32,
    end: u32,
    range: lsp_types::Range,
}

/// The last segment of a URI's path, which is what a message about a file names.
pub(super) fn file_name(uri: &DocUri) -> String {
    uri.as_str()
        .rsplit_once('/')
        .map_or(uri.as_str(), |(_, name)| name)
        .to_owned()
}

/// Byte spans to LSP ranges, reading each document at most once.
///
/// A project-wide answer names hundreds of spans across a handful of files, and converting one
/// span means reading and line-indexing the whole file it is in. Doing that per span reads the
/// same file once per hit in it — which on a file with fifty references to a method is fifty
/// reads of the same bytes.
struct Ranges<'a> {
    analysis: &'a Analysis,
    /// `None` for a document that could not be read, so a missing file is not re-attempted per
    /// span either.
    read: HashMap<DocUri, Option<TextDocument>>,
}

impl<'a> Ranges<'a> {
    fn new(analysis: &'a Analysis) -> Self {
        Self {
            analysis,
            read: HashMap::new(),
        }
    }

    fn at(&mut self, uri: &DocUri, start: u32, end: u32) -> Option<lsp_types::Range> {
        self.with(uri, |text| text.range_at(start, end))
    }

    /// The bytes a span covers, for a caller that has to know what it is about to replace.
    ///
    /// `None` for a span that runs past the end of the text as it is *now*, which is what a
    /// plan made against a file that has since been edited looks like — and one more reason
    /// `rename` reads every span back rather than trusting the offsets it was given.
    fn text_at(&mut self, uri: &DocUri, start: u32, end: u32) -> Option<String> {
        self.with(uri, |text| {
            Some(text.text().get(start as usize..end as usize)?.to_owned())
        })?
    }

    fn with<R>(&mut self, uri: &DocUri, read: impl FnOnce(&TextDocument) -> R) -> Option<R> {
        // An open buffer shadows disk and is already indexed; never cached, because the copy
        // would go stale the moment the user types.
        if let Some(open) = self.analysis.open.get(uri) {
            return Some(read(&open.text));
        }
        let encoding = self.analysis.encoding;
        self.read
            .entry(uri.clone())
            .or_insert_with(|| {
                let text = std::fs::read_to_string(uri.to_path()?).ok()?;
                Some(TextDocument::new(text, encoding))
            })
            .as_ref()
            .map(read)
    }
}

/// Wrap a handler's answer as a successful response.
///
/// `None` becomes JSON `null`, which is how LSP spells "there is nothing here". An error
/// response would be wrong: editors surface those to the user, and "no definition found" is
/// not something to complain about.
/// ya-lsp's own suggestion kinds, in LSP's vocabulary.
/// The declaration an item's `data` field names, as three requests round-trip one.
///
/// A decimal string rather than a JSON number, because a `DeclarationId` is a 64-bit hash and
/// JSON numbers are doubles — the round trip through a client would corrupt it. Everything about
/// the value is the client's word: it echoes back whatever the list it is looking at carried, and
/// a config reload drops the graph that list was built from, so nothing here may assume the id
/// still names anything.
/// What `completionItem/resolve` is given to find the row again.
///
/// An object rather than the bare id string the type hierarchy sends, because the card also has
/// to say which tier the row came from — and the row is the only thing that still knows by the
/// time the client asks. Both halves are strings: a `DeclarationId` is a 64-bit hash and JSON
/// numbers are doubles, so the round trip through a client would corrupt a number.
fn completion_data(
    declaration: DeclarationId,
    precise: bool,
    guess: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "declaration": declaration.get().to_string(),
        "precise": precise,
        // Absent rather than null when the receiver was not guessed, so that the common row
        // carries two fields and not three.
        "guess": guess,
    })
}

/// The declaration a resolve request names, and which tier its list came from.
///
/// Anything else — a stale id, an older client's bare string, nothing at all — answers `None`,
/// and the item comes back unenriched. The protocol says it comes back either way. The guess is
/// optional where `precise` is not: a client holding a list from an older server still
/// resolves, it just cannot say a thing it was never told.
fn completion_target(
    data: Option<&serde_json::Value>,
) -> Option<(DeclarationId, bool, Option<String>)> {
    let data = data?;
    let declaration = declaration_in(data.get("declaration"))?;
    let guess = data
        .get("guess")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Some((declaration, data.get("precise")?.as_bool()?, guess))
}

fn declaration_in(data: Option<&serde_json::Value>) -> Option<DeclarationId> {
    data?
        .as_str()?
        .parse::<u64>()
        .ok()
        .filter(|raw| *raw != 0)
        .map(DeclarationId::new)
}

/// Whether a request's answer is drawn from the graph, and so has to wait for it to be linked.
///
/// `foldingRange`, `selectionRange` and `semanticTokens/full` are pure functions of one buffer —
/// neither `analysis::ranges` nor `analysis::tokens` ever sees a `Graph` — so they are the three
/// that do not. Semantic tokens fire on the first keystroke in a newly opened file, which is
/// exactly when a cold index is still being built, and waiting would leave the file uncoloured
/// for as long as that takes.
/// Whether this request may be answered without indexing what has just been typed.
///
/// **The three a caret asks**, which is the set whose whole answer is a function of the
/// cursor and the graph. Each needs `Rebase::to_graph` on the way in; `hover` and
/// `definition` also need `span_to_buffer` on the way out, because they answer with a span
/// `locate` found in the graph and `definition`'s span can be in a different document
/// again — see `Analysis::link`.
///
/// The rest are not excluded on principle but on demand: `references`, `documentHighlight`
/// and the hierarchies answer about the *workspace* rather than about the caret, nobody
/// asks them between two keystrokes, and each would need every span it returns mapped
/// through the map of whichever document it came from. `signatureHelp` reads the buffer and
/// never the graph, and the four in `needs_the_graph`'s exemption list do not settle at
/// all.
fn defers(method: &str) -> bool {
    matches!(
        method,
        "textDocument/completion" | "textDocument/hover" | "textDocument/definition"
    )
}

fn needs_the_graph(method: &str) -> bool {
    !matches!(
        method,
        "textDocument/foldingRange"
            | "textDocument/selectionRange"
            | "textDocument/semanticTokens/full"
            | "textDocument/codeAction"
    )
}

fn completion_kind(kind: completion::Kind) -> CompletionItemKind {
    match kind {
        completion::Kind::Class => CompletionItemKind::CLASS,
        completion::Kind::Module => CompletionItemKind::MODULE,
        completion::Kind::Constant => CompletionItemKind::CONSTANT,
        completion::Kind::Method => CompletionItemKind::METHOD,
        completion::Kind::Variable => CompletionItemKind::VARIABLE,
        completion::Kind::Field => CompletionItemKind::FIELD,
        completion::Kind::Keyword => CompletionItemKind::KEYWORD,
    }
}

/// Whether a response carries no answer at all, in either of the two shapes one can take.
///
/// `reply(id, None)` serialises to `null`, and a `CompletionList` that matched nothing is an
/// empty `items`. A deferred request retries on both: the refusal that motivates the retry
/// produces the first, and an empty list is cheap enough to re-derive that telling them apart
/// would be a distinction with no consequence.
fn answered_nothing(response: &Response) -> bool {
    match &response.response_result {
        Ok(serde_json::Value::Null) => true,
        Ok(value) => value
            .get("items")
            .and_then(serde_json::Value::as_array)
            .is_some_and(Vec::is_empty),
        Err(_) => false,
    }
}

/// Which document and which position a request names, for the line that says it arrived.
///
/// Read out of the raw params rather than the parsed ones because this runs before `dispatch`
/// picks a handler, and the twenty-four params types have exactly these two fields in common.
/// A request that names neither — `workspace/symbol`, `completionItem/resolve` — says so with a
/// dash rather than being logged as a request about nothing.
fn asked_about(params: &serde_json::Value) -> (String, String) {
    let document = params
        .get("textDocument")
        .and_then(|document| document.get("uri"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("-")
        .to_owned();
    let position = params.get("position").map_or_else(
        || "-".to_owned(),
        |position| format!("{}:{}", position["line"], position["character"]),
    );
    (document, position)
}

/// What the client was actually told, in one word.
///
/// **`nothing` and `empty` are not the same answer** and the log has to keep them apart: `null`
/// is a cursor the server could not make anything of, an empty `items` is a list that matched
/// nothing, and the two are repaired in different modules. `cancelled` is the client changing
/// its mind and is not a defect; `failed` is an error response, which for everything except a
/// crash means a method this build does not answer.
fn outcome(response: &Response) -> &'static str {
    match &response.response_result {
        Err(error) if error.code == ErrorCode::RequestCanceled as i32 => "cancelled",
        Err(_) => "failed",
        Ok(serde_json::Value::Null) => "nothing",
        Ok(_) if answered_nothing(response) => "empty",
        Ok(_) => "answered",
    }
}

fn reply<T: serde::Serialize>(id: &RequestId, value: Option<T>) -> Response {
    match serde_json::to_value(value) {
        Ok(result) => Response {
            id: id.clone(),
            response_result: Ok(result),
        },
        Err(error) => Response::new_err(
            id.clone(),
            ErrorCode::InternalError as i32,
            format!("could not serialise the response: {error}"),
        ),
    }
}

/// Request params are client input: malformed ones get logged and answered with `null`, never
/// a panic.
fn parse_params<T: serde::de::DeserializeOwned>(params: serde_json::Value) -> Option<T> {
    match serde_json::from_value(params) {
        Ok(parsed) => Some(parsed),
        Err(error) => {
            tracing::warn!("malformed request params: {error}");
            None
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::REQUESTS_TO_CRASH;
    use crate::analysis::testing::*;
    use lsp_server::Message;

    /// Every method `dispatch` answers, read out of `dispatch` itself.
    ///
    /// Written out by hand this would be a second copy of the table, and the copy that goes
    /// stale the moment somebody adds a twenty-fifth method — which is exactly the method whose
    /// silence nobody would notice. `messages.rs` reads its own source for the same reason.
    fn dispatched_methods() -> Vec<String> {
        include_str!("requests.rs")
            .lines()
            .map(str::trim)
            .filter_map(|line| line.strip_prefix('"'))
            .filter_map(|rest| rest.split_once("\" => "))
            .filter(|(_, tail)| tail.starts_with("reply(") || *tail == "{")
            .map(|(method, _)| method.to_owned())
            .collect()
    }

    #[test]
    fn every_method_says_it_arrived_and_says_what_it_answered() {
        // The defect this is the test for: twenty-one of the twenty-four answered in silence, so
        // "hover does nothing in this file" and "the client never sent a hover" produced
        // identical logs — and the second is what a document selector one folder too narrow
        // really does. One pair per request, whatever the request turned out to be worth.
        let methods = dispatched_methods();
        assert_eq!(methods.len(), 24, "{methods:?}");

        let mut harness = Harness::new();
        let source = "class Story\n  def title\n  end\nend\n";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        // One params object with every field the twenty-four read between them. The ones it does
        // not fit answer `null`, which is the point: the pair is written whether or not the
        // request was worth anything.
        let params = serde_json::json!({
            "textDocument": { "uri": uri.as_str() },
            "position": { "line": 1, "character": 7 },
            "range": {
                "start": { "line": 1, "character": 2 },
                "end": { "line": 2, "character": 5 }
            },
            "context": { "diagnostics": [] },
            "newName": "heading",
            "query": "Story",
        });

        for method in &methods {
            let (_, logged) = crate::testing::captured_logs(tracing::Level::DEBUG, || {
                harness.ask_raw(method, params.clone());
            });
            let arrived: Vec<&str> = logged
                .lines()
                .filter(|line| line.contains(&format!("request method=\"{method}\"")))
                .collect();
            let answered: Vec<&str> = logged
                .lines()
                .filter(|line| line.contains(&format!("answered method=\"{method}\"")))
                .collect();
            assert_eq!(arrived.len(), 1, "{method}: {logged}");
            assert_eq!(answered.len(), 1, "{method}: {logged}");
            assert!(
                answered[0].contains("elapsed="),
                "{method} answered without saying how long it took: {logged}"
            );
            assert!(
                answered[0].contains("settled="),
                "{method} answered without saying whether the graph had to be linked: {logged}"
            );
        }
    }

    #[test]
    fn the_pair_names_the_document_and_the_position_it_was_asked_about() {
        // Paths and positions, never a line of the source: `logging.rs` holds that rule and this
        // is where the request half of it is asserted.
        let mut harness = Harness::new();
        let source = "class Story\n  def title\n  end\nend\n";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        let (_, logged) = crate::testing::captured_logs(tracing::Level::DEBUG, || {
            harness.ask(
                "textDocument/hover",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": 1, "character": 7 },
                }),
            );
        });
        assert!(logged.contains("app/models/story.rb\""), "{logged}");
        assert!(logged.contains("position=\"1:7\""), "{logged}");
        assert!(
            !logged.contains("class Story"),
            "the source is not the log's business: {logged}"
        );

        // And a request that names no document says so rather than being logged as one about
        // nothing.
        let (_, logged) = crate::testing::captured_logs(tracing::Level::DEBUG, || {
            harness.symbol_search("Story");
        });
        assert!(logged.contains("document=\"-\""), "{logged}");
        assert!(logged.contains("position=\"-\""), "{logged}");
    }

    #[test]
    fn a_cancelled_request_is_answered_in_the_same_shape_as_every_other_one() {
        let mut harness = Harness::new();
        let uri = harness.write("app/models/story.rb", "class Story\nend\n");
        harness.index();

        let id = RequestId::from(7);
        harness.analysis.cancellations.cancel(id.clone());
        let (_, logged) = crate::testing::captured_logs(tracing::Level::DEBUG, || {
            harness.analysis.serve(Request {
                id: id.clone(),
                method: "textDocument/hover".to_owned(),
                params: serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": 0, "character": 6 },
                }),
            });
        });
        assert!(logged.contains("outcome=\"cancelled\""), "{logged}");
        assert!(logged.contains("id=7"), "{logged}");
    }

    #[test]
    fn a_list_that_matched_nothing_and_a_cursor_on_nothing_are_different_words() {
        // The distinction the log exists to make: `null` is a cursor the server could not make
        // anything of and an empty `items` is a list that matched nothing. They are repaired in
        // different modules, and a log that called both "empty" would send every report to the
        // wrong one.
        let mut harness = Harness::new();
        let source = "class Story\nend\n";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        let (_, logged) = crate::testing::captured_logs(tracing::Level::DEBUG, || {
            harness.ask(
                "textDocument/hover",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": 1, "character": 3 },
                }),
            );
        });
        assert!(logged.contains("outcome=\"nothing\""), "{logged}");

        let (_, logged) = crate::testing::captured_logs(tracing::Level::DEBUG, || {
            harness.ask(
                "textDocument/documentSymbol",
                serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
            );
        });
        assert!(logged.contains("outcome=\"answered\""), "{logged}");
    }

    /// The request seam, which neither the indexing bulkhead nor `resolve`'s guard covers: an
    /// unguarded handler that panics takes the analysis thread with it, and a cursor on a `def`
    /// can reach one.
    ///
    /// Armed rather than provoked. The pinned rev fixes the unwraps that used to reach it — the
    /// test below asserts that Ruby *answers* — so the seam is exercised deliberately. It is kept
    /// because `create_declaration`'s unwraps survive upstream unchanged, and every handler under
    /// `dispatch` reads the graph.
    #[test]
    fn a_request_that_crashes_costs_its_own_answer_and_not_the_session() {
        let mut harness = Harness::new();
        let source = "class Bar; end\nAliased = Bar\nclass Aliased\n  def self.foo; end\nend\n";
        let uri = harness.write("app/bar.rb", source);
        let person = "class Person\n  def shout\n  end\nend\n";
        let elsewhere = harness.write("app/person.rb", person);
        harness.index();

        REQUESTS_TO_CRASH.set(1);
        let (response, logged) = crate::testing::captured_logs(tracing::Level::ERROR, || {
            harness.ask_raw(
                "textDocument/definition",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": position_of(source, "foo"),
                }),
            )
        });
        assert_eq!(REQUESTS_TO_CRASH.get(), 0, "the crash was armed and taken");

        let error = response.response_result.expect_err("the request crashed");
        assert_eq!(error.code, ErrorCode::InternalError as i32);
        assert!(
            error.message.contains("textDocument/definition"),
            "{error:?}"
        );
        assert!(logged.contains("answers nothing"), "{logged}");

        assert!(
            !harness.definition_at(&elsewhere, person, "shout").is_null(),
            "the thread is alive and every other request still works"
        );
    }

    /// A constant alias reopened under its alias, which the pinned rev answers.
    ///
    /// `Graph::find_self_receiver_declaration` unwraps twice on 0.2.5 and upstream's `ab88ef1`
    /// fixes it, so this pins the fix: a pin moved back to 0.2.5 fails here rather than killing
    /// the test thread quietly under the guard above.
    #[test]
    fn a_constant_alias_reopened_under_its_alias_answers_rather_than_crashing() {
        let mut harness = Harness::new();
        let source = "class Bar; end\nAliased = Bar\nclass Aliased\n  def self.foo; end\nend\n";
        let uri = harness.write("app/bar.rb", source);
        harness.index();

        let call = "Bar.foo\nAliased.foo\n";
        let call_uri = harness.write("app/call.rb", call);
        harness.index();

        // The cursor on the `def` is what the report reached the unwraps through. Upstream's
        // fix stops the panic by answering `None`, which left the line silent; `locator`
        // follows the alias itself and the `def` now declares on the class the alias names,
        // like every other `def self.` line.
        assert!(!harness.definition_at(&uri, source, "foo").is_null());
        // What a user actually writes does resolve, under either spelling of the constant.
        assert!(
            !harness.definition_at(&call_uri, call, "Bar.foo").is_null(),
            "the class's own name"
        );
        assert!(
            !harness
                .definition_at(&call_uri, call, "Aliased.foo")
                .is_null(),
            "and the alias it was reopened under"
        );
    }

    #[test]
    fn a_cancelled_request_is_answered_with_request_cancelled() {
        let harness = Harness::new();
        let id = RequestId::from(7);
        harness.analysis.cancellations.cancel(id.clone());

        let mut analysis = harness.analysis;
        analysis.serve(Request {
            id: id.clone(),
            method: "textDocument/hover".to_owned(),
            params: serde_json::Value::Null,
        });

        let Message::Response(response) = harness.outgoing.try_recv().expect("a response") else {
            panic!("expected a response");
        };
        assert_eq!(response.id, id);
        let error = response.response_result.expect_err("cancelled");
        assert_eq!(error.code, ErrorCode::RequestCanceled as i32);
    }

    #[test]
    fn a_position_with_nothing_under_it_answers_null_rather_than_erroring() {
        // An error response is something editors show the user. "No definition here" is not
        // an error, it is an answer.
        let (mut harness, uri) = library();
        for method in [
            "textDocument/hover",
            "textDocument/definition",
            "textDocument/documentSymbol",
        ] {
            let answer = harness.ask(
                method,
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": 4, "character": 0 },
                }),
            );
            if method == "textDocument/documentSymbol" {
                continue;
            }
            assert_eq!(answer, serde_json::Value::Null, "{method}");
        }

        // Malformed params are client input, and must not take the thread down.
        assert_eq!(
            harness.ask(
                "textDocument/hover",
                serde_json::json!({ "nonsense": true })
            ),
            serde_json::Value::Null
        );
    }

    #[test]
    fn a_response_that_cannot_be_serialised_becomes_an_error_rather_than_silence() {
        // Every request's answer goes out through `reply`. `serde_json::to_value` cannot fail
        // for any type it is handed today, but the protocol has no shape for "no response":
        // a client that sent an id waits on it forever. An `InternalError` is the only exit
        // that lets the editor carry on.
        let id = RequestId::from(7);
        let ok = reply(&id, Some("fine"));
        assert_eq!(ok.response_result.expect("a result"), "fine");

        // A map whose keys are not strings is what `serde_json` actually refuses — a non-finite
        // float is quietly written as `null` rather than rejected.
        let broken = reply(
            &id,
            Some(std::collections::BTreeMap::from([((1u8, 2u8), 3u8)])),
        );
        let error = broken.response_result.expect_err("an error");
        assert_eq!(error.code, ErrorCode::InternalError as i32);
        assert!(error.message.contains("could not serialise"), "{error:?}");
    }

    #[test]
    fn nothing_found_is_null_rather_than_an_empty_list() {
        // The same contract as every other handler: `[]` claims the project has no such symbol,
        // which is only ever true by accident.
        let mut harness = Harness::new();
        let source = "class Person\nend\n";
        let uri = harness.write("app/person.rb", source);
        harness.index();

        assert!(harness.symbol_search("nothing_is_called_this").is_null());
        assert!(
            harness
                .references_at(&uri, source, "Person", false)
                .is_null()
        );
    }
}
