//! The LSP request layer: one handler per method, and the table that picks it.
//!
//! - **Every handler opens with the same four lines**: [`parse_params`], the position,
//!   `DocUri::from_lsp`, and one of [`Analysis::with_text`] / [`Analysis::read_of`]. Then it calls
//!   one analysis module and shapes the result into the `lsp-types` type. Nothing here decides
//!   anything about Ruby; everything here decides what a client may be told.
//! - **Those four lines are deliberately not a macro or a trait.** They differ in the params type,
//!   and a handler that reads top to bottom is worth more than four saved lines.
//! - **[`Analysis::serve`] is the entry point**, the only item the thread calls. It carries the
//!   bulkhead, the settle policy and the deferred retry, because all three belong to answering *a
//!   request*, not to any one method.
//!
//! The rule for the whole file is [`reply`]: an absent answer is `null`, never an empty list.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    time::Instant,
};

use lsp_server::{ErrorCode, Request, RequestId, Response};
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, CodeAction,
    CodeActionKind, CodeActionOrCommand, CompletionItem, CompletionItemKind, CompletionItemTag,
    CompletionList, CompletionResponse, CompletionTextEdit, DocumentHighlight, DocumentLink,
    DocumentSymbolResponse, Documentation, FoldingRange, GotoDefinitionResponse, Hover,
    HoverContents, InlayHint, InlayHintKind, InlayHintLabel, Location, LocationLink, MarkupContent,
    MarkupKind, OneOf, OptionalVersionedTextDocumentIdentifier, PrepareRenameResponse,
    SelectionRange, SemanticToken, SemanticTokens, SignatureHelp, SymbolInformation,
    TextDocumentEdit, TextEdit, TypeHierarchyItem, WorkspaceEdit, WorkspaceSymbolResponse,
};
use rubydex::model::ids::{DeclarationId, StringId, UriId};

use super::{
    Analysis, MAX_COMPLETION_ITEMS, MAX_INCOMING_CALLS, MAX_REFERENCES, MAX_SUBTYPES,
    MAX_UNTYPED_CANDIDATES, MAX_UNTYPED_COMPLETION_ITEMS, MAX_WORKSPACE_SYMBOLS, code_actions,
    completion, cursor, environment, erb, hierarchy, highlight, hints, hover, locator,
    locator::Site,
    position::{self, ByteSpan, TextDocument},
    ranges, references, rename, requires, search, signature_help, symbols, tokens, types, views,
};
use crate::messages;
use crate::workspace::DocUri;

impl Analysis {
    pub(super) fn serve(&mut self, request: Request) {
        let arrived = Instant::now();
        self.arrived(&request);

        if self.cancellations.take(&request.id) {
            self.cancelled(request, arrived);
            return;
        }

        let id = request.id.clone();
        let method = request.method.clone();
        // The bulkhead's third seam, on the *request* path: ordinary Ruby (a class reopened under a
        // constant aliasing it) reaches an unwrap in `find_self_receiver_declaration` from
        // `textDocument/definition`, a path neither the indexing bulkhead nor `resolve`'s guard
        // covers.
        //
        // - **The failure unit is the request**: it answers nothing and says so.
        // - **`settle` is inside the guard**, because everything it does is reachable the same way:
        //   a request arriving dirty pays for the pass and the link, and only one of the three has
        //   its own guard.
        // - **A crashed settle has already cleared `dirty`**, so the next request answers against
        //   the graph as it stands instead of settling again. Deliberate, and the same policy
        //   `recovering` uses one level down: retrying per request would settle, crash and re-arm
        //   on every keystroke. A workspace change re-arms it.
        let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Answer against a settled graph: a stale answer is worse than a slightly slower one.
            // `dirty`, not `resolve_at`, because background gem indexing deliberately does not arm
            // the timer, yet its files stay unlinked until something resolves them.
            //
            // - **Four requests are exempt**, because their answer does not come from the graph
            //   (or, for `codeAction`, four fifths of it does not, and the fifth is not worth a
            //   re-index per cursor move). Waiting for an unread resolve would cost every
            //   keystroke, up to two orders of magnitude on a large file. `settles` has the split,
            //   a *narrower* question than `needs_the_graph` below: an index a keystroke behind
            //   costs one code action; an unstarted server costs the whole menu.
            // - **The third is *deferred*, not exempt**; the retry below is the difference.
            //
            // **A document just opened whose own facts wait on the settle** is answered after them
            // (`reopens`): the deferred path would read a graph that never held them, and answer a
            // spec's `let` with a name guess one settle short of its one place. The settle is the
            // cheap kind (RSpec alone), paid once: nothing is reopened after it.
            let fresh = defers(&method) && self.reopens(&request.params);
            let deferred = self.dirty && defers(&method) && !fresh;
            let mut settled = false;
            if self.dirty && settles(&method) && (!defers(&method) || fresh) {
                self.settle();
                settled = true;
            }
            // **The graph is not complete yet**: a different shortfall from an index a keystroke
            // behind, and settling does not fix it (`settle` links the graph as it stands and,
            // before the pipeline finishes, does not even generate). `Stage::is_ready` is false
            // from startup until the last stage has run: exactly "more is coming".
            //
            // Keyed on that, not on `dirty`, because `step_bundle` marks dirty per batch *without*
            // arming the timer, so any settle in between clears the flag while most of the bundle
            // is still pending. Wider than `defers`, which names three methods about a caret racing
            // the index: this is about a cold server, where `references`, the hierarchies and
            // `workspace/symbol` come back just as empty.
            let cold = !self.stage.is_ready() && needs_the_graph(&method);
            let again = (deferred || cold).then(|| request.params.clone());
            self.unplaced.set(false);
            let mut response = self.dispatch(&id, request);
            let mut retried = false;
            let mut drained = false;
            if let Some(params) = again
                && answered_nothing(&response)
            {
                // **A `Rebase` is an optimisation, not a filter.** It refuses an offset inside text
                // the graph has not seen (almost always a just-typed constant, since `self`, locals
                // and instance variables resolve against the buffer), and from here a refusal looks
                // like a cursor with nothing to complete. Both are repaired the same way: settle
                // and ask again, with the two coordinate systems back in step.
                //
                // So the deferred path answers *sooner* than the eager one and never *less*, which
                // is what makes it safe to leave on. Without this it would trade a real answer for
                // a fast empty list.
                //
                // **Except where the cursor was read and its member has no place** (`unplaced`,
                // RSpec's `describe`): nothing was refused, and a settle finds the same member with
                // the same nowhere to go. The answer is the last settled graph's, as it is where a
                // stale member has a place, and the debounce settles as it would have. Paying a
                // settle for it cost the audit 193 settles.
                if deferred && !self.unplaced.get() {
                    self.settle();
                    response = self.dispatch(&id, asked_again(&id, &method, params.clone()));
                    settled = true;
                    retried = true;
                }
                // **The rung a cold server pays, once.** The run loop steps the background index
                // only `if receiver.is_empty()` (`Analysis::run`), so a client whose `initialize`,
                // `initialized`, `didOpen` and first request arrive within a second lets no batch
                // through, and the first answer would use a graph holding only the workspace. An
                // agent records that empty answer as fact and does not ask again.
                //
                // - **Only a request that would otherwise answer nothing waits**, and the rung
                //   disappears with the pipeline: the next request finds it `None`. Answers the
                //   workspace alone can give are as fast as ever.
                // - **It drains instead of waiting.** `step_pipeline` runs on this thread, so
                //   blocking for background work would block the only thread that can do it. The
                //   cost: tasks queued behind this one (a cancellation among them) are not seen
                //   until it returns (`concurrency.md`).
                if cold && answered_nothing(&response) {
                    while self.step_pipeline() {}
                    self.settle();
                    response = self.dispatch(&id, asked_again(&id, &method, params));
                    settled = true;
                    drained = true;
                }
            }
            (response, settled, retried, drained)
        }));

        let (response, settled, retried, drained) = match served {
            Ok(answered) => answered,
            Err(_) => {
                // Not a `messages::` sentence: this answers one request to the client, the boundary
                // `messages.md` draws. The panic hook has already printed rubydex's file and line
                // to stderr.
                tracing::error!("answering {method} crashed; that request answers nothing");
                (
                    Response::new_err(
                        id.clone(),
                        ErrorCode::InternalError as i32,
                        format!("ya-lsp crashed while answering {method}"),
                    ),
                    false,
                    false,
                    false,
                )
            }
        };

        // The other half of the pair: the fields saying *what happened*. How long; whether the
        // graph had to be linked first; which rung answered (first attempt, deferred retry, or the
        // drain of a bundle not yet in the graph); and which of five things the client was told.
        // `nothing` and `empty` are kept apart: one is a cursor on nothing, the other a list that
        // matched nothing, and they are fixed in different places.
        let outcome = outcome(&response);
        let elapsed = format!("{:.2?}", arrived.elapsed());
        tracing::debug!(
            method = method,
            id = %id,
            outcome,
            settled,
            retried,
            drained,
            elapsed,
            "answered"
        );

        self.cancellations.forget(&id);
        self.respond(response);
    }

    /// A held `inlayHint` that a newer one for the same range replaced, or that the client
    /// cancelled while it waited (`Analysis::hold`). Answered without being computed, through the
    /// same two log lines as every other request.
    ///
    /// `ContentModified` is the protocol's word for an answer a later state made worthless, and
    /// clients drop it without showing an error.
    pub(super) fn supersede(&mut self, request: Request) {
        let arrived = Instant::now();
        self.arrived(&request);
        if self.cancellations.take(&request.id) {
            self.cancelled(request, arrived);
            return;
        }
        self.refuse(
            request,
            arrived,
            ErrorCode::ContentModified,
            "a newer request for the same range replaced this one",
        );
    }

    /// **The one line saying a request arrived.** Without it, "hover does nothing in this file" is
    /// indistinguishable from "the client never sent a hover", which is what a document selector
    /// one folder too narrow actually does.
    ///
    /// Written by the two doors a request leaves through, not per method. Fields, not prose:
    /// grepping `method=textDocument/hover` gives the pair.
    fn arrived(&self, request: &Request) {
        let (document, position) = asked_about(&request.params);
        tracing::debug!(
            method = request.method,
            id = %request.id,
            document,
            position,
            dirty = self.dirty,
            "request"
        );
    }

    /// Answer a request the client cancelled. `cancelled` is a normal thing for a client to do.
    fn cancelled(&mut self, request: Request, arrived: Instant) {
        self.refuse(
            request,
            arrived,
            ErrorCode::RequestCanceled,
            "request cancelled by the client",
        );
    }

    /// Answer `request` with an error without computing it.
    ///
    /// Logged in the same shape as every other outcome: following one id through the log always
    /// gives the same two lines.
    fn refuse(&mut self, request: Request, arrived: Instant, code: ErrorCode, message: &str) {
        let response = Response::new_err(request.id, code as i32, message.to_owned());
        let outcome = outcome(&response);
        let elapsed = format!("{:.2?}", arrived.elapsed());
        tracing::debug!(
            method = request.method,
            id = %response.id,
            outcome,
            settled = false,
            retried = false,
            drained = false,
            elapsed,
            "answered"
        );
        self.respond(response);
    }

    /// Which handler answers `request`. Split from [`Analysis::serve`] only so the bulkhead there
    /// is one expression, not a closure around a long `match`.
    fn dispatch(&mut self, id: &RequestId, request: Request) -> Response {
        #[cfg(test)]
        super::crash_the_next_request_if_asked();
        match request.method.as_str() {
            "textDocument/documentSymbol" => reply(id, self.document_symbols(request.params)),
            "textDocument/hover" => reply(id, self.hover(request.params)),
            "textDocument/definition" => reply(id, self.goto_definition(request.params)),
            "textDocument/implementation" => reply(id, self.goto_implementation(request.params)),
            "textDocument/typeDefinition" => reply(id, self.goto_type_definition(request.params)),
            "textDocument/declaration" => reply(id, self.goto_declaration(request.params)),
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
            "textDocument/signatureHelp" => reply(id, self.signature_help(request.params)),
            "textDocument/codeAction" => reply(id, self.code_actions(request.params)),
            "textDocument/prepareRename" => reply(id, self.prepare_rename(request.params)),
            "textDocument/rename" => reply(id, self.rename(request.params)),
            "textDocument/completion" => reply(id, self.completion(request.params)),
            "completionItem/resolve" => reply(id, self.resolve_completion(request.params)),
            "workspace/textDocumentContent" => self.text_document_content(id, request.params),
            "workspace/executeCommand" => reply(id, self.execute_command(request.params)),
            "workspace/willRenameFiles" => reply(id, self.will_rename_files(request.params)),
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
        // At most one reread of this document, and only if it declares a private method: the
        // outline prints the word, and rubydex's record is wrong for one shape of it.
        let read = |uri: &str| self.read_of(uri);
        let modifiers = locator::Modifiers::new(&read, &self.exits);
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
        let memo = types::Memo::new(&read, &self.exits);
        let sources = self.sources(&read, &memo);
        let modifiers = &memo.modifiers;
        self.with_text(&uri, |text| {
            let parsed = cursor::Parsed::new(text.text());
            let offset = text.offset_at(position);
            // The graph may be a keystroke behind the buffer, so the cursor goes *in* through the
            // map and every span comes back *out* through it.
            let rebase = self.rebase_for(&uri, text.text());
            let at = rebase.to_graph(offset)?;
            let uri_id = UriId::from(uri.as_str());
            let answer = |value: String, (start, end): (u32, u32)| Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value,
                }),
                range: Some(text.range_at(start, end)),
            };
            // The scope walk first, in the same order as `definition` and `documentHighlight`: what
            // an instance variable holds is what ya-lsp derived, and its card is the variable with
            // that type, never the card of the class it holds.
            //
            // It falls through when nothing could be derived, like the `find_map` below: at a
            // write, the graph still has rubydex's declaration for the assignment, and
            // `Shelf::Book#@title` beats no card.
            if let Some((variable, typed)) =
                locator::variable_type(&sources, uri_id, &parsed, offset, at, &rebase)
                && let Some(markdown) = hover::variable(
                    &self.synthesized,
                    &sources,
                    locator::variable_declaration(&sources, uri_id, &variable, &rebase),
                    text.text()
                        .get(variable.start as usize..variable.end as usize)
                        .unwrap_or_default(),
                    Some(&typed),
                    Some(uri.as_str()),
                )
            {
                return Some(answer(markdown, (variable.start, variable.end)));
            }
            // An instance variable a symbol names (`instance_variable_get(:@x)`, a macro's
            // `:@x`), which no read spells: the card a read of it gets.
            if let Some((symbol, named)) =
                locator::named_variable(&sources, uri_id, &parsed, offset, &rebase)
                && let Some(markdown) = hover::variable(
                    &self.synthesized,
                    &sources,
                    types::named_writes(&sources, uri_id, &named).declaration,
                    &named.name,
                    types::named_read(&sources, uri_id, &named).as_ref(),
                    Some(uri.as_str()),
                )
            {
                return Some(answer(markdown, (symbol.start, symbol.end)));
            }
            // A bare name in a partial that its render calls pass as a local: a
            // variable's card, before the graph, as Ruby reads the local before any method.
            if let Some(local) = locator::partial_local(&sources, uri_id, &parsed, offset)
                && let Some(markdown) = hover::variable(
                    &self.synthesized,
                    &sources,
                    None,
                    text.text()
                        .get(local.span.0 as usize..local.span.1 as usize)
                        .unwrap_or_default(),
                    Some(&local.typed),
                    Some(uri.as_str()),
                )
            {
                return Some(answer(markdown, local.span));
            }
            // A local, a block's parameter or a method's parameter: what it holds.
            if let Some(((start, end), typed, parameter)) =
                locator::local_type(&sources, uri_id, &parsed, offset, at, &rebase)
                && let Some(markdown) = hover::local(
                    sources.graph,
                    text.text()
                        .get(start as usize..end as usize)
                        .unwrap_or_default(),
                    &typed,
                    parameter,
                )
            {
                return Some(answer(markdown, (start, end)));
            }
            // What the call under the cursor returns, for a method that declares nothing of its own
            //. `None` off a call's name.
            let at_this_call = locator::call_type(&sources, uri_id, &parsed, offset, at, &rebase);
            // Several targets can share the narrowest span; take the first with something to say,
            // not the first that exists.
            let located =
                locator::locate_written(&self.graph, uri_id, at, &parsed, offset, &rebase);
            let found = located.into_iter().find_map(|located| {
                let found = rebase.span_to_buffer(ByteSpan {
                    start: located.start,
                    end: located.end,
                })?;
                let (start, end) = (found.start, found.end);
                let resolution =
                    locator::resolve_typed(&sources, uri_id, &parsed, &located, start, &rebase)?;
                let markdown = hover::markdown(
                    &self.synthesized,
                    modifiers,
                    &sources,
                    &resolution,
                    Some(uri.as_str()),
                    at_this_call.as_ref(),
                )?;
                Some(answer(markdown, (start, end)))
            });
            // A call rubydex recorded nothing for (`a.b::C`, an upstream defect), read from the
            // parse, where the graph held nothing at the cursor.
            let found = found.or_else(|| {
                let (message, resolution) =
                    locator::resolve_misplaced(&sources, uri_id, &parsed, offset, &rebase)?;
                let markdown = hover::markdown(
                    &self.synthesized,
                    modifiers,
                    &sources,
                    &resolution,
                    Some(uri.as_str()),
                    at_this_call.as_ref(),
                )?;
                Some(answer(markdown, message))
            });
            // A macro's `:symbol`, after the graph, not before, as in `definition`.
            found
                .or_else(|| {
                    let (symbol, resolution) =
                        locator::symbol_at(&sources, uri_id, &parsed, offset, at, &rebase)?;
                    let markdown = hover::markdown(
                        &self.synthesized,
                        modifiers,
                        &sources,
                        &resolution,
                        Some(uri.as_str()),
                        None,
                    )?;
                    Some(answer(markdown, (symbol.start, symbol.end)))
                })
                // A literal key a member looks up: what the main locale holds there.
                .or_else(|| {
                    let (literal, member) =
                        locator::resolve_keyed(&sources, uri_id, &parsed, offset, &rebase)?;
                    let entry = self.keyed_entry(&literal, &member)?;
                    Some(answer(
                        hover::keyed(&entry.shown),
                        (literal.start, literal.end),
                    ))
                })
                // Last, after every rung that reads the graph at the cursor: an instance variable
                // read nothing typed gets the card its write gets, where `definition` jumps.
                .or_else(|| {
                    let (variable, resolution) =
                        locator::written_variable(&sources, uri_id, &parsed, offset, &rebase)?;
                    let markdown = hover::markdown(
                        &self.synthesized,
                        modifiers,
                        &sources,
                        &resolution,
                        Some(uri.as_str()),
                        None,
                    )?;
                    Some(answer(markdown, (variable.start, variable.end)))
                })
        })?
    }

    /// The declarations the cursor's call or constant resolves to, spelled as a card spells them
    /// and sorted: what a list card counts, and what its rows said before it lost them. The graph
    /// half of [`Self::hover`] only, for tests.
    #[cfg(test)]
    pub(crate) fn resolved_names(
        &self,
        uri: &DocUri,
        position: lsp_types::Position,
    ) -> Vec<String> {
        let read = |uri: &str| self.read_of(uri);
        let memo = types::Memo::new(&read, &self.exits);
        let sources = self.sources(&read, &memo);
        self.with_text(uri, |text| {
            let parsed = cursor::Parsed::new(text.text());
            let offset = text.offset_at(position);
            let rebase = self.rebase_for(uri, text.text());
            let at = rebase.to_graph(offset)?;
            let uri_id = UriId::from(uri.as_str());
            let resolution =
                locator::locate_written(&self.graph, uri_id, at, &parsed, offset, &rebase)
                    .into_iter()
                    .find_map(|located| {
                        let start = rebase
                            .span_to_buffer(ByteSpan {
                                start: located.start,
                                end: located.end,
                            })?
                            .start;
                        locator::resolve_typed(&sources, uri_id, &parsed, &located, start, &rebase)
                    })?;
            let mut names: Vec<String> = resolution
                .declarations
                .iter()
                .filter_map(|id| self.graph.declarations().get(id))
                .map(|declaration| super::render::qualified_name(&self.graph, declaration.name()))
                .collect();
            names.sort();
            names.dedup();
            Some(names)
        })
        .flatten()
        .unwrap_or_default()
    }

    /// What the body of knowledge keeping a member's keys says about the literal key a call passes
    /// it ([`crate::knowledge::Knowledge::keyed_entry`]).
    fn keyed_entry(
        &self,
        literal: &cursor::KeyedLiteral,
        member: &str,
    ) -> Option<crate::knowledge::KeyedEntry> {
        let asked = crate::knowledge::Keyed {
            member,
            key: &literal.key,
            keywords: &literal.keywords,
            // Where a key is written does not hang on what the call hands its value to.
            block: false,
        };
        self.knowledge
            .modules()
            .find_map(|module| module.keyed_entry(&asked))
    }

    /// `textDocument/signatureHelp`.
    ///
    /// Answered from the buffer, not the graph's copy, like completion: the call under the cursor
    /// is half-written, and which argument the user is on is a fact about the text this keystroke.
    fn signature_help(&self, params: serde_json::Value) -> Option<SignatureHelp> {
        let params: lsp_types::SignatureHelpParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let read = |uri: &str| self.read_of(uri);
        let modifiers = locator::Modifiers::new(&read, &self.exits);
        let blocks = locator::Blocks::new(&read);
        self.with_text(&uri, |text| {
            let call = cursor::call_at(text.text(), text.offset_at(position))?;
            let method = locator::precise_call(
                &self.graph,
                UriId::from(uri.as_str()),
                call.name,
                self.layout(),
                // Read from the call node of the same parse, so the card and the jump agree at one
                // cursor: no signature is drawn for a private method where Ruby would refuse the
                // call. See `cursor::Call::allows_private`.
                locator::Privacy::written(call.allows_private, &modifiers),
                &blocks,
            )?;
            signature_help::help(&self.graph, method, &call.active)
        })?
    }

    /// `textDocument/documentHighlight`.
    ///
    /// Answered from the buffer, like completion and signature help: the half from `scopes`
    /// describes the text this keystroke, and a highlight drawn over stale offsets lands on the
    /// wrong words.
    fn document_highlights(&self, params: serde_json::Value) -> Option<Vec<DocumentHighlight>> {
        let params: lsp_types::DocumentHighlightParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let read = |uri: &str| self.read_of(uri);
        let memo = types::Memo::new(&read, &self.exits);
        let sources = self.sources(&read, &memo);
        let found = self.with_text(&uri, |text| {
            let parsed = cursor::Parsed::new(text.text());
            let uri_id = UriId::from(uri.as_str());
            let offset = text.offset_at(position);
            let rebase = self.rebase_for(&uri, text.text());
            let named = || locator::resolve_named(&sources, uri_id, &parsed, offset, &rebase);
            let handed = |names: &[StringId], scope: &HashSet<UriId>| self.named_uses(names, scope);
            let rebound = locator::rebinding(&sources, uri_id, &rebase);
            let variable = || {
                let (symbol, named) =
                    locator::named_variable(&sources, uri_id, &parsed, offset, &rebase)?;
                let here: Vec<(u32, u32)> = types::named_writes(&sources, uri_id, &named)
                    .places
                    .into_iter()
                    .find(|(written, _)| written == uri.as_str())
                    .map(|(_, spans)| spans)
                    .unwrap_or_default();
                Some((symbol, here))
            };
            let unspelled = |read: u32| {
                types::instance_writes(&sources, uri_id, read)
                    .places
                    .into_iter()
                    .find(|(written, _)| written == uri.as_str())
                    .map(|(_, spans)| spans)
                    .unwrap_or_default()
            };
            let found = highlight::find(
                &self.graph,
                &self.synthesized,
                uri_id,
                &parsed,
                offset,
                self.layout(),
                &highlight::Asked {
                    named: &named,
                    handed: &handed,
                    rebound: &rebound,
                    variable: &variable,
                    unspelled: &unspelled,
                },
            );
            found
                .into_iter()
                .map(|at| DocumentHighlight {
                    range: text.range_at(at.start, at.end),
                    kind: Some(at.kind),
                })
                .collect::<Vec<_>>()
        })?;

        // `null`, not `[]`, as completion answers inside a comment: it tells the client nothing was
        // known here, so it may fall back to matching words itself.
        (!found.is_empty()).then_some(found)
    }

    /// `textDocument/selectionRange`.
    ///
    /// One chain per position, in the order asked: the protocol pairs the two arrays by index and
    /// has no way to say "not this one", so every position answers, with the whole buffer where
    /// there was nothing else.
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
    /// `null`, not `[]`, and it matters most here: a client with a folding provider stops guessing
    /// from indentation, so an empty array would remove the fallback *and* add nothing. `null` can
    /// only give it back.
    fn folding_ranges(&self, params: serde_json::Value) -> Option<Vec<FoldingRange>> {
        let params: lsp_types::FoldingRangeParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;
        // Declined in a template, which the `null` above makes possible. The walk sees only the
        // Ruby: in a template with a five-line `<div>`, it offers folds for the `<% %>` blocks and
        // none for the markup. Returning the editor's indentation guess folds the whole file, those
        // two included.
        if erb::is_template_uri(&uri) {
            return None;
        }
        let found = self.with_text(&uri, ranges::folds)?;

        (!found.is_empty()).then_some(found)
    }

    /// `textDocument/semanticTokens/full`.
    ///
    /// The whole document every time, no delta (see [`tokens`] for why). The relative encoding is
    /// the protocol's: each token is a delta from the previous one, which is why [`tokens::of`]
    /// sorts.
    fn semantic_tokens(&self, params: serde_json::Value) -> Option<SemanticTokens> {
        let params: lsp_types::SemanticTokensParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;

        self.with_text(&uri, |text| {
            let mut data: Vec<SemanticToken> = Vec::new();
            let (mut line, mut start) = (0, 0);
            for token in tokens::of(text.text()) {
                let at = text.position_at(token.start);
                // A token never spans a line (every one is an identifier), so the length is the
                // difference between two characters on one line, in the client's negotiated unit.
                // `end - start` in bytes would be wrong for every non-ASCII name, and `имя` is a
                // legal local.
                let length = text
                    .position_at(token.end)
                    .character
                    .saturating_sub(at.character);
                // Saturating, all three. The list is sorted, so none can go backwards, but a
                // subtraction that could underflow sits on the analysis thread, where a panic is
                // not a wrong colour but a server that stops answering.
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
        let memo = types::Memo::new(&read, &self.exits);
        let sources = self.sources(&read, &memo);

        // The scope walk first, for `highlight::find`'s reason: an instance variable is the one
        // ordinary thing the graph does not model, and at `@name = 1` (the one span both could
        // answer) the graph resolves to the single write and none of the reads.
        //
        // - **Nothing here is rebased, or may be.** The writes come from the client's buffer, in
        //   the cursor's own file, so they are already in the answer's coordinates, as for
        //   `documentHighlight`.
        // - **The name comes back beside the links** because the fall-through needs it and the
        //   buffer is open here: a template's writes are in another document, and re-reading this
        //   one to spell `@story` would blank the whole file again for six bytes.
        if let Some((origin, name, read, links)) = self
            .with_text(&uri, |text| {
                let parsed = cursor::Parsed::new(text.text());
                let offset = text.offset_at(position);
                let rebase = self.rebase_for(&uri, text.text());
                let rebound = locator::rebinding(&sources, UriId::from(uri.as_str()), &rebase);
                let variable = locator::variable_at(&parsed, offset, &rebound)?;
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
                        // The name alone: an instance variable's construct *is* its name, and the
                        // assigned value is not what was navigated to.
                        target_range: text.range_at(start, end),
                        target_selection_range: text.range_at(start, end),
                    })
                    .collect();
                Some((origin, name, variable.start, links))
            })
            .flatten()
        {
            // A file that only reads `@foo` has nothing of its own (a superclass or a controller
            // assigned it), so it falls through rather than declining, as the loop below does for a
            // target with no site.
            if !links.is_empty() {
                return self.definition_response(links);
            }
            // The same walk, continued into the one document a template's writes can be in, still
            // before the graph: a *read* of `@story` is a span rubydex files nothing under, so no
            // graph answer is displaced.
            if let Some(links) = self.template_variable_links(&uri, origin, &name, &sources) {
                return self.definition_response(links);
            }
            // And into every file of the object's classes: the writes the type side folds for this
            // read, a parent's, a concern's or a subclass's, or every renderer's for a
            // template.
            let writes = types::instance_writes(&sources, UriId::from(uri.as_str()), read);
            if let Some(links) = self.links_to_writes(origin, writes.places) {
                return self.definition_response(links);
            }
        }
        // An instance variable a symbol names, which no read spells: where it is written, as for a
        // read of it. The graph holds nothing at the symbol, so nothing is displaced.
        if let Some((origin, places)) = self
            .with_text(&uri, |text| {
                let parsed = cursor::Parsed::new(text.text());
                let uri_id = UriId::from(uri.as_str());
                let rebase = self.rebase_for(&uri, text.text());
                let offset = text.offset_at(position);
                let (symbol, named) =
                    locator::named_variable(&sources, uri_id, &parsed, offset, &rebase)?;
                Some((
                    text.range_at(symbol.start, symbol.end),
                    types::named_writes(&sources, uri_id, &named).places,
                ))
            })
            .flatten()
            && let Some(links) = self.links_to_writes(origin, places)
        {
            return self.definition_response(links);
        }
        // A bare name in a partial that its render calls pass as a local: where each passes it
        //, before the graph, which would answer a method of the name.
        if let Some((origin, places)) = self
            .with_text(&uri, |text| {
                let parsed = cursor::Parsed::new(text.text());
                let offset = text.offset_at(position);
                let local =
                    locator::partial_local(&sources, UriId::from(uri.as_str()), &parsed, offset)?;
                Some((text.range_at(local.span.0, local.span.1), local.places))
            })
            .flatten()
            && let Some(links) = self.links_to_writes(origin, places)
        {
            return self.definition_response(links);
        }

        let (origin, sites) = self.with_text(&uri, |text| {
            let parsed = cursor::Parsed::new(text.text());
            let offset = text.offset_at(position);
            let rebase = self.rebase_for(&uri, text.text());
            let at = rebase.to_graph(offset)?;

            let uri_id = UriId::from(uri.as_str());
            let mut unplaced = false;
            for located in
                locator::locate_written(&self.graph, uri_id, at, &parsed, offset, &rebase)
            {
                let found = rebase.span_to_buffer(ByteSpan {
                    start: located.start,
                    end: located.end,
                })?;
                let (start, end) = (found.start, found.end);
                // The same rung hover reads. A jump and a card disagreeing about what `person.` is
                // would be worse than either being absent.
                let resolved =
                    locator::resolve_typed(&sources, uri_id, &parsed, &located, start, &rebase)?
                        .declarations;
                let mut sites: Vec<Site> = locator::all_places(
                    &self.graph,
                    &self.synthesized,
                    self.layout(),
                    resolved.clone(),
                    Some(uri.as_str()),
                );
                // A member with no place of its own whose first argument names one:
                // `create(:user)` goes to the `factory :user` call.
                if sites.is_empty() {
                    sites = self.literal_places(&resolved, text.text(), start);
                }
                // One whose code a gem writes in a `def` rubydex files elsewhere: RSpec's
                // `expect` goes to rspec-expectations' `def expect`.
                if sites.is_empty() {
                    sites = self.written_places(&resolved, uri.as_str());
                }
                if !sites.is_empty() {
                    return Some((text.range_at(start, end), sites));
                }
                unplaced |= !resolved.is_empty();
            }
            // **Found, with nowhere to go**: a member Ruby defines at run time. Settling cannot
            // give it a place, so the deferred retry would pay a settle to answer nothing again.
            // Said only once every target covering the cursor was read: a refused rebase above
            // returns first and keeps its retry.
            self.unplaced.set(unplaced);

            // A call rubydex recorded nothing for (`a.b::C`, an upstream defect), read from the
            // parse by the rungs hover reads it with.
            if let Some((message, resolution)) =
                locator::resolve_misplaced(&sources, uri_id, &parsed, offset, &rebase)
            {
                let sites: Vec<Site> = locator::all_places(
                    &self.graph,
                    &self.synthesized,
                    self.layout(),
                    resolution.declarations,
                    Some(uri.as_str()),
                );
                if !sites.is_empty() {
                    return Some((text.range_at(message.0, message.1), sites));
                }
            }

            // Nothing in the graph covers the cursor, and the two ordinary things it never covers
            // are *arguments*: rubydex records a call, not what is inside it. A macro's `:symbol`
            // and a `require`'s path are asked here, not before the loop, because unlike the scope
            // walk the graph is not wrong about them, just silent.
            //
            // **These sites are rebased; the writes above are not.** What a symbol names is a graph
            // declaration, possibly in another file, and `link` moves each into its own document's
            // coordinates, as for a constant.
            if let Some((symbol, resolution)) =
                locator::symbol_at(&sources, uri_id, &parsed, offset, at, &rebase)
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

            // A literal key a member looks up (`t("users.title")`): where the key is written, in a
            // file the graph does not hold.
            if let Some((literal, member)) =
                locator::resolve_keyed(&sources, uri_id, &parsed, offset, &rebase)
                && let Some(entry) = self.keyed_entry(&literal, &member)
            {
                let site = Site {
                    uri: entry.uri,
                    full: entry.at,
                    selection: entry.at,
                };
                return Some((text.range_at(literal.start, literal.end), vec![site]));
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

    /// Whether a request asks about a document the editor just opened whose own generated facts
    /// wait on the next settle ([`Analysis::reopened`]: a spec's groups, which are built only for
    /// the files the editor holds).
    fn reopens(&self, params: &serde_json::Value) -> bool {
        params
            .get("textDocument")
            .and_then(|document| document.get("uri"))
            .and_then(serde_json::Value::as_str)
            .and_then(|written| written.parse::<lsp_types::Uri>().ok())
            .and_then(|written| DocUri::from_lsp(&written))
            .is_some_and(|uri| self.reopened.contains(uri.as_str()))
    }

    /// Every method name in `scope`'s documents handed as a symbol to a call that takes one
    /// (`send(:shout)`, `try(:shout)`, `method(:shout)`), whose name is one of `names`: the uses
    /// of a method no call reference records ([`references::Named`]).
    ///
    /// - **By name, as a method's other references are**: the call is matched by what it is
    ///   spelled, not by what it resolves to ([`types::naming_calls`]).
    /// - **Only a document that makes such a call is read**, found in the graph's own call index,
    ///   and read as the graph holds it (`read_of`); a place is moved back into the graph's
    ///   coordinates, where every other reference is, or dropped where an edit moved it.
    /// - **Read once per version** ([`types::HeldExits::handed`]): a references request on a
    ///   common name would otherwise parse every file that calls `send` or `respond_to?`.
    fn named_uses(&self, names: &[StringId], scope: &HashSet<UriId>) -> Vec<references::Reference> {
        let calls = types::naming_calls(&self.graph, &self.types);
        // The documents that make such a call, from the graph's own index of calls by name, held
        // for the graph (`Indexed::calls_named`): walking every document's calls on every request
        // cost the largest corpus's references a quarter of their time.
        let mut documents: Vec<UriId> = calls
            .iter()
            .flat_map(|call| {
                self.graph
                    .calls_named(call)
                    .iter()
                    .map(|(uri_id, _, _)| *uri_id)
                    .filter(|uri_id| scope.contains(uri_id))
                    .collect::<Vec<_>>()
            })
            .collect();
        documents.sort_unstable();
        documents.dedup();
        let mut found = Vec::new();
        for (uri_id, document) in documents
            .into_iter()
            .filter_map(|uri_id| Some((uri_id, self.graph.documents().get(&uri_id)?)))
        {
            let uri = document.uri();
            let read = || -> Option<types::HeldHanded> {
                let (source, rebase) = self.read_of(uri)?;
                Some(
                    cursor::symbols_handed(&source)
                        .into_iter()
                        .filter_map(|(call, name, start, end)| {
                            Some((call, name, rebase.to_graph(start)?, rebase.to_graph(end)?))
                        })
                        .collect(),
                )
            };
            let Some(handed) = self.exits.handed(uri_id, document.content_hash(), read) else {
                continue;
            };
            for (call, name, start, end) in handed.iter() {
                if calls.contains(call) && names.contains(&StringId::from(name.as_str())) {
                    found.push(references::Reference {
                        uri: uri.to_owned(),
                        start: *start,
                        end: *end,
                        write: false,
                    });
                }
            }
        }
        found
    }

    /// Where the Symbol a call passes to a generated member is written, as the body of knowledge
    /// that declared the member knows ([`crate::knowledge::Knowledge::literal_place`]).
    fn literal_places(&self, resolved: &[DeclarationId], source: &str, name: u32) -> Vec<Site> {
        let Some(literal) = cursor::first_symbol(source, name) else {
            return Vec::new();
        };
        resolved
            .iter()
            .filter_map(|id| {
                let declaration = self.graph.declarations().get(id)?;
                let (owner, method) = declaration.name().rsplit_once('#')?;
                let method = method.trim_end_matches("()");
                let (uri, (full, selection)) = self
                    .knowledge
                    .modules()
                    .find_map(|module| module.literal_place(owner, method, &literal))?;
                Some(Site {
                    uri,
                    full,
                    selection,
                })
            })
            .collect()
    }

    /// Where a gem writes the code of a generated member with no place, as the body of knowledge
    /// that declared the member knows ([`crate::knowledge::Knowledge::written_in`]): that
    /// declaration's own places, fenced as every jump's are.
    fn written_places(&self, resolved: &[DeclarationId], cursor: &str) -> Vec<Site> {
        let written: Vec<DeclarationId> = resolved
            .iter()
            .filter_map(|id| {
                let declaration = self.graph.declarations().get(id)?;
                let (owner, method) = declaration.name().rsplit_once('#')?;
                let method = method.trim_end_matches("()");
                let name = self
                    .knowledge
                    .modules()
                    .find_map(|module| module.written_in(owner, method))?;
                // A declaration the bundle lacks has no places, so it adds none.
                Some(DeclarationId::from(name.as_str()))
            })
            .collect();
        locator::all_places(
            &self.graph,
            &self.synthesized,
            self.layout(),
            written,
            Some(cursor),
        )
    }

    /// Where a *template's* instance variable is written, which is never the cursor's file.
    ///
    /// - **The second half of the scope walk above, not a second mechanism.** A template has no
    ///   enclosing class, so `@story`'s writes are in another document, and
    ///   [`locator::variable_at`] searches only the cursor's buffer. Without this, `definition`
    ///   answers almost no template reads that the card answers.
    /// - **The same rung as the card, so they cannot disagree.** `types::renderer_writes` and the
    ///   card's reader share one function for the class and its documents (the controller a
    ///   template's directory names, or the mailer when there is none). Where the path's class
    ///   writes nothing, or it names none (a partial), the jump falls to every renderer's writes,
    ///   the card's fold (`types::instance_writes`), and lists them rather than picking
    ///   one. The tier stays *Derived*: a jump built on a path convention is not promoted for
    ///   having a `Location`.
    /// - **Nothing is rebased**, the walk's rule with two texts: the origin span is measured in the
    ///   template's buffer and each target in the renderer's, each in the text it came from.
    /// - **The renderer is read twice** (writes, then position encoding), both through `with_text`,
    ///   so both see the same string. One file, on a path ordinary Ruby never takes.
    fn template_variable_links(
        &self,
        uri: &DocUri,
        origin: lsp_types::Range,
        name: &str,
        sources: &types::Sources<'_>,
    ) -> Option<Vec<LocationLink>> {
        // A path test first. This runs for every `definition` at an instance variable the file
        // never writes (including a subclass reading a superclass's), and an ordinary Ruby file
        // should pay only a look at its own name for a rung it can never reach.
        if !uri.to_file_path().is_some_and(|path| views::is_view(&path)) {
            return None;
        }
        let uri_id = UriId::from(uri.as_str());
        self.links_to_writes(origin, types::renderer_writes(sources, uri_id, name))
    }

    /// One link per write, by document, each landing on the variable's name: what was navigated to
    /// is where the variable is written, not the expression. `None` for no link at all.
    fn links_to_writes(
        &self,
        origin: lsp_types::Range,
        writes: Vec<(String, Vec<(u32, u32)>)>,
    ) -> Option<Vec<LocationLink>> {
        let links: Vec<LocationLink> = writes
            .into_iter()
            .filter_map(|(target, writes)| {
                let target = DocUri::from_graph_uri(&target)?;
                let target_uri = target.to_lsp().ok()?;
                self.with_text(&target, |text| {
                    writes
                        .iter()
                        .map(|&(start, end)| LocationLink {
                            origin_selection_range: Some(origin),
                            target_uri: target_uri.clone(),
                            // The name alone, as the in-file walk answers: what was navigated to is
                            // where the variable is written, not the expression.
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

    /// `textDocument/implementation`: the definition, and every override below the receiver.
    ///
    /// - **The graph half of [`Analysis::goto_definition`], not its scope walk.** An instance
    ///   variable and a `require` path have no implementations, and a macro's `:symbol` names a
    ///   method whose overrides this could list, but all three are the parts of `definition` about
    ///   something other than a declaration. What remains is exactly what a hierarchy can be taken
    ///   of.
    /// - **A guessed receiver answers `null`.** The rung below the graph matches on a name alone,
    ///   so its answer is declarations that happen to be spelled alike, and every override below
    ///   *those* multiplies the guess. A jump has no room for a footnote, so, as with `hints`'
    ///   margin, the bottom tier is not drawn where it cannot be shown. `hover` at the same cursor
    ///   still answers, and says it is guessing.
    fn goto_implementation(&self, params: serde_json::Value) -> Option<GotoDefinitionResponse> {
        let params: lsp_types::request::GotoImplementationParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let read = |uri: &str| self.read_of(uri);
        let memo = types::Memo::new(&read, &self.exits);
        let sources = self.sources(&read, &memo);

        let (origin, sites) = self.with_text(&uri, |text| {
            let parsed = cursor::Parsed::new(text.text());
            let offset = text.offset_at(position);
            let rebase = self.rebase_for(&uri, text.text());
            let at = rebase.to_graph(offset)?;

            let uri_id = UriId::from(uri.as_str());
            for located in
                locator::locate_written(&self.graph, uri_id, at, &parsed, offset, &rebase)
            {
                let found = rebase.span_to_buffer(ByteSpan {
                    start: located.start,
                    end: located.end,
                })?;
                let (start, end) = (found.start, found.end);
                // The same rung `definition` and `hover` read, so this list's first row and
                // `definition`'s only row agree on what the call reaches.
                let resolved =
                    locator::resolve_typed(&sources, uri_id, &parsed, &located, start, &rebase)?;
                if !resolved.precise || !jumpable(resolved.derivation.tier()) {
                    continue;
                }
                let sites = hierarchy::implementations(
                    &self.graph,
                    &self.synthesized,
                    self.layout(),
                    &resolved,
                    Some(uri.as_str()),
                    MAX_SUBTYPES,
                    self.placed(),
                );
                if !sites.is_empty() {
                    return Some((text.range_at(start, end), sites));
                }
            }
            None
        })??;

        self.goto_response(
            sites
                .into_iter()
                .filter_map(|site| self.link(origin, &site))
                .collect(),
            // **Its own capability, not `definition`'s.** The protocol gives each goto its own
            // `linkSupport`, and Claude Code (the one client that asks for this) declares
            // `definition.linkSupport: true` and nothing for `implementation`.
            self.client.implementation_links,
        )
    }

    /// `textDocument/typeDefinition`: the class of the value under the cursor.
    ///
    /// - **A different question from `definition` at the same byte, on purpose.** At `story.author`
    ///   the jump goes to `def author`; this goes to `class Author`. At a local, `definition`
    ///   answers nothing (the line is visible, per `navigation.md`), and this answers the class it
    ///   holds, the one thing about a local *not* on the line.
    /// - **Four kinds of cursor, one classification each**, [`locator::type_of`]'s: an instance
    ///   variable via the scope walk, a binding via the inlay margin's call, a read or call via the
    ///   walk beside it, a constant via the class-object arm.
    /// - **A guessed type answers `null`**, as for `implementation`: the bottom tier is a name
    ///   match and a jump has no room for the footnote. `hover` still answers and says it is
    ///   guessing. The gate tests the **tier**, never shapes, so a new rung below the graph is
    ///   refused by default.
    /// - **`null` at a local is expected, not a defect.** Most locals are assigned a bare parameter
    ///   with no declared type, so the number to watch is how many cursors answer, not how many
    ///   answer wrongly.
    fn goto_type_definition(&self, params: serde_json::Value) -> Option<GotoDefinitionResponse> {
        let params: lsp_types::request::GotoTypeDefinitionParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let read = |uri: &str| self.read_of(uri);
        let memo = types::Memo::new(&read, &self.exits);
        let sources = self.sources(&read, &memo);

        let (origin, sites) = self.with_text(&uri, |text| {
            let parsed = cursor::Parsed::new(text.text());
            let offset = text.offset_at(position);
            let rebase = self.rebase_for(&uri, text.text());
            let at = rebase.to_graph(offset)?;

            let uri_id = UriId::from(uri.as_str());
            let ((start, end), resolution) =
                locator::type_of(&sources, uri_id, &parsed, offset, at, &rebase)?;
            if !jumpable(resolution.derivation.tier()) {
                return None;
            }
            let sites = locator::all_places(
                &self.graph,
                &self.synthesized,
                self.layout(),
                resolution.declarations,
                Some(uri.as_str()),
            );
            (!sites.is_empty()).then(|| (text.range_at(start, end), sites))
        })??;

        self.goto_response(
            sites
                .into_iter()
                .filter_map(|site| self.link(origin, &site))
                .collect(),
            // Its own `linkSupport`, not `definition`'s: the protocol gives each of the four its
            // own, and clients differ.
            self.client.type_definition_links,
        )
    }

    /// `textDocument/declaration`: the signature, or `null`.
    ///
    /// - **The answer is a file the reader would never have opened.** `String#split` is written in
    ///   C, which ya-lsp does not index; what it has is `stdlib/…/string.rbs`, stating what the
    ///   method takes and returns. That is a *declaration*, not a definition, which is why
    ///   [`locator::places`] drops it where source survives, and why this method asks for exactly
    ///   that half.
    /// - **Never the `.rb`; an empty list is the answer, not a failure.** Falling back to source
    ///   would make this a second `definition`, which every client already binds. `null` returns
    ///   the editor's own behaviour, which for the clients that send this is to fall through to
    ///   `definition`.
    /// - **The graph half of [`Analysis::goto_definition`], not its scope walk**, as
    ///   [`Analysis::goto_implementation`] does. An instance variable has no signature, a `require`
    ///   path is a file, and a macro's `:symbol` names a generator's method, which answers `null`
    ///   by the next point.
    /// - **A generated declaration answers `null`, by design.** `synthesize`'s output lives under a
    ///   non-`file:` URI that [`locator::site`] maps back to the macro line, an `.rb`: `story.user`
    ///   answers `definition` at `belongs_to :user` and nothing here, since no written signature
    ///   says what `user` is. A Sorbet `sig` or YARD `@return` behaves the same: the declaration is
    ///   the `def` itself, and `annotations.rs` contributes a *type*, not a second place.
    /// - **A guessed receiver answers `null`**, through the same [`jumpable`] gate. Here it matters
    ///   most: a vendored signature is the most official-looking document in the index, and
    ///   pointing a name match at one would pass a guess off as the standard library's word.
    fn goto_declaration(&self, params: serde_json::Value) -> Option<GotoDefinitionResponse> {
        let params: lsp_types::request::GotoDeclarationParams = parse_params(params)?;
        let position = params.text_document_position_params.position;
        let uri = DocUri::from_lsp(&params.text_document_position_params.text_document.uri)?;

        let read = |uri: &str| self.read_of(uri);
        let memo = types::Memo::new(&read, &self.exits);
        let sources = self.sources(&read, &memo);

        let (origin, sites) = self.with_text(&uri, |text| {
            let parsed = cursor::Parsed::new(text.text());
            let offset = text.offset_at(position);
            let rebase = self.rebase_for(&uri, text.text());
            let at = rebase.to_graph(offset)?;

            let uri_id = UriId::from(uri.as_str());
            for located in
                locator::locate_written(&self.graph, uri_id, at, &parsed, offset, &rebase)
            {
                let found = rebase.span_to_buffer(ByteSpan {
                    start: located.start,
                    end: located.end,
                })?;
                let (start, end) = (found.start, found.end);
                // The same rung `definition` reads, so the signature here belongs to the `def` the
                // other jump lands in.
                let resolved =
                    locator::resolve_typed(&sources, uri_id, &parsed, &located, start, &rebase)?;
                if !resolved.precise || !jumpable(resolved.derivation.tier()) {
                    continue;
                }
                let sites = locator::all_signatures(
                    &self.graph,
                    &self.synthesized,
                    self.layout(),
                    resolved.declarations,
                    Some(uri.as_str()),
                );
                if !sites.is_empty() {
                    return Some((text.range_at(start, end), sites));
                }
            }
            None
        })??;

        self.goto_response(
            sites
                .into_iter()
                .filter_map(|site| self.link(origin, &site))
                .collect(),
            // The fourth goto, read from its own capability like the other three.
            self.client.declaration_links,
        )
    }

    /// The two shapes `definition` answers in, decided in one place.
    ///
    /// Two paths reach it (the scope walk and the graph), and which shape the client asked for is
    /// the client's property, not either path's.
    fn definition_response(&self, links: Vec<LocationLink>) -> Option<GotoDefinitionResponse> {
        self.goto_response(links, self.client.definition_links)
    }

    /// A goto's answer in the shape the client negotiated **for that goto**.
    ///
    /// The flag is a parameter, not a field read here, because the protocol has one per method and
    /// they differ in practice (see [`ClientSupport`](super::ClientSupport)). The conversion is
    /// shared; the deciding flag is not.
    fn goto_response(
        &self,
        links: Vec<LocationLink>,
        wants_links: bool,
    ) -> Option<GotoDefinitionResponse> {
        if links.is_empty() {
            return None;
        }
        Some(if wants_links {
            GotoDefinitionResponse::Link(links)
        } else {
            GotoDefinitionResponse::Array(
                links
                    .into_iter()
                    .map(|link| Location {
                        uri: link.target_uri,
                        // The name, not the whole body: where the editor parks the cursor, and
                        // landing on `class` is landing in the right place.
                        range: link.target_selection_range,
                    })
                    .collect(),
            )
        })
    }

    /// `textDocument/documentLink`.
    ///
    /// The `require` half of `definition`, for the whole file at once, with no cursor needed.
    /// Nothing else in a Ruby file is a link: a constant is navigation, not a resource, and the
    /// editor already ctrl-clicks it.
    ///
    /// **A `require` that resolves nowhere produces no link.** The protocol allows a link with no
    /// `target`, resolved later, but that is wrong here twice: there is nothing to resolve later,
    /// and an underlined path that goes nowhere is worse than none. `require "json"` with no
    /// indexed stdlib is the common case.
    fn document_links(&self, params: serde_json::Value) -> Option<Vec<DocumentLink>> {
        let params: lsp_types::DocumentLinkParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;

        // The buffer is parsed and measured, so no rebase: both halves of every link come from the
        // same text, and the graph is asked only about a path string, which no edit can move.
        let links = self.with_text(&uri, |text| {
            requires::all(text.text())
                .into_iter()
                .filter_map(|require| {
                    let site = self.require_site(&uri, &require)?;
                    Some(DocumentLink {
                        range: text.range_at(require.start, require.end),
                        target: Some(DocUri::from_graph_uri(&site.uri)?.to_lsp().ok()?),
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
    /// Only the user's own code; see `analysis::references` for why, and for how constant and
    /// method precision differ.
    fn references(&self, params: serde_json::Value) -> Option<Vec<Location>> {
        let params: lsp_types::ReferenceParams = parse_params(params)?;
        let position = params.text_document_position.position;
        let uri = DocUri::from_lsp(&params.text_document_position.text_document.uri)?;
        let include_declaration = params.context.include_declaration;
        let scope = self.scope_rooted_in(Some(&uri));

        let read = |uri: &str| self.read_of(uri);
        let memo = types::Memo::new(&read, &self.exits);
        let sources = self.sources(&read, &memo);
        let handed = |names: &[StringId], scope: &HashSet<UriId>| self.named_uses(names, scope);
        let mut found = self.with_text(&uri, |text| {
            let parsed = cursor::Parsed::new(text.text());
            let offset = text.offset_at(position);
            let uri_id = UriId::from(uri.as_str());
            // As in goto-definition: several targets can share the narrowest span, so take the
            // first with something to say.
            let found = locator::locate(&self.graph, uri_id, offset)
                .into_iter()
                .find_map(|located| {
                    let resolution = locator::resolve(
                        &self.graph,
                        &located,
                        // Never the tree fence (a use under `spec/` is a use), always the other,
                        // because a document outside the project is not a file this work list may
                        // offer to edit. `environment`'s table has the argument.
                        environment::Fence::uses(
                            locator::uri_of(&self.graph, UriId::from(uri.as_str())),
                            self.layout(),
                        ),
                    );
                    let found = references::find(
                        &self.graph,
                        &self.synthesized,
                        &located,
                        &resolution,
                        &scope,
                        include_declaration,
                        &handed,
                    );
                    (!found.is_empty()).then_some(found)
                });
            // **A symbol argument, which the graph holds no target for**, as `definition` asks:
            // `send(:shout)`'s `:shout` is a use of `shout`, and so are the rest of its uses.
            found.or_else(|| {
                let rebase = self.rebase_for(&uri, text.text());
                let at = rebase.to_graph(offset)?;
                let (symbol, resolution) =
                    locator::symbol_at(&sources, uri_id, &parsed, offset, at, &rebase)?;
                let found = references::to_member(
                    &self.graph,
                    &self.synthesized,
                    &symbol.name,
                    &resolution.declarations,
                    &scope,
                    &handed,
                );
                (!found.is_empty()).then_some(found)
            })
        })??;

        if found.len() > MAX_REFERENCES {
            // **Shown to the user, not just logged.** A truncated "find all references" is a wrong
            // answer shaped like a right one, and only the user can decide what to do about it.
            //
            // - **The message names the file the list stops in.** `references::ordered` sorts by
            //   URI before this, so the cut is *contiguous*: everything up to one point is present
            //   and nothing after it. Without the file name, a reader finding no use under some
            //   directory cannot tell the search stopped before reaching it.
            // - **It is reached in practice**, on common names in large workspaces, so the sentence
            //   has to be worth reading.
            let total = found.len();
            found.truncate(MAX_REFERENCES);
            let stops_at = found
                .last()
                .and_then(|reference| DocUri::from_graph_uri(&reference.uri))
                .and_then(|uri| uri.to_file_path());
            let message =
                messages::references_truncated(total, MAX_REFERENCES, stops_at.as_deref());
            tracing::warn!("{message}");
            self.show_warning(&message);
        }

        let mut ranges = Ranges::new(self);
        let locations: Vec<Location> = found
            .into_iter()
            .filter_map(|reference| {
                let uri = DocUri::from_graph_uri(&reference.uri)?;
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
            self.placed(),
            self.layout(),
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
                let uri = DocUri::from_graph_uri(&hit.site.uri)?;
                // The name span, not the whole construct: a client reveals `location.range`
                // selected, and selecting a 400-line class body to show where it starts is not what
                // anyone asked for.
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

        // `null` rather than `[]` for nothing found, like every handler: an empty array claims the
        // project has no such symbol, which is only true by accident.
        (!symbols.is_empty()).then_some(WorkspaceSymbolResponse::Flat(symbols))
    }

    // -----------------------------------------------------------------------
    // Type hierarchy
    // -----------------------------------------------------------------------

    /// `textDocument/prepareTypeHierarchy`.
    ///
    /// The returned item is what both follow-ups arrive holding, so it carries the declaration in
    /// `data` as a decimal string (for `completionItem/resolve`'s reason: a `DeclarationId` is a
    /// 64-bit hash and JSON numbers are doubles). Unlike a completion item it survives a config
    /// reload, because the hash is of the *name*: the id still finds the class after the graph is
    /// rebuilt.
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
                self.placed(),
                self.layout(),
            )
        })?;
        self.hierarchy_items(items)
    }

    /// `typeHierarchy/supertypes`.
    fn supertypes(&self, params: serde_json::Value) -> Option<Vec<TypeHierarchyItem>> {
        let params: lsp_types::TypeHierarchySupertypesParams = parse_params(params)?;
        let declaration = declaration_in(params.item.data.as_ref())?;
        // The row the client sent back, where this request is rooted: an expansion carries no
        // cursor, so the reader's document is the item's. See `hierarchy::rooted_in`.
        let rooted = DocUri::from_lsp(&params.item.uri);
        let items = hierarchy::supertypes(
            &self.graph,
            &self.synthesized,
            declaration,
            self.placed(),
            self.layout(),
            rooted.as_ref().map(DocUri::as_str),
        );
        self.hierarchy_items(items)
    }

    /// `typeHierarchy/subtypes`.
    fn subtypes(&self, params: serde_json::Value) -> Option<Vec<TypeHierarchyItem>> {
        let params: lsp_types::TypeHierarchySubtypesParams = parse_params(params)?;
        let declaration = declaration_in(params.item.data.as_ref())?;
        let rooted = DocUri::from_lsp(&params.item.uri);
        let found = hierarchy::subtypes(
            &self.graph,
            &self.synthesized,
            declaration,
            MAX_SUBTYPES,
            self.placed(),
            self.layout(),
            rooted.as_ref().map(DocUri::as_str),
        );

        if found.found > MAX_SUBTYPES {
            // Shown to the user, as a truncated `references` is: a short subtype list looks
            // complete, and only the user can decide what to do. Reaching it takes asking about
            // something near the object model's root, a deliberate click, so the message is
            // information, not noise.
            let message = messages::subtypes_truncated(found.found, MAX_SUBTYPES);
            tracing::warn!("{message}");
            self.show_warning(&message);
        }
        self.hierarchy_items(found.items)
    }

    /// Turn hierarchy rows into the wire shape, reading each file at most once.
    ///
    /// A row whose file cannot be read is dropped, not sent with a made-up range (rubydex's
    /// synthetic built-in document is the one that reaches here, and `DocUri` rejects it
    /// everywhere). `null` for an empty result, never `[]`: an empty array claims a class has no
    /// ancestors, which is untrue of anything in Ruby.
    fn hierarchy_items(&self, items: Vec<hierarchy::Item>) -> Option<Vec<TypeHierarchyItem>> {
        let mut ranges = Ranges::new(self);
        let items: Vec<TypeHierarchyItem> = items
            .into_iter()
            .filter_map(|item| {
                let uri = DocUri::from_graph_uri(&item.site.uri)?;
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
    /// The same `data` as the type hierarchy, for the same reason: follow-ups arrive with only this
    /// item, and JSON doubles would round a 64-bit `DeclarationId`. `outgoingCalls` also reads the
    /// item's *position*; see `hierarchy::outgoing` for why that direction cannot use the
    /// declaration.
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
                self.placed(),
                self.layout(),
            )
        })?;
        self.call_items(items)
    }

    /// `callHierarchy/incomingCalls`.
    ///
    /// Every row is a name match, because rubydex links no method reference to a declaration
    /// (`references` makes the same trade). The row says so: every caller's detail column ends in
    /// "by name", because a tree implies an exactness this answer lacks.
    fn incoming_calls(&self, params: serde_json::Value) -> Option<Vec<CallHierarchyIncomingCall>> {
        let params: lsp_types::CallHierarchyIncomingCallsParams = parse_params(params)?;
        let declaration = declaration_in(params.item.data.as_ref())?;
        let rooted = DocUri::from_lsp(&params.item.uri);
        let found = hierarchy::incoming(
            &self.graph,
            &self.synthesized,
            declaration,
            // **No fence of either kind, because the scope makes it right.** This list answers
            // *where is this used*, over the project's documents plus the one the tree is rooted
            // in: a call in a file outside the project is not in `scope` unless that file is where
            // the reader is asking from.
            MAX_INCOMING_CALLS,
            &self.scope_rooted_in(rooted.as_ref()),
        );

        if found.found > MAX_INCOMING_CALLS {
            // Shown to the user, like a truncated `references` or subtype list: a short caller list
            // looks complete, and a call hierarchy is read as complete.
            let message = messages::incoming_calls_truncated(found.found, MAX_INCOMING_CALLS);
            tracing::warn!("{message}");
            self.show_warning(&message);
        }

        let mut ranges = Ranges::new(self);
        let calls: Vec<CallHierarchyIncomingCall> = found
            .calls
            .into_iter()
            .filter_map(|call| {
                // The ranges are in the caller's own file, this row's file.
                let uri = DocUri::from_graph_uri(&call.item.site.uri)?;
                let from_ranges: Vec<lsp_types::Range> = call
                    .ranges
                    .iter()
                    .filter_map(|(start, end)| ranges.at(&uri, *start, *end))
                    .collect();
                // A row whose calls could not be placed is dropped, not sent empty: a caller with
                // nothing to jump to is worse than none.
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
    /// Addressed by the item's position, not its declaration: the body being expanded is the one
    /// asked about, and a reopened class has more than one.
    fn outgoing_calls(&self, params: serde_json::Value) -> Option<Vec<CallHierarchyOutgoingCall>> {
        let params: lsp_types::CallHierarchyOutgoingCallsParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.item.uri)?;
        let position = params.item.selection_range.start;

        let read = |uri: &str| self.read_of(uri);
        let blocks = locator::Blocks::new(&read);
        let calls = self.with_text(&uri, |text| {
            hierarchy::outgoing(
                &self.graph,
                &self.synthesized,
                UriId::from(uri.as_str()),
                text.offset_at(position),
                self.placed(),
                self.layout(),
                &blocks,
            )
        })?;

        let mut ranges = Ranges::new(self);
        let calls: Vec<CallHierarchyOutgoingCall> = calls
            .into_iter()
            .filter_map(|call| {
                // Unlike incoming calls, the ranges are in the file being *expanded*, not the row's
                // file: an outgoing call is written where the caller is.
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
    /// `None` where the file cannot be read or the URI is not one an editor can open, the drop
    /// `hierarchy_items` makes. Two functions, not one generic, because `TypeHierarchyItem` and
    /// `CallHierarchyItem` are unrelated structs with the same fields.
    fn call_item(&self, ranges: &mut Ranges, item: hierarchy::Item) -> Option<CallHierarchyItem> {
        let uri = DocUri::from_graph_uri(&item.site.uri)?;
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
    /// The one request that shows an answer nobody asked for, so the tier decides what may be
    /// drawn, not just how it is labelled (see [`hints`]). The range is the editor's visible
    /// window, passed straight down: it bounds the work, not the answer.
    fn inlay_hints(&self, params: serde_json::Value) -> Option<Vec<InlayHint>> {
        let params: lsp_types::InlayHintParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;

        let read = |uri: &str| self.read_of(uri);
        let memo = types::Memo::new(&read, &self.exits);
        let sources = self.sources(&read, &memo);
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
            .map(|hint| Self::inlay_hint(text, &hint))
            .collect::<Vec<_>>()
        })?;

        (!found.is_empty()).then_some(found)
    }

    /// One hint in the wire shape: the label and nothing else. There is no tooltip to resolve,
    /// so no `data`.
    ///
    /// **`TYPE` for all three families.** The protocol's `PARAMETER` means argument labels at a
    /// *call site*, a different feature.
    fn inlay_hint(text: &TextDocument, hint: &hints::Hint) -> InlayHint {
        InlayHint {
            position: text.position_at(hint.at),
            label: InlayHintLabel::String(hint.label.clone()),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: None,
            data: None,
        }
    }

    // -----------------------------------------------------------------------
    // Code actions, and the one document the server serves itself
    // -----------------------------------------------------------------------

    /// `textDocument/codeAction`.
    ///
    /// **Two families with two gates**, so the refusals below are not one `return None`.
    ///
    /// - **The four refactorings write a line of Ruby**, so they are declined outside the user's
    ///   own code (silently, since the user did not ask; an absent menu entry says enough) and
    ///   inside a template, where a line belongs to the markup: `erb::ruby_view` keeps offsets, but
    ///   cannot help a line starting with `<td>`.
    /// - **The fifth action writes nothing**: it opens a read-only document, so neither reason
    ///   applies. A cursor in an ERB view on a column a migration declared is exactly the reader it
    ///   is for.
    fn code_actions(&self, params: serde_json::Value) -> Option<Vec<CodeActionOrCommand>> {
        let params: lsp_types::CodeActionParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;
        let wanted = params.context.only.as_deref().unwrap_or_default();
        let mut actions = self.generated_actions(&uri, params.range.start, wanted);

        if self.is_own_code(uri.as_str())
            && !erb::is_template_uri(&uri)
            && let Some(refactorings) = self.with_text(&uri, |text| {
                let start = text.offset_at(params.range.start);
                let end = text.offset_at(params.range.end);
                code_actions::at(text.text(), start, end)
                    .into_iter()
                    .map(|action| {
                        let kind = match action.kind {
                            code_actions::Kind::Extract => CodeActionKind::REFACTOR_EXTRACT,
                            code_actions::Kind::Rewrite => CodeActionKind::REFACTOR_REWRITE,
                        };
                        // Converted here, once, from the same read that produced the offsets; why
                        // `rename::Replacement` carries both.
                        let edits = action
                            .edits
                            .iter()
                            .map(|edit| TextEdit {
                                range: text.range_at(edit.start, edit.end),
                                new_text: edit.text.clone(),
                            })
                            .collect();
                        (
                            kind.clone(),
                            CodeActionOrCommand::CodeAction(CodeAction {
                                title: action.title,
                                kind: Some(kind),
                                edit: Some(self.workspace_edit(vec![(uri.clone(), edits)])),
                                ..CodeAction::default()
                            }),
                        )
                    })
                    .filter(|(kind, _)| asked_for(wanted, kind))
                    .map(|(_, action)| action)
                    .collect::<Vec<_>>()
            })
        {
            actions.extend(refactorings);
        }

        (!actions.is_empty()).then_some(actions)
    }

    /// The fifth action: open the RBS ya-lsp wrote for the member under the cursor.
    ///
    /// - **Offered where the cursor is on a declaration no file declares**: a column, an
    ///   association reader, an `enum` predicate, a route helper, a `Struct.new` member, a method a
    ///   `sig` block typed. Each has a place (`locator::site` maps it to the line that implied it),
    ///   but *none* has a document a reader can open: its text is indexed under a scheme with no
    ///   file. This is the only way in.
    /// - **Both capabilities are required.** `window/showDocument` shows the document and
    ///   `workspace/textDocumentContent` fills it, and a client can have the first without the
    ///   second (Neovim does: the jump opens a buffer named after a URI, `bufload` finds no file,
    ///   and the reader gets an empty window). So the action is not offered where the text cannot
    ///   follow.
    /// - **The position is the range's start; the selection is ignored**, unlike the refactorings,
    ///   which act on a dragged span. This asks a question about a name.
    fn generated_actions(
        &self,
        uri: &DocUri,
        position: lsp_types::Position,
        wanted: &[CodeActionKind],
    ) -> Vec<CodeActionOrCommand> {
        if !self.client.show_document
            || !self.client.generated_content
            || !asked_for(wanted, &CodeActionKind::EMPTY)
        {
            return Vec::new();
        }
        let read = |uri: &str| self.read_of(uri);
        let memo = types::Memo::new(&read, &self.exits);
        let sources = self.sources(&read, &memo);
        let found = self.with_text(uri, |text| {
            let parsed = cursor::Parsed::new(text.text());
            let offset = text.offset_at(position);
            let rebase = self.rebase_for(uri, text.text());
            let at = rebase.to_graph(offset)?;
            let uri_id = UriId::from(uri.as_str());
            for located in locator::locate(&self.graph, uri_id, at) {
                let found = rebase.span_to_buffer(ByteSpan {
                    start: located.start,
                    end: located.end,
                })?;
                let resolved = locator::resolve_typed(
                    &sources,
                    uri_id,
                    &parsed,
                    &located,
                    found.start,
                    &rebase,
                )?;
                // The gotos' gate, for its reason: a jump has nowhere to print *matched on the name
                // alone*, and this one lands in a document headed `class Story` that ya-lsp wrote,
                // the most authoritative-looking page in the index, reached by a guess.
                if !resolved.precise || !jumpable(resolved.derivation.tier()) {
                    continue;
                }
                let documents = self.generated_documents(&resolved.declarations);
                if !documents.is_empty() {
                    return Some(documents);
                }
            }
            // **A macro's `:symbol`, after the graph**, as `definition` and `hover` ask. It is the
            // cursor this feature is for: standing on `belongs_to :user`'s `:user` and asking what
            // the line declared. The walk above never reaches it, because a symbol is not a call.
            //
            // **No tier gate, and not as an exception.** `resolve_symbol` finds the member by exact
            // name on the nesting or caller and answers `Resolution::derived`: `precise` by
            // construction, with `named_by` and no `guess`, so `Derivation::tier` cannot be
            // `Guessed` here. The gate refuses names *matched* across the graph, and this rung
            // never matches.
            let (_, resolution) =
                locator::symbol_at(&sources, uri_id, &parsed, offset, at, &rebase)?;
            let documents = self.generated_documents(&resolution.declarations);
            (!documents.is_empty()).then_some(documents)
        });
        found
            .flatten()
            .unwrap_or_default()
            .into_iter()
            .map(|generated| {
                CodeActionOrCommand::CodeAction(CodeAction {
                    title: show_generated_title(&generated),
                    // No kind: the protocol's `CodeActionKind::Empty`, which `server::capabilities`
                    // advertises as a third entry. This is neither a refactoring nor a fix (it
                    // edits nothing), and filing it under one for a menu entry would be a lie a
                    // client acts on.
                    kind: None,
                    command: Some(lsp_types::Command {
                        title: show_generated_title(&generated),
                        command: self.show_generated_command(),
                        arguments: Some(vec![serde_json::Value::String(generated)]),
                    }),
                    ..CodeAction::default()
                })
            })
            .collect()
    }

    /// Every generated document that wrote one of these declarations, deduplicated, in order.
    ///
    /// A declaration merges every definition of a name, so one cursor can reach two documents:
    /// `Story#title` from the schema *and* a `sig` block. Both are offered, because picking would
    /// mean ranking two statements of the same fact, and there is no rank.
    fn generated_documents(&self, declarations: &[DeclarationId]) -> Vec<String> {
        let mut documents: Vec<String> = Vec::new();
        for declaration in declarations {
            for definition in locator::definitions_of(&self.graph, *declaration) {
                if !self.synthesized.is_generated(definition.uri_id()) {
                    continue;
                }
                // **No arm for a document the graph lacks, because there is no such definition.**
                // `Synthesized::record` indexes the text and files the table together, and
                // `Synthesized::drop` removes both, so any definition the table calls generated is
                // in a document the graph holds. A `continue` would be an unreachable arm.
                let uri = self
                    .graph
                    .documents()
                    .get(definition.uri_id())
                    .map_or("", rubydex::model::document::Document::uri);
                if !documents.iter().any(|held| held == uri) {
                    documents.push(uri.to_owned());
                }
            }
        }
        documents
    }

    /// `workspace/textDocumentContent`: the text of a document the server itself wrote.
    ///
    /// - **The one request whose argument is a URI the client was *given***, not one read from an
    ///   open file, so the reader is deliberately narrow: `Synthesized::content` refuses every
    ///   scheme but its own, so this can never be made to read a file from disk.
    /// - **Answering records that someone is reading it**, the server's only way to learn that (a
    ///   fileless document has no `didOpen`). `refresh_generated` reads it back, so the refresh
    ///   after a migration reaches the window it concerns and nothing else.
    /// - **The one handler that answers an error, because of [`reply`]'s rule.** "An absent answer
    ///   is `null`" is about *optional* results, and this one is not: `TextDocumentContentResult`
    ///   is a bare `{ text }` with no null arm. It also makes multi-root work: the editor registers
    ///   one content provider per server and takes the first that answers, so a server that did not
    ///   write the document must say *not mine* distinguishably.
    fn text_document_content(&mut self, id: &RequestId, params: serde_json::Value) -> Response {
        let asked = params
            .get("uri")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some((spelling, text)) = self.synthesized.content(&asked) else {
            tracing::debug!("no generated document is filed under {asked}");
            return Response::new_err(
                id.clone(),
                ErrorCode::InvalidParams as i32,
                format!("ya-lsp generated no document under {asked}"),
            );
        };
        let (spelling, text) = (spelling.to_owned(), text.to_owned());
        tracing::debug!("serving {} bytes of generated {spelling}", text.len());
        self.serving(&spelling, &asked);
        Response::new_ok(id.clone(), serde_json::json!({ "text": text }))
    }

    /// `workspace/executeCommand`: the one command, which only opens a document.
    ///
    /// The result is `null` either way. The protocol has no shape for *the command ran*, and the
    /// command's work is a separate request travelling the other way, so a client waiting on this
    /// response would wait for the wrong message.
    fn execute_command(&self, params: serde_json::Value) -> Option<serde_json::Value> {
        let params: lsp_types::ExecuteCommandParams = parse_params(params)?;
        if params.command != self.show_generated_command() {
            tracing::warn!("ya-lsp registers no command called {}", params.command);
            return None;
        }
        let uri = params
            .arguments
            .first()
            .and_then(serde_json::Value::as_str)?;
        // Checked against the table, not trusted. The argument comes from whatever ran the command
        // (a lightbulb, a keybinding, another extension), and making an editor open a URI someone
        // else chose is the one way this command could be abused.
        if self.synthesized.content(uri).is_none() {
            tracing::warn!("no generated document is filed under {uri}");
            return None;
        }
        self.show_generated(uri);
        None
    }

    // -----------------------------------------------------------------------
    // Rename
    // -----------------------------------------------------------------------

    /// `textDocument/prepareRename`.
    ///
    /// Answering with a range *promises* the rename will succeed, so this runs the whole plan and
    /// reads every span back before saying yes. That costs one read per file containing the name,
    /// once, on a deliberate keypress, and is the only way the promise is honest: a yes followed by
    /// a refused rename would refuse after the user typed the new name.
    fn prepare_rename(&self, params: serde_json::Value) -> Option<PrepareRenameResponse> {
        let params: lsp_types::TextDocumentPositionParams = parse_params(params)?;
        let uri = DocUri::from_lsp(&params.text_document.uri)?;
        let offset = self.with_text(&uri, |text| text.offset_at(params.position))?;
        let renaming = self.renaming(&uri, offset)?;

        // The one replacement containing the cursor: the editor puts its rename box over exactly
        // this range, pre-filled with its text. It is not always the span the cursor was *located*
        // in: `Failure = Class.new(StandardError)`'s name is located as the whole assignment and
        // narrowed to the word, so a cursor on its `=` finds nothing and answers `null`.
        let here = renaming
            .files
            .iter()
            .filter(|(at, _)| *at == uri)
            .flat_map(|(_, replacements)| replacements)
            // A range, not a pair of comparisons: the same test without the short-circuit arm a
            // `&&` would add in a file measured for coverage.
            .find(|at| (at.start..=at.end).contains(&offset))?;
        Some(PrepareRenameResponse::Range(here.range))
    }

    /// `textDocument/rename`.
    fn rename(&self, params: serde_json::Value) -> Option<WorkspaceEdit> {
        let params: lsp_types::RenameParams = parse_params(params)?;
        let position = params.text_document_position;
        let uri = DocUri::from_lsp(&position.text_document.uri)?;
        let offset = self.with_text(&uri, |text| text.offset_at(position.position))?;
        // The plan is rebuilt, not remembered from the prepare: `prepareSupport` is a client
        // capability, and a client without it sends this request alone, so every refusal must be
        // reachable from here too.
        let renaming = self.renaming(&uri, offset)?;

        if !rename::is_name(&params.new_name, renaming.constant) {
            let message = messages::rename_needs_a_ruby_name(&params.new_name, renaming.constant);
            tracing::info!("{message}");
            self.show_warning(&message);
            return None;
        }

        // Nothing left to decide: every range was converted when the plan was confirmed, from the
        // read that checked its bytes, so there is no second conversion to disagree or fail.
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
        // Counted into locals first: an argument on its own line inside `tracing::debug!` is
        // evaluated only at that level, so it would read as a line no test ran.
        let edited: usize = files.iter().map(|(_, edits)| edits.len()).sum();
        let (touched, from, to) = (files.len(), &renaming.name, &params.new_name);
        tracing::debug!("rename {from:?} -> {to:?}: {edited} edits across {touched} files");
        Some(self.workspace_edit(files))
    }

    /// The rename at a position, with every span read back and checked against the name it
    /// replaces.
    ///
    /// Both requests go through here, and a refusal is shown, not just answered with `null`: the
    /// user pressed a key for this, and the editor's own "cannot be renamed" does not say why or
    /// what to do. It is the one place ya-lsp raises a `window/showMessage` for a single request
    /// rather than workspace state; the keypress earns it.
    fn renaming(&self, uri: &DocUri, offset: u32) -> Option<Renaming> {
        // ya-lsp never proposes an edit to a file that is not the user's own. Silently, like every
        // other request inside a bundle: a gem is opened to be read, and nobody pressing rename
        // there expects it to work.
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
                own,
                self.layout(),
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

        // Every span read back from the current text and confirmed to hold only the name. This
        // stops `Error = Class.new(StandardError)` (whose name span rubydex records as the whole
        // assignment) from being replaced wholesale, and makes a refusal total: a rename that
        // changed most places a name is written would leave code that no longer runs.
        let mut ranges = Ranges::new(self);
        let mut files: HashMap<DocUri, Vec<Replacement>> = HashMap::new();
        for edit in edits {
            let at = DocUri::from_graph_uri(&edit.uri)?;
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
                // Converted here, from the read that just confirmed the bytes: offsets and range
                // are two views of one span, and deriving them separately is how they come to
                // disagree.
                range: ranges.at(&at, start, end)?,
            });
        }

        // Sorted by URI so the same rename produces the same edit every time. Spans within a file
        // already arrive in order, from `references` and the scope walk alike.
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
    /// `documentChanges` is worth negotiating, not always sending the older `changes` map, because
    /// it carries each file's version, so a client can reject a rename the user has typed past
    /// instead of applying it to moved text. The version comes from `didOpen`/`didChange` where
    /// there is a buffer, and is `null` for a file only on disk (the protocol's *optional*
    /// version).
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

    /// `workspace/willRenameFiles`: the class follows the file.
    ///
    /// - **What it does.** The editor asks before moving a file and applies the answer with the
    ///   move, so `app/models/order.rb` renamed to `purchase.rb` arrives declaring `Purchase`.
    ///   Under Zeitwerk that matters: a file keeping its old class raises on the next boot,
    ///   somewhere nothing links back to the drag.
    /// - **It is the ordinary constant rename, with every guard.** Only the source of the two names
    ///   differs ([`rename::moved`] reads the old name from the file and the new one from the path;
    ///   [`Analysis::rename`] gets both from the client), and the user typed no name, which is why
    ///   every refusal here is silent where the same refusal at a cursor is a sentence.
    /// - **Gated on `rails.enabled`**, because the rule is Zeitwerk's: plain Ruby has no relation
    ///   between a file's name and its class. A gem using Zeitwerk without Rails is the case this
    ///   gate leaves out.
    fn will_rename_files(&self, params: serde_json::Value) -> Option<WorkspaceEdit> {
        if !self.workspace.features().rails {
            return None;
        }
        let params: lsp_types::RenameFilesParams = parse_params(params)?;
        // Every file of one move, into one edit. A multi-selection dragged across a tree is one
        // request and must be one answer: a second `WorkspaceEdit` is a second undo step for one
        // user action.
        let files: Vec<(DocUri, Vec<TextEdit>)> = params
            .files
            .iter()
            .filter_map(|file| self.renamed_file(file))
            .flatten()
            .collect();
        (!files.is_empty()).then(|| self.workspace_edit(files))
    }

    /// The edits one file of a move needs, or `None` where it needs none.
    fn renamed_file(&self, file: &lsp_types::FileRename) -> Option<Vec<(DocUri, Vec<TextEdit>)>> {
        let old = DocUri::from_graph_uri(&file.old_uri)?;
        // The new path has nothing behind it yet (the point of the request), so this reads a URI as
        // a path, never opens a document.
        let new = DocUri::from_graph_uri(&file.new_uri)?;
        let (at, to) = rename::moved(
            &self.graph,
            UriId::from(old.as_str()),
            &old.to_file_path()?,
            &new.to_file_path()?,
        )?;
        let renaming = self.renaming(&old, at)?;
        Some(
            renaming
                .files
                .iter()
                .map(|(at, replacements)| {
                    let edits = replacements
                        .iter()
                        .map(|at| TextEdit {
                            range: at.range,
                            new_text: to.clone(),
                        })
                        .collect();
                    (at.clone(), edits)
                })
                .collect(),
        )
    }

    // -----------------------------------------------------------------------
    // Completion
    // -----------------------------------------------------------------------

    /// `textDocument/completion`.
    ///
    /// The list is `isIncomplete` when the cap dropped rows, the only way it can miss something a
    /// longer prefix would reach, since every filter behind it is a subsequence match. Below the
    /// cap the client narrows locally and sends nothing, saving a request per keystroke. See
    /// `analysis::completion` for the full argument and for what is exact versus guessed.
    fn completion(&self, params: serde_json::Value) -> Option<CompletionResponse> {
        let params: lsp_types::CompletionParams = parse_params(params)?;
        let position = params.text_document_position.position;
        let uri = DocUri::from_lsp(&params.text_document_position.text_document.uri)?;
        let started = Instant::now();

        // The one request that must tell a template from a Ruby file, and it must ask the *source*,
        // not the blanked view, because the view is where the markup went. A half-typed word in an
        // `<h1>` is spaces by then, so usually this changes nothing, but a caret in markup after
        // some Ruby-looking bytes would otherwise offer the workspace's constants to someone
        // writing prose.
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
        let memo = types::Memo::new(&read, &self.exits);
        let sources = self.sources(&read, &memo);
        let (completion, range) = self.with_text(&uri, |text| {
            let offset = text.offset_at(position);
            // How this buffer's offsets relate to the graph's: the identity unless indexing was
            // deferred, when it is what makes the answer trustworthy.
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
                self.placed(),
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
                // Clients sort by their own fuzzy score first and fall back to this, so the ranking
                // survives as the tiebreak. Zero-padded because it is compared as text.
                sort_text: Some(format!("{index:05}")),
                // The span of the half-typed word, so accepting `empty?` over `emp` replaces it
                // instead of appending.
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: item.label.clone(),
                })),
                // Only what `completionItem/resolve` needs to find the declaration again. A string,
                // not a number: a `DeclarationId` is a 64-bit hash and JSON numbers are doubles, so
                // a client round trip would corrupt it.
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

    /// `completionItem/resolve`: the documentation for the one row the user is looking at.
    ///
    /// Reading a declaration's comments reaches into its definitions, and a five-hundred-row list
    /// would do that five hundred times for the one row anyone reads. LSP avoids exactly that: the
    /// list ships without documentation, and this fills it in.
    fn resolve_completion(&self, params: serde_json::Value) -> Option<CompletionItem> {
        let mut item: CompletionItem = parse_params(params)?;
        let read = |uri: &str| self.read_of(uri);
        let memo = types::Memo::new(&read, &self.exits);
        let sources = self.sources(&read, &memo);
        let modifiers = &memo.modifiers;

        let markdown =
            completion_target(item.data.as_ref()).and_then(|(declaration, precise, guess)| {
                hover::markdown(
                    &self.synthesized,
                    modifiers,
                    &sources,
                    &locator::Resolution {
                        declarations: vec![declaration],
                        // What the list was built from, carried back on the item: a row from the
                        // name-based list is a guess, and its card must say so.
                        precise,
                        redirected: false,
                        // The other guess: the rows are a real class's members, and the class was
                        // read from the receiver's name.
                        derivation: types::Derivation {
                            guess,
                            ..types::Derivation::default()
                        },
                        // Nothing here reads it (a card is about one declaration), and the item has
                        // no class id to fill it with.
                        receiver: None,
                    },
                    // `completionItem/resolve` gets an item, not a position, so there is no cursor
                    // to read a test tree from. `None` is unfenced, which can only keep a place,
                    // never invent one, and the row was already ranked by a list that did ask.
                    None,
                    // Nor a call, so no call's own type.
                    None,
                )
            });

        if let Some(value) = markdown {
            item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }));
        }
        // Always the item, never `null`: the client sent one, and the protocol says it gets one
        // back, enriched or not.
        Some(item)
    }

    /// Where this project's own files are and what `require` can name: the one value every
    /// test-tree-fencing surface takes.
    ///
    /// Built per call, not held, for [`Analysis::sources`]' reason: it borrows two fields the next
    /// settle rewrites, and a held copy would be a third place for them to diverge.
    pub(super) fn layout(&self) -> environment::Layout<'_> {
        environment::Layout {
            root: &self.workspace_prefix,
            load: &self.load_prefixes,
            names: environment::Names::of(&self.workspace.config().trees),
            own: &self.own_prefixes,
            foreign: &self.foreign_prefixes,
            held: Some(self.graph.paths()),
        }
    }

    /// Where every document in the graph sits, built by the first request that asks and held until
    /// the graph changes.
    ///
    /// The closure keeps the table's non-graph inputs out of `indexed.rs`: the directory names
    /// `[trees]` sets and the four prefix lists saying where the project is (one
    /// [`environment::Layout`], for `Fence`'s reason). `indexed::Indexed::forget_placement` drops
    /// the answer when either changes; see [`Placed`](super::indexed::Placed).
    pub(super) fn placed(&self) -> &super::indexed::Placed {
        self.graph.placed(|graph| {
            super::indexed::Placed::of(graph, |uri| self.is_own_code(uri), self.layout())
        })
    }

    /// The documents a *where is this used* list may draw from, for a request rooted in `uri`.
    ///
    /// [`Self::own_documents`], plus, when the request is rooted in a document the project does not
    /// contain, that one document. A use written in the reader's buffer is a use; without this,
    /// `references` from inside a scratch file would return the project's uses but none of the
    /// file's own.
    ///
    /// **Borrowed in every ordinary case**, which is what the `Cow` is for: the set is every
    /// workspace document and the exception is one id, so only the request that needs it pays for
    /// the copy. Every *other* document outside the project stays out, here as everywhere; see
    /// `environment::Outward`.
    fn scope_rooted_in(
        &self,
        rooted: Option<&DocUri>,
    ) -> Cow<'_, std::collections::HashSet<UriId>> {
        let own = self.own_documents();
        match rooted.filter(|uri| self.layout().is_outside(uri.as_str())) {
            None => Cow::Borrowed(own),
            Some(uri) => {
                let mut scope = own.clone();
                scope.insert(UriId::from(uri.as_str()));
                Cow::Owned(scope)
            }
        }
    }

    /// The documents that are the user's own code.
    ///
    /// A borrow of the table above, not a fresh `HashSet` per call: many handlers ask for it,
    /// `completion` on every keystroke, and rebuilding it tested every graph document against the
    /// workspace root, the external load paths and every gem root.
    ///
    /// **Surfaces that also need the documents *outside* the project take the whole table
    /// instead**, because the two sets are read together, and passing both separately at six call
    /// sites invites getting half right. The outside set's predicate compares a URI against every
    /// load path in the bundle, so asking it per document per request is expensive.
    pub(super) fn own_documents(&self) -> &std::collections::HashSet<UriId> {
        self.placed().own()
    }

    /// Turn a graph site into a link, with its ranges placed in the target document.
    fn link(&self, origin: lsp_types::Range, site: &Site) -> Option<LocationLink> {
        let target = DocUri::from_graph_uri(&site.uri)?;
        let target_uri = target.to_lsp().ok()?;
        let (target_range, target_selection_range) = self
            .indexed_ranges(&target, site)
            .or_else(|| self.read_ranges(&target, site))?;
        Some(LocationLink {
            origin_selection_range: Some(origin),
            target_uri,
            target_range,
            target_selection_range,
        })
    }

    /// A site's two ranges, from the line index rubydex already holds for that document.
    ///
    /// - **The fast half of [`Analysis::link`], used by almost every jump.** Placing a span needs
    ///   the line start and the code units before it on that line, and [`position::range_in`] gets
    ///   both from the index without the text, so a jump into a gem, the stdlib or another project
    ///   file costs no `open`, `read` or line scan.
    /// - **Two documents cannot be placed this way, for one reason**: the index is built over the
    ///   text rubydex was given, and here that differs from the client's text. A buffer being typed
    ///   in is ahead of the graph (what [`Rebase`](super::position::Rebase) is for), and a template
    ///   is *read* as the blanked Ruby view but *addressed* as the markup (what
    ///   [`TextDocument::blanked`] is for). Either answers `None`, and [`Analysis::read_ranges`]
    ///   handles it the old way.
    /// - **A third edited text is deliberately not gated.** An `.rbs` reaches rubydex through
    ///   [`signatures::without_interfaces`](super::signatures::without_interfaces), which blanks
    ///   `interface` blocks preserving length and newlines. A column could only move if a *blanked*
    ///   non-ASCII character sat before a declaration **on the declaration's own line**, which
    ///   needs a declaration after an `interface … end` on one line. No vendored signature has such
    ///   a line, and gating `.rbs` would send every jump into a signature back to the disk.
    fn indexed_ranges(
        &self,
        target: &DocUri,
        site: &Site,
    ) -> Option<(lsp_types::Range, lsp_types::Range)> {
        if self.open.contains_key(target) || erb::is_template_uri(target) {
            return None;
        }
        // **The replaced read did two jobs, and the second stays.** A file deleted since indexing
        // is still in the graph with a line index, and would still get two good ranges in text
        // nobody can open. A link promises somewhere to go, and a dead one is worse than none
        // (`a_definition_whose_file_is_gone_answers_nothing_rather_than_a_dead_link`). So the
        // existence check remains, as the single `stat` it always was underneath, instead of
        // reading and scanning the whole file.
        if !target.to_file_path().is_some_and(|path| path.is_file()) {
            return None;
        }
        let document = self.graph.documents().get(&UriId::from(target.as_str()))?;
        let index = document.line_index();
        Some((
            position::range_in(index, self.encoding, site.full.0, site.full.1)?,
            position::range_in(index, self.encoding, site.selection.0, site.selection.1)?,
        ))
    }

    /// A site's two ranges, from the target document's text, read for the purpose.
    ///
    /// **The target's map, not the requester's.** A jump can land in any document, and any open
    /// buffer may be a keystroke ahead of the graph, so the span moves into *that* file's
    /// coordinates. A document nobody has typed in since the last settle has the identity.
    fn read_ranges(
        &self,
        target: &DocUri,
        site: &Site,
    ) -> Option<(lsp_types::Range, lsp_types::Range)> {
        self.with_text(target, |text| {
            let rebase = self.rebase_for(target, text.text());
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
        })?
    }

    /// Where a `require` points, if the graph has that file.
    ///
    /// `require_relative` resolves against the requiring file's directory; a plain `require`
    /// against the configured load paths, as Ruby walks `$LOAD_PATH`.
    fn require_site(&self, from: &DocUri, require: &requires::Require) -> Option<Site> {
        let load_paths = if require.relative {
            vec![from.to_file_path()?.parent()?.to_path_buf()]
        } else {
            self.workspace.load_paths()
        };
        locator::require_site(&self.graph, &require.path, &load_paths)
    }
}

/// A rename checked against the bytes it would replace.
///
/// Byte spans, not ranges, because the two uses are a containment test against the cursor (plain
/// offset arithmetic, versus a two-field comparison on positions) and a conversion.
#[derive(Debug)]
struct Renaming {
    /// The name every one of these spans currently holds.
    name: String,
    /// Whether the replacement must be a constant name rather than a variable's.
    constant: bool,
    /// What to replace, by document: each file's spans in order, files by URI.
    files: Vec<(DocUri, Vec<Replacement>)>,
}

/// One confirmed replacement: its byte offsets, and the range the client is sent.
///
/// Both from the one read. The offsets are what a cursor is compared against and the range is what
/// goes on the wire; converting again elsewhere, perhaps from a re-read file, is two chances to
/// disagree.
#[derive(Debug)]
struct Replacement {
    start: u32,
    end: u32,
    range: lsp_types::Range,
}

/// The last segment of a URI's path: what a message about a file names.
pub(super) fn file_name(uri: &DocUri) -> String {
    uri.as_str()
        .rsplit_once('/')
        .map_or(uri.as_str(), |(_, name)| name)
        .to_owned()
}

/// Byte spans to LSP ranges, reading each document at most once.
///
/// A project-wide answer names hundreds of spans across a few files, and converting one span means
/// reading and line-indexing its whole file. Per span, a file with fifty references to a method
/// would be read fifty times.
struct Ranges<'a> {
    analysis: &'a Analysis,
    /// `None` for a document that could not be read, so a missing file is not retried per span
    /// either.
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

    /// The bytes a span covers, for a caller that must know what it is replacing.
    ///
    /// `None` for a span running past the end of the *current* text: what a plan made before an
    /// edit looks like, and another reason `rename` reads every span back instead of trusting its
    /// offsets.
    fn text_at(&mut self, uri: &DocUri, start: u32, end: u32) -> Option<String> {
        self.with(uri, |text| {
            Some(text.text().get(start as usize..end as usize)?.to_owned())
        })?
    }

    fn with<R>(&mut self, uri: &DocUri, read: impl FnOnce(&TextDocument) -> R) -> Option<R> {
        // An open buffer shadows disk and is already indexed; never cached, since the copy would go
        // stale at the next keystroke.
        if let Some(open) = self.analysis.open.get(uri) {
            return Some(read(&open.text));
        }
        let encoding = self.analysis.encoding;
        self.read
            .entry(uri.clone())
            .or_insert_with(|| {
                let text = std::fs::read_to_string(uri.to_file_path()?).ok()?;
                Some(TextDocument::new(text, encoding))
            })
            .as_ref()
            .map(read)
    }
}

/// What `completionItem/resolve` is given to find the row again.
///
/// An object, not the bare id string the type hierarchy sends, because the card must also say which
/// tier the row came from, and only the row still knows when the client asks. Both halves are
/// strings: a `DeclarationId` is a 64-bit hash and JSON numbers are doubles, so a client round trip
/// would corrupt a number.
fn completion_data(
    declaration: DeclarationId,
    precise: bool,
    guess: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "declaration": declaration.get().to_string(),
        "precise": precise,
        // Absent rather than `null` when the receiver was not guessed, so the common row has two
        // fields, not three.
        "guess": guess,
    })
}

/// The declaration a resolve request names, and which tier its list came from.
///
/// Anything else (a stale id, an older client's bare string, nothing) answers `None`, and the item
/// comes back unenriched, as the protocol requires. The guess is optional where `precise` is not: a
/// client holding a list from an older server still resolves; it just cannot say what it was never
/// told.
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

/// The declaration an item's `data` field names, as three requests round-trip one.
///
/// A decimal string, not a JSON number, because a `DeclarationId` is a 64-bit hash and JSON
/// numbers are doubles. The value is the client's word: it echoes whatever its list carried, and
/// a config reload drops the graph that list came from, so nothing here may assume the id still
/// names anything.
fn declaration_in(data: Option<&serde_json::Value>) -> Option<DeclarationId> {
    data?
        .as_str()?
        .parse::<u64>()
        .ok()
        .filter(|raw| *raw != 0)
        .map(DeclarationId::new)
}

/// Whether this request may be answered without indexing what was just typed.
///
/// - **The three a caret asks** (`completion`, `hover`, `definition`): their whole answer depends
///   on the cursor and the graph. Each needs `Rebase::to_graph` on the way in; `hover` and
///   `definition` also need `span_to_buffer` on the way out, because they answer with a span
///   `locate` found in the graph, and `definition`'s can be in yet another document (see
///   `Analysis::link`).
/// - **The rest are left out on demand, not principle.** `references`, `documentHighlight` and the
///   hierarchies answer about the *workspace*, nobody asks them between keystrokes, and each would
///   need every returned span mapped through its own document's map. `signatureHelp` reads only the
///   buffer, and the three [`needs_the_graph`] exempts never settle.
fn defers(method: &str) -> bool {
    matches!(
        method,
        "textDocument/completion" | "textDocument/hover" | "textDocument/definition"
    )
}

/// Whether a request's answer comes from the graph at all, which decides whether it should wait for
/// a server still starting.
///
/// `foldingRange`, `selectionRange` and `semanticTokens/full` are pure functions of one buffer
/// (neither `analysis::ranges` nor `analysis::tokens` sees a `Graph`), so they do not. Semantic
/// tokens fire on the first keystroke in a newly opened file, exactly when a cold index is still
/// building, and waiting would leave the file uncoloured.
fn needs_the_graph(method: &str) -> bool {
    !matches!(
        method,
        "textDocument/foldingRange"
            | "textDocument/selectionRange"
            | "textDocument/semanticTokens/full"
    )
}

/// Whether a request must also wait for an edit the index has not caught up with.
///
/// - **A much shorter interval, and `codeAction` is the one method answering differently.** Four of
///   its five actions are Prism rewrites of the buffer that read no graph. The fifth (the jump into
///   a generated document) reads one, but would pay at the wrong moment: clients ask for code
///   actions on every cursor move, and `settle` starts by indexing whatever `didChange` deferred,
///   hundreds of milliseconds for a large file. So a lagging index costs only that one action,
///   which returns on the next cursor move.
/// - **A cold server is a different shortfall, not traded away**: the graph holds nothing, the
///   whole menu would be empty, and the drain rung in [`Analysis::serve`] runs for this method like
///   any other.
fn settles(method: &str) -> bool {
    needs_the_graph(method) && method != "textDocument/codeAction"
}

/// Whether a client that filtered its menu asked for actions of this kind.
///
/// - **Kinds are a dotted hierarchy**: a request for `refactor` includes every `refactor.*`, so the
///   test is a prefix on a segment boundary. `refactor` must not match `refactoring.something`,
///   which is why the boundary is checked.
/// - **Two empty cases, with opposite meanings.** An **absent** `only` is no filter (what a
///   lightbulb sends), so everything is offered. An `only` holding the **empty kind** asks for
///   kindless actions: the one this module returns for a generated document, and nothing else.
fn asked_for(wanted: &[CodeActionKind], kind: &CodeActionKind) -> bool {
    if wanted.is_empty() {
        return true;
    }
    let kind = kind.as_str();
    wanted.iter().any(|asked| {
        let asked = asked.as_str();
        asked == kind
            || (!asked.is_empty()
                && kind.starts_with(asked)
                && kind.as_bytes().get(asked.len()) == Some(&b'.'))
    })
}

/// The menu sentence for a document a generator wrote.
///
/// Both halves, because either alone is ambiguous. The **body** is what the reader is in (`Story`);
/// the **source** is which generator wrote this copy, the question a Rails developer actually has:
/// `db/schema.rb` said `title` is a `String` and `app/models/story.rb` said `author` is an
/// `Author`, so the same class is opened from two files. The source is a path, not a URI, because a
/// URI in a menu is a paragraph.
///
/// A URI this cannot read never occurs (every one is
/// [`synthesized::generated_uri`](super::synthesized::generated_uri)'s output). It still answers
/// with the URI instead of declining, because an empty menu entry is worse than an ugly one.
fn show_generated_title(generated: &str) -> String {
    let Some((source, body)) = generated.rsplit_once('#') else {
        return generated.to_owned();
    };
    let body = body.split_once(':').map_or(body, |(_, name)| name);
    let source = source.rsplit_once('/').map_or(source, |(_, file)| file);
    format!("Show the RBS ya-lsp generated for {body}, from {source}")
}

/// ya-lsp's own suggestion kinds, in LSP's vocabulary.
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

/// Whether a response carries no answer at all, in either shape.
///
/// `reply(id, None)` serialises to `null`, and a `CompletionList` that matched nothing has empty
/// `items`. A deferred request retries on both: the refusal behind the retry produces the first,
/// and an empty list is cheap enough to re-derive that telling them apart would change nothing.
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

/// Which document and position a request names, for the line saying it arrived.
///
/// Read from the raw params, because this runs before `dispatch` picks a handler, and every params
/// type shares exactly these two fields. A request naming neither (`workspace/symbol`,
/// `completionItem/resolve`) logs a dash instead of looking like a request about nothing.
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

/// Which tiers a **jump** may answer from: the one exhaustive match in the crate that says so.
///
/// - **Three requests refuse a guess** (`implementation`, `typeDefinition`, `declaration`), for one
///   reason, so the rule is one function, not three comparisons that could drift.
/// - **The reason is `hints.md`'s: where the tier cannot be shown, the bottom tier is not drawn.**
///   A card can footnote *matched on the name alone*; a jump has nowhere to put that, and a reader
///   landing in a `def` cannot see the server was guessing which one.
/// - **A `match`, not a comparison**, so a fourth tier is a **compile error** here rather than a
///   silent promotion: someone must decide in this function, as
///   [`Derivation::tier`](super::types::Derivation::tier) forces one layer down.
const fn jumpable(tier: types::Tier) -> bool {
    match tier {
        // The code named the type, or something ya-lsp followed did. Both can be checked by going
        // where the reader was sent.
        types::Tier::Resolved | types::Tier::Derived => true,
        types::Tier::Guessed => false,
    }
}

/// What the client was actually told, in one word.
///
/// **`nothing` and `empty` differ** and the log keeps them apart: `null` is a cursor the server
/// could make nothing of, empty `items` a list that matched nothing, and they are fixed in
/// different modules. `cancelled` is the client changing its mind, not a defect; `superseded` a
/// held hint request a newer one replaced; `failed` is an error response, which apart from a crash
/// means a method this build does not answer.
fn outcome(response: &Response) -> &'static str {
    match &response.response_result {
        Err(error) if error.code == ErrorCode::RequestCanceled as i32 => "cancelled",
        Err(error) if error.code == ErrorCode::ContentModified as i32 => "superseded",
        Err(_) => "failed",
        Ok(serde_json::Value::Null) => "nothing",
        Ok(_) if answered_nothing(response) => "empty",
        Ok(_) => "answered",
    }
}

/// The same request again, for the two retry rungs in [`Analysis::serve`].
///
/// `dispatch` takes the `Request` by value (it hands the params to a handler), so a retry must
/// build a new one. One function, so the two rungs read as the same act for different reasons.
fn asked_again(id: &RequestId, method: &str, params: serde_json::Value) -> Request {
    Request {
        id: id.clone(),
        method: method.to_owned(),
        params,
    }
}

/// Wrap a handler's answer as a successful response.
///
/// `None` becomes JSON `null`, LSP's spelling of "there is nothing here". An error response would
/// be wrong: editors show those to the user, and "no definition found" is not a complaint.
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

/// Request params are client input: malformed ones are logged and answered with `null`, never a
/// panic.
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

    #[test]
    fn an_instance_variable_the_file_never_writes_is_not_a_template_variable() {
        // `template_variable_links` runs at **every** `definition` on an instance variable its file
        // never assigns (including a subclass reading its superclass's), and checks the path first,
        // so an ordinary Ruby file pays one look at its own name for a rung it can never reach.
        // This test pins that the look happens and stops there.
        let mut harness = Harness::new();
        harness.write(
            "app/models/parent.rb",
            "class Parent\n  def initialize\n    @name = \"x\"\n  end\nend\n",
        );
        let source = "class Child < Parent\n  def show\n    @name\n  end\nend\n";
        let child = harness.write("app/models/child.rb", source);
        harness.index();

        // The answer comes from the scope walk and the graph, never the view rung: a `.rb` file has
        // no renderer to search for writes.
        let found = harness.definition_at(&child, source, "@name\n");
        assert!(
            found.is_null() || !found.to_string().contains("child.rb"),
            "the file never writes it, so nothing in it is the definition: {found}"
        );
    }

    #[test]
    fn every_surface_answers_about_a_document_with_no_file_behind_it() {
        // **One buffer, every handler that takes a `textDocument`.** A document with no path is
        // new, and half the request layer reads one: `with_text` falls back to disk,
        // `indexed_ranges` checks the target exists, `require_site` resolves a relative require
        // against the asking file's directory, and `renaming` asks whose code it is. Each has a
        // `None` arm no document could reach before, and an unreached arm is an unread one.
        //
        // Asserted per surface, not just "no panic": the two that must decline say so, and the rest
        // answer about the buffer as they would about a file.
        let mut harness = Harness::new();
        harness.write(
            "app/models/store.rb",
            "class Store\n  def restock(count)\n  end\nend\n",
        );
        harness.index();

        let source = concat!(
            "require_relative \"sibling\"\n",
            "\n",
            "class Draft\n",
            "  def run\n",
            "    store = Store.new\n",
            "    store.restock(1)\n",
            "  end\n",
            "end\n",
        );
        let uri = DocUri::from_lsp(&"untitled:Untitled-7".parse().expect("a uri"))
            .expect("an unsaved buffer is a document");
        harness.open(&uri, source);

        let at = |needle: &str| {
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": position_of(source, needle),
            })
        };

        // The buffer is read like any file, by everything that needs only its text.
        for (method, params) in [
            (
                "textDocument/documentSymbol",
                serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
            ),
            (
                "textDocument/foldingRange",
                serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
            ),
            (
                "textDocument/semanticTokens/full",
                serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
            ),
            ("textDocument/hover", at("Store.new")),
            ("textDocument/documentHighlight", at("store.restock")),
            (
                "textDocument/selectionRange",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "positions": [position_of(source, "Store.new")],
                }),
            ),
        ] {
            assert!(
                !harness.ask(method, params).is_null(),
                "{method} answered nothing about an unsaved buffer"
            );
        }

        // And the graph answers about it: the project's own class resolves from inside it.
        assert!(
            !harness
                .ask("textDocument/definition", at("Store.new"))
                .is_null()
        );

        // **`require_relative` from a buffer with no directory resolves nowhere**, the one thing an
        // unsaved buffer loses, for free, since there is no directory to resolve against. A file
        // beside the project still links to its sibling; this cannot, and says so with `null`
        // instead of a link into someone else's file.
        assert!(
            harness
                .ask(
                    "textDocument/documentLink",
                    serde_json::json!({ "textDocument": { "uri": uri.as_str() } })
                )
                .is_null(),
            "an unsaved buffer has no directory to resolve a relative require against"
        );

        // **Rename is refused, as in a gem and for the same reason**: ya-lsp never proposes an edit
        // to a document that is not the user's own code, and a buffer outside the project is not.
        // An existing rule reaching a new document, not a new rule.
        assert!(
            harness
                .ask("textDocument/prepareRename", at("Draft"))
                .is_null(),
            "a rename is not offered in a document the project does not contain"
        );
    }

    /// The one spelling of a generated URI a client may send back that differs from what it was
    /// given: VS Code parses URIs into its own type, decoding each component, and re-encodes
    /// everything outside the unreserved set plus `/`, so a `:` in the path and one in the fragment
    /// both return as `%3A`.
    fn as_vscode_spells_it(uri: &str) -> String {
        uri.replacen("ya-lsp-generated:", "ya-lsp-generated\u{0}", 1)
            .replace(':', "%3A")
            .replacen('\u{0}', ":", 1)
    }

    #[test]
    fn a_member_no_file_declares_is_offered_the_document_the_generator_wrote() {
        // `title` is a column. No file writes `def title`, so the only statement of what it returns
        // is RBS this crate generated, readable only through this action.
        let (mut harness, _, uri) = rails_project("Story.new.title\n");
        let titles = harness.code_action_titles(&uri, 0, 12);
        assert_eq!(
            titles,
            vec!["Show the RBS ya-lsp generated for Story, from schema.rb"]
        );

        let command = harness
            .code_action_command(&uri, 0, 12, &titles[0])
            .expect("the action carries a command");
        assert_eq!(command["command"], harness.show_generated_command());
        let named = command["arguments"][0].as_str().expect("one argument");
        assert!(named.starts_with("ya-lsp-generated:"), "{named}");
        assert!(named.ends_with("/db/schema.rb#class:Story"), "{named}");
    }

    #[test]
    fn a_member_a_file_really_declares_is_offered_nothing() {
        // The negative half, showing the action is about *generated* declarations, not members:
        // `def title` is written down, so the reader is one goto-definition from everything ya-lsp
        // knows and there is nothing to show.
        let mut harness = Harness::new();
        harness.write(
            "app/models/story.rb",
            "class Story\n  def title\n    \"t\"\n  end\nend\n",
        );
        let uri = harness.write("app/main.rb", "Story.new.title\n");
        harness.index();
        assert!(harness.code_action_titles(&uri, 0, 12).is_empty());
    }

    #[test]
    fn the_macro_that_declared_the_member_is_a_cursor_too_and_it_is_the_one_this_is_for() {
        // `belongs_to :user` declares `user`, `user=`, `build_user` and `create_user`, writing none
        // down. A reader on that line asking *what did this do* is the feature's purpose, and the
        // action's initial walk never reaches it, because a symbol is not a call.
        let (mut harness, _, uri) = rails_project("Story.new.title\n");
        let model = harness.write(
            "app/models/story.rb",
            "class Story < ApplicationRecord\n  belongs_to :user\nend\n",
        );
        harness.write(
            "app/models/user.rb",
            "class User < ApplicationRecord\nend\n",
        );
        harness.index();
        let _ = uri;

        let titles = harness.code_action_titles(&model, 1, 14);
        assert_eq!(
            titles,
            vec!["Show the RBS ya-lsp generated for Story, from story.rb"]
        );
        let command = harness
            .code_action_command(&model, 1, 14, &titles[0])
            .expect("a command");
        let named = command["arguments"][0].as_str().expect("one argument");
        assert!(
            named.ends_with("/app/models/story.rb#class:Story"),
            "{named}"
        );

        // And the document really says what the macro declared.
        let answer = harness.ask(
            "workspace/textDocumentContent",
            serde_json::json!({ "uri": named }),
        );
        let rbs = answer["text"].as_str().expect("text");
        assert!(rbs.contains("def user: () -> User"), "{rbs}");
    }

    #[test]
    fn a_member_written_down_and_generated_both_offers_the_document_once() {
        // The ordinary Rails case, showing this is about *definitions*, not declarations: `title`
        // is a column **and** a `def` someone wrote, one declaration with two definitions (why a
        // schema answer is *derived*, not resolved). `definition` already reaches the written half;
        // the generated half has no document, so the action is still offered, once.
        let (mut harness, _, uri) = rails_project("Story.new.title\n");
        harness.write(
            "app/models/story.rb",
            "class Story\n  def title\n    \"t\"\n  end\nend\n",
        );
        harness.index();
        let titles = harness.code_action_titles(&uri, 0, 12);
        assert_eq!(
            titles,
            vec!["Show the RBS ya-lsp generated for Story, from schema.rb"]
        );
    }

    #[test]
    fn a_receiver_matched_on_a_name_alone_is_never_handed_a_generated_document() {
        // The gotos' gate, on a fourth surface. `thing` is an untyped parameter, so `title` is
        // reached by matching the name across the graph, and the document it would open is headed
        // `class Story` and written by this server: the most authoritative-looking page in the
        // index, reached by a guess.
        let (mut harness, _, uri) = rails_project("def handle(thing)\n  thing.title\nend\n");
        assert!(harness.code_action_titles(&uri, 1, 9).is_empty());

        // The same cursor answers a card, so the refusal is this gate, not the cursor finding
        // nothing.
        let hover = harness.ask(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": 1, "character": 9 },
            }),
        );
        assert!(!hover.is_null(), "{hover}");
    }

    #[test]
    fn a_file_the_server_cannot_read_is_a_menu_with_nothing_in_it() {
        // Under the workspace root (so the refactorings' gate lets it through), not on disk and not
        // open: what a client asking about a file deleted under it looks like. Both families answer
        // nothing; neither panics.
        let mut harness = Harness::new();
        harness.write("app/models/story.rb", "class Story\nend\n");
        harness.index();
        let missing =
            DocUri::from_path(&harness.root.path().join("app/models/ghost.rb")).expect("a uri");
        assert!(harness.code_action_titles(&missing, 0, 0).is_empty());
    }

    #[test]
    fn a_client_that_cannot_read_the_document_is_not_offered_the_door_to_it() {
        // Neovim's shape, and why this gate exists: it answers `window/showDocument` (opening a
        // buffer named after the URI), but nothing in its client would fill it, so the user would
        // get an empty window whose name looks like a bug.
        let (mut harness, _, uri) = rails_project("Story.new.title\n");
        harness.takes_no_generated_content();
        assert!(harness.code_action_titles(&uri, 0, 12).is_empty());

        // The other half off: a client that could read the document but cannot be shown one. Same
        // answer, because the pair is what makes the feature work.
        let (mut harness, _, uri) = rails_project("Story.new.title\n");
        harness.takes_no_show_document();
        assert!(harness.code_action_titles(&uri, 0, 12).is_empty());
    }

    #[test]
    fn the_command_asks_the_client_to_show_the_document_and_nothing_else_does() {
        let (mut harness, _, uri) = rails_project("Story.new.title\n");
        let titles = harness.code_action_titles(&uri, 0, 12);
        let command = harness
            .code_action_command(&uri, 0, 12, &titles[0])
            .expect("a command");
        let named = command["arguments"][0].as_str().expect("one argument");

        let answer = harness.ask(
            "workspace/executeCommand",
            serde_json::json!({ "command": harness.show_generated_command(), "arguments": [named] }),
        );
        // `null`, because the protocol has no shape for *the command ran*; its work travels the
        // other way as a request of its own.
        assert_eq!(answer, serde_json::Value::Null);

        let shown = harness.requests("window/showDocument");
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].params["uri"], named);
        assert_eq!(shown[0].params["external"], false);
    }

    #[test]
    fn a_command_argument_this_server_did_not_write_opens_nothing() {
        // The argument comes from whatever ran the command (a lightbulb, a keybinding, another
        // extension), so it is checked against the table, not forwarded. Making an editor open a
        // URI someone else chose is the one way this command could be abused, and it is a file read
        // on the user's machine.
        let (mut harness, schema, _) = rails_project("Story.new.title\n");
        for argument in [
            serde_json::json!(schema.as_str()),
            serde_json::json!("ya-lsp-generated:file:///nowhere/db/schema.rb#class:Nothing"),
        ] {
            let answer = harness.ask(
                "workspace/executeCommand",
                serde_json::json!({ "command": harness.show_generated_command(), "arguments": [argument] }),
            );
            assert_eq!(answer, serde_json::Value::Null);
            assert!(harness.requests("window/showDocument").is_empty());
        }

        // A command this server never registered: the same refusal, since `executeCommandProvider`
        // names one command and a client may send any string.
        let answer = harness.ask(
            "workspace/executeCommand",
            serde_json::json!({ "command": "rubocop.formatAll", "arguments": [] }),
        );
        assert_eq!(answer, serde_json::Value::Null);
        assert!(harness.requests("window/showDocument").is_empty());
    }

    #[test]
    fn the_document_reads_as_the_rbs_the_generator_wrote_however_the_client_spells_it() {
        let (mut harness, _, uri) = rails_project("Story.new.title\n");
        let titles = harness.code_action_titles(&uri, 0, 12);
        let command = harness
            .code_action_command(&uri, 0, 12, &titles[0])
            .expect("a command");
        let named = command["arguments"][0]
            .as_str()
            .expect("one argument")
            .to_owned();

        // The server's own spelling, what a client echoing its input sends.
        let answer = harness.ask(
            "workspace/textDocumentContent",
            serde_json::json!({ "uri": named }),
        );
        let text = answer["text"].as_str().expect("text").to_owned();
        assert!(text.contains("class Story"), "{text}");
        assert!(text.contains("def title: () -> String"), "{text}");

        // And VS Code's, which differs: `%3A` for both colons, same document.
        let rewritten = as_vscode_spells_it(&named);
        assert_ne!(rewritten, named);
        let answer = harness.ask(
            "workspace/textDocumentContent",
            serde_json::json!({ "uri": rewritten }),
        );
        assert_eq!(answer["text"].as_str(), Some(text.as_str()));
    }

    #[test]
    fn nothing_but_a_document_this_server_wrote_is_ever_served() {
        // The first request whose argument is a URI the client did not read from an open file, so
        // the scheme test is a rule, not luck: a reader that answered a `file:` URI would be a file
        // server nobody asked for.
        //
        // An **error**, not `null`, this handler's one departure from the file's rule: the
        // protocol's result has no null arm, and a window with two Ruby folders has two content
        // providers, only one of which wrote the document.
        let (mut harness, schema, _) = rails_project("Story.new.title\n");
        for asked in [
            serde_json::json!(schema.as_str()),
            serde_json::json!("ya-lsp-generated:file:///nowhere/db/schema.rb#class:Nothing"),
            serde_json::json!(""),
            serde_json::Value::Null,
        ] {
            let answer = harness.ask_raw(
                "workspace/textDocumentContent",
                serde_json::json!({ "uri": asked }),
            );
            let error = answer
                .response_result
                .expect_err("an error rather than a null document");
            assert_eq!(error.code, ErrorCode::InvalidParams as i32, "{asked}");
            assert!(
                error.message.starts_with("ya-lsp generated no document"),
                "{error:?}"
            );
        }
    }

    #[test]
    fn a_reader_is_told_when_the_generator_rewrites_what_it_is_reading() {
        // What keeps the document honest: there is no file, so no editor reload, watcher or
        // `didChange` reaches it, and a reader who adds a column would otherwise see the
        // pre-migration version indefinitely.
        let (mut harness, _, uri) = rails_project("Story.new.title\n");
        let titles = harness.code_action_titles(&uri, 0, 12);
        let command = harness
            .code_action_command(&uri, 0, 12, &titles[0])
            .expect("a command");
        let named = command["arguments"][0]
            .as_str()
            .expect("one argument")
            .to_owned();
        // Asked for in the client's own spelling, which the refresh must name back.
        let rewritten = as_vscode_spells_it(&named);
        harness.ask(
            "workspace/textDocumentContent",
            serde_json::json!({ "uri": rewritten }),
        );
        let _ = harness.requests("workspace/textDocumentContent/refresh");

        harness.write(
            "db/schema.rb",
            &SCHEMA_RB.replace("t.text \"description\"", "t.integer \"score\""),
        );
        harness.index();

        let refreshed = harness.requests("workspace/textDocumentContent/refresh");
        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0].params["uri"], rewritten);

        // And the document now reads as the migration left it.
        let answer = harness.ask(
            "workspace/textDocumentContent",
            serde_json::json!({ "uri": rewritten }),
        );
        let text = answer["text"].as_str().expect("text");
        assert!(text.contains("def score: () -> Integer"), "{text}");
    }

    #[test]
    fn a_document_nobody_asked_about_is_never_refreshed() {
        // The refresh reaches only what is being read. A cold index regenerates every body, and a
        // request per body, at a cold start's busiest moment, about unopened documents, would be a
        // storm for nothing.
        let (mut harness, _, _) = rails_project("Story.new.title\n");
        let _ = harness.requests("workspace/textDocumentContent/refresh");
        harness.write(
            "db/schema.rb",
            &SCHEMA_RB.replace("t.text \"description\"", "t.integer \"score\""),
        );
        harness.index();
        assert!(
            harness
                .requests("workspace/textDocumentContent/refresh")
                .is_empty()
        );
    }

    #[test]
    fn a_menu_the_client_filtered_gets_what_it_filtered_for() {
        // `only` is honoured, which became necessary once a kindless action existed: the empty kind
        // is advertised so the lightbulb keeps asking, and in exchange a client asking only for
        // `quickfix` must not receive four refactorings and a jump.
        let (mut harness, _, uri) = rails_project("Story.new.title\n");
        let at = serde_json::json!({ "line": 0, "character": 12 });
        let ask = |harness: &mut Harness, only: serde_json::Value| {
            harness.ask(
                "textDocument/codeAction",
                serde_json::json!({
                    "textDocument": { "uri": uri.as_str() },
                    "range": { "start": at, "end": at },
                    "context": { "diagnostics": [], "only": only },
                }),
            )
        };
        assert_eq!(
            ask(&mut harness, serde_json::json!(["quickfix"])),
            serde_json::Value::Null
        );
        assert_eq!(
            ask(&mut harness, serde_json::json!(["refactor"])),
            serde_json::Value::Null
        );
        let kindless = ask(&mut harness, serde_json::json!([""]));
        assert_eq!(kindless.as_array().map(Vec::len), Some(1));
        assert_eq!(
            kindless[0]["command"]["command"],
            harness.show_generated_command()
        );
    }

    #[test]
    fn a_kind_is_matched_on_the_hierarchy_the_protocol_defines_and_not_on_the_letters() {
        let extract = CodeActionKind::REFACTOR_EXTRACT;
        // No filter means every action, which is what a lightbulb sends.
        assert!(asked_for(&[], &extract));
        assert!(asked_for(&[], &CodeActionKind::EMPTY));
        // A parent asks for its children.
        assert!(asked_for(&[CodeActionKind::REFACTOR], &extract));
        assert!(asked_for(std::slice::from_ref(&extract), &extract));
        // A prefix that is not a segment does not: `refactor` must not match
        // `refactoring.anything`, which is why the boundary is checked.
        assert!(!asked_for(&[CodeActionKind::new("refactorings")], &extract));
        assert!(!asked_for(&[CodeActionKind::QUICKFIX], &extract));
        // The empty kind asks for kindless actions and matches nothing else, either way: it is not
        // a prefix of every string.
        assert!(!asked_for(&[CodeActionKind::EMPTY], &extract));
        assert!(!asked_for(
            &[CodeActionKind::REFACTOR],
            &CodeActionKind::EMPTY
        ));
        assert!(asked_for(&[CodeActionKind::EMPTY], &CodeActionKind::EMPTY));
    }

    #[test]
    fn a_title_with_no_body_in_it_is_the_uri_rather_than_an_empty_menu_entry() {
        // Unreachable through `generated_uri`, which always writes the `#`. Pinned because an empty
        // menu entry is worse than an ugly one, and the ordinary shape belongs beside the fallback.
        assert_eq!(
            show_generated_title("ya-lsp-generated:file:///p/db/schema.rb#class:Story"),
            "Show the RBS ya-lsp generated for Story, from schema.rb"
        );
        assert_eq!(
            show_generated_title("ya-lsp-generated:file:///p/db/schema.rb"),
            "ya-lsp-generated:file:///p/db/schema.rb"
        );
        // A body with no kind before it, and a source with no directory above it.
        assert_eq!(
            show_generated_title("schema.rb#Story"),
            "Show the RBS ya-lsp generated for Story, from schema.rb"
        );
    }

    /// Every method `dispatch` answers, read from `dispatch` itself.
    ///
    /// A hand-written list would be a second copy of the table, stale as soon as someone adds a
    /// method, exactly the method whose silence nobody would notice. `messages.rs` reads its own
    /// source for the same reason.
    fn dispatched_methods() -> Vec<String> {
        include_str!("requests.rs")
            .lines()
            .map(str::trim)
            .filter_map(|line| line.strip_prefix('"'))
            .filter_map(|rest| rest.split_once("\" => "))
            // Three shapes an arm takes: `reply(...)` for a handler answering an optional result, a
            // bare call for one answering a `Response` itself, and `{` where rustfmt broke a long
            // line. The fallback arm starts `method =>`, never `"`, so the prefix above cannot
            // reach it.
            .filter(|(_, tail)| {
                tail.starts_with("reply(") || tail.starts_with("self.") || *tail == "{"
            })
            .map(|(method, _)| method.to_owned())
            .collect()
    }

    #[test]
    fn every_method_says_it_arrived_and_says_what_it_answered() {
        // Without the log pair, "hover does nothing in this file" and "the client never sent a
        // hover" produce identical logs, and the second is what a document selector one folder too
        // narrow really does. One pair per request, whatever the request was worth.
        let methods = dispatched_methods();
        assert_eq!(methods.len(), 29, "{methods:?}");

        let mut harness = Harness::new();
        let source = "class Story\n  def title\n  end\nend\n";
        let uri = harness.write("app/models/story.rb", source);
        harness.index();

        // One params object with every field the thirty methods read between them. Methods it does
        // not fit answer `null`, which is the point: the pair is written whether or not the request
        // was worth anything.
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
            "files": [],
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
            assert!(
                answered[0].contains("retried=") && answered[0].contains("drained="),
                "{method} answered without saying which rung answered it: {logged}"
            );
        }
    }

    #[test]
    fn the_pair_names_the_document_and_the_position_it_was_asked_about() {
        // Paths and positions, never a line of source: `logging.rs` holds that rule, and this
        // asserts the request half.
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

        // A request naming no document says so, instead of being logged as one about nothing.
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
        // The distinction the log exists for: `null` is a cursor the server could make nothing of,
        // and empty `items` a list that matched nothing. They are fixed in different modules, and a
        // log calling both "empty" would send every report to the wrong one.
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
    /// unguarded panicking handler takes the analysis thread with it, and a cursor on a `def` can
    /// reach one.
    ///
    /// Armed, not provoked. No unwrap in the pinned rev reaches it (the test below asserts that
    /// Ruby *answers*), so the seam is exercised deliberately. It stays because
    /// `create_declaration`'s unwraps survive upstream, and every handler under `dispatch` reads
    /// the graph.
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
    /// `Graph::find_self_receiver_declaration` unwraps twice on 0.2.5; upstream's `ab88ef1` fixes
    /// it. This pins the fix: moving back to 0.2.5 fails here instead of quietly killing the test
    /// thread under the guard above.
    #[test]
    fn a_constant_alias_reopened_under_its_alias_answers_rather_than_crashing() {
        let mut harness = Harness::new();
        let source = "class Bar; end\nAliased = Bar\nclass Aliased\n  def self.foo; end\nend\n";
        let uri = harness.write("app/bar.rb", source);
        harness.index();

        let call = "Bar.foo\nAliased.foo\n";
        let call_uri = harness.write("app/call.rb", call);
        harness.index();

        // The cursor on the `def` is how the report reached the unwraps. Upstream's fix answers
        // `None`, leaving the line silent; `locator` follows the alias itself, so the `def`
        // declares on the class the alias names, like any other `def self.` line.
        assert!(!harness.definition_at(&uri, source, "foo").is_null());
        // What a user actually writes resolves, under either spelling of the constant.
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
        // An error response is something editors show the user. "No definition here" is an answer,
        // not an error.
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

        // Malformed params are client input and must not take the thread down.
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
        // Every answer goes out through `reply`. `serde_json::to_value` cannot fail for any type it
        // is given today, but the protocol has no shape for "no response": a client that sent an id
        // waits forever. An `InternalError` is the only exit that lets the editor carry on.
        let id = RequestId::from(7);
        let ok = reply(&id, Some("fine"));
        assert_eq!(ok.response_result.expect("a result"), "fine");

        // A map with non-string keys is what `serde_json` actually refuses; a non-finite float is
        // quietly written as `null`.
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
        // The same contract as every handler: `[]` claims the project has no such symbol, which is
        // only true by accident.
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
    // -----------------------------------------------------------------------
    // The cold server
    // -----------------------------------------------------------------------

    /// The one name in the bundle, and the one name in the workspace.
    const COLD_MAIN: &str = "Shouty::Megaphone.new\n";
    const COLD_STORY: &str = "class Story\nend\n";

    /// The state the three tests below start from: the workspace is in the graph, and the bundle
    /// is queued with not one batch indexed.
    ///
    /// Not contrived. The run loop steps the background index only `if receiver.is_empty()`, and
    /// a client whose `initialize`, `initialized`, `didOpen` and first request all arrive within
    /// one second never leaves it a gap, which is exactly what Claude Code does.
    fn cold_server_with_a_queued_bundle() -> (Harness, DocUri, DocUri) {
        let (dir, gem_home, env) =
            project_with_gem("module Shouty\n  class Megaphone\n  end\nend\n");
        // The gem home is a `TempDir` and dropping it takes the gem with it, so it is leaked
        // rather than returned through a tuple every caller would have to keep alive by name.
        std::mem::forget(gem_home);
        let mut harness = Harness::at_with_env(dir, PositionEncoding::Utf16, env);
        let main = harness.write("app/main.rb", COLD_MAIN);
        let story = harness.write("app/models/story.rb", COLD_STORY);
        harness.index();
        harness.analysis.queue_background_indexing();
        assert!(
            harness.analysis.stage.indexing_bundle(),
            "the fixture has to leave the bundle queued and unindexed"
        );
        (harness, main, story)
    }

    #[test]
    fn the_first_request_on_a_cold_server_drains_the_bundle_rather_than_answering_nothing() {
        // The bug: the first answer used a graph with only the workspace, so a gem-declared name
        // hovered as nothing, and an agent, unlike a person, records that as fact and never asks
        // again. The deferred retry fires but does not help, because `settle` links the graph as it
        // stands.
        let (mut harness, main, _story) = cold_server_with_a_queued_bundle();

        let (hover, logged) = crate::testing::captured_logs(tracing::Level::DEBUG, || {
            harness.hover_at(&main, COLD_MAIN, "Megaphone")
        });
        assert!(
            !hover.is_null(),
            "a class declared in the bundle hovered as nothing on a cold server"
        );
        assert!(
            logged.contains("outcome=\"answered\"") && logged.contains("drained=true"),
            "the pair has to say which rung answered: {logged}"
        );

        // Paid once: the pipeline is `Ready` now, so the rung is not even considered.
        assert!(harness.analysis.stage.is_ready());
        let (hover, logged) = crate::testing::captured_logs(tracing::Level::DEBUG, || {
            harness.hover_at(&main, COLD_MAIN, "Megaphone")
        });
        assert!(!hover.is_null());
        assert!(
            logged.contains("drained=false"),
            "the second request on the same server drained again: {logged}"
        );
    }

    #[test]
    fn the_cold_rung_is_wider_than_the_three_a_caret_asks() {
        // `defers` names `completion`, `hover` and `definition` because it concerns a caret racing
        // the index. A cold server is a different question that every graph-reading method has:
        // `prepareTypeHierarchy` is not deferred, settles up front, and still finds nothing until
        // the bundle is in.
        let (mut harness, main, _story) = cold_server_with_a_queued_bundle();

        let prepared = harness.ask(
            "textDocument/prepareTypeHierarchy",
            serde_json::json!({
                "textDocument": { "uri": main.as_str() },
                "position": position_of(COLD_MAIN, "Megaphone"),
            }),
        );
        assert!(
            !prepared.is_null(),
            "a hierarchy rooted in the bundle prepared as nothing on a cold server"
        );
        assert!(harness.analysis.stage.is_ready(), "it drained");
    }

    #[test]
    fn an_answer_the_workspace_alone_can_give_leaves_the_bundle_where_it_is() {
        // The rung's price, and why it hangs off `answered_nothing`, not the stage alone: a cursor
        // the workspace can answer is as fast as ever, and the bundle stays background work.
        let (mut harness, _main, story) = cold_server_with_a_queued_bundle();

        let (hover, logged) = crate::testing::captured_logs(tracing::Level::DEBUG, || {
            harness.hover_at(&story, COLD_STORY, "Story")
        });
        assert!(!hover.is_null());
        assert!(logged.contains("drained=false"), "{logged}");
        assert!(
            harness.analysis.stage.indexing_bundle(),
            "a request the workspace answered indexed the bundle anyway"
        );
    }
}
