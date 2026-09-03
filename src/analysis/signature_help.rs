//! `textDocument/signatureHelp` — the parameters of the call the cursor is inside.
//!
//! Nothing here is resolved: `locator::precise_call` has already found the method and
//! `render::signature_label` has already spelled it. What this module decides is the two
//! numbers LSP asks for on top of that — which overload the call fits, and which parameter the
//! cursor is writing — and it is only ever asked when the callee resolved *exactly*. A
//! name-based guess would put another class's parameters under the cursor while the user types
//! into them, which is the same wrong answer keyword-argument completion refuses for the same
//! reason: it would be syntactically valid.

use lsp_types::{
    Documentation, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, SignatureHelp,
    SignatureInformation,
};
use rubydex::model::{
    definitions::{Definition, Parameter},
    graph::Graph,
    ids::DeclarationId,
};

use super::{cursor::Active, locator, render};

/// The signature card for a resolved call, or `None` when the declaration is not a method
/// anybody wrote parameters for.
///
/// The declaration is the one the *call* resolves to, which for `Foo.new(` is `Foo#initialize`
/// — the redirect `locator` makes, and the only reading under which the parameters shown are
/// the ones the call takes. The label says `Foo#initialize` rather than `Foo.new` for the same
/// reason hover does: what the user is looking at is the method they are passing arguments to.
#[must_use]
pub fn help(
    graph: &Graph,
    declaration_id: DeclarationId,
    active: &Active,
) -> Option<SignatureHelp> {
    let declaration = graph.declarations().get(&declaration_id)?;
    let definitions = locator::definitions_of(graph, declaration_id);
    let method = definitions.iter().find_map(|definition| match definition {
        Definition::Method(method) => Some(method),
        _ => None,
    })?;

    let name = render::qualified_name(declaration.name());
    // The same rule hover uses: the first definition that has anything to say. A reopened class
    // documents a method in one of its parts and not in all of them.
    let documentation = definitions
        .iter()
        .find_map(|definition| render::documentation(definition.comments()))
        .map(|value| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            })
        });

    let overloads = method.signatures().as_slice();
    let signatures: Vec<SignatureInformation> = overloads
        .iter()
        .map(|signature| {
            let rendered = render::signature_label(graph, &name, signature);
            SignatureInformation {
                label: rendered.label,
                documentation: documentation.clone(),
                parameters: Some(
                    rendered
                        .parameters
                        .into_iter()
                        .map(|(start, end)| ParameterInformation {
                            label: ParameterLabel::LabelOffsets([start, end]),
                            documentation: None,
                        })
                        .collect(),
                ),
                active_parameter: active_parameter(graph, signature, active),
            }
        })
        .collect();

    // RBS is the only source of more than one, and it declares them so that one of them fits:
    // `String#gsub` has three, and which is being written is exactly what the argument count
    // says. Flattening them into one would be a choice to know less than the signatures do.
    let chosen = overloads
        .iter()
        .position(|signature| accepts(graph, signature, active))
        .unwrap_or(0);

    Some(SignatureHelp {
        // `SignatureInformation::activeParameter` is 3.16 and overrides this one per signature;
        // a client older than that reads only this, so the chosen overload's answer is repeated
        // here rather than left for it to infer.
        active_parameter: signatures
            .get(chosen)
            .and_then(|signature| signature.active_parameter),
        active_signature: u32::try_from(chosen).ok(),
        signatures,
    })
}

/// Whether a signature has the parameter the cursor is writing.
///
/// The question an overload set is picked by, and nothing else: a call with three arguments is
/// not being written against the two-argument arm.
fn accepts(graph: &Graph, signature: &[Parameter], active: &Active) -> bool {
    match active {
        Active::Nth(nth) => {
            (*nth as usize) < signature.len()
                || signature.iter().any(|parameter| {
                    matches!(
                        parameter,
                        Parameter::RestPositional(_) | Parameter::Forward(_)
                    )
                })
        }
        // An arm that *declares* this keyword, or one with a `**opts` for it to fall into. An
        // arm that merely takes some other keyword does not: `(a:, b:)` is not the arm being
        // written when the cursor is inside `c:`.
        Active::Keyword(name) => {
            keyword(graph, signature, name).is_some() || rest_keyword(signature).is_some()
        }
        Active::AnyKeyword => takes_keywords(signature),
    }
}

/// Whether a signature has anywhere for a keyword argument to go.
fn takes_keywords(signature: &[Parameter]) -> bool {
    first_keyword(signature).is_some() || rest_keyword(signature).is_some()
}

/// Which parameter of this signature to highlight, as an index into it.
///
/// The ceiling is not decoration, and it is not the end of the list either. **A `*rest` absorbs
/// every positional argument after it**, so the fourth argument to
/// `def new(name, age = 18, *nicknames)` is still `*nicknames` rather than something past the
/// end; counting straight through would walk one parameter further along for every argument
/// typed. Where there is no splat the last parameter is the ceiling instead, because LSP 3.17
/// has no way to say that *no* parameter is active — an index outside the list, and an omitted
/// one, both mean zero — so an answer that has run off the end has to land on the nearest
/// parameter that is still true rather than fall back to pointing at the first one.
///
/// A `Post` parameter — the `c` in `def f(a, *b, c)` — is the one case the ceiling is wrong
/// about, and it is unknowable rather than unhandled: which argument fills it depends on how
/// many there turn out to be, which is not decided while the call is still being written.
fn active_parameter(graph: &Graph, signature: &[Parameter], active: &Active) -> Option<u32> {
    let index = match active {
        Active::Nth(nth) => {
            let last = signature.len().checked_sub(1)?;
            (*nth as usize).min(rest_positional(signature).unwrap_or(last))
        }
        // A keyword is found by name, because keywords are written in any order. One the
        // method does not declare is what `**opts` is for, and belongs there.
        Active::Keyword(name) => keyword(graph, signature, name)
            .or_else(|| rest_keyword(signature))
            .or_else(|| first_keyword(signature))?,
        // Nothing to look up yet, so the answer is the region rather than the parameter: the
        // first keyword the method declares, and `**opts` only if it declares none.
        Active::AnyKeyword => first_keyword(signature).or_else(|| rest_keyword(signature))?,
    };
    u32::try_from(index).ok()
}

/// The keyword parameter written under `name`, positionally.
fn keyword(graph: &Graph, signature: &[Parameter], name: &str) -> Option<usize> {
    signature.iter().position(|parameter| {
        matches!(
            parameter,
            Parameter::RequiredKeyword(_) | Parameter::OptionalKeyword(_)
        ) && spelled(graph, parameter) == Some(name)
    })
}

fn first_keyword(signature: &[Parameter]) -> Option<usize> {
    signature.iter().position(|parameter| {
        matches!(
            parameter,
            Parameter::RequiredKeyword(_) | Parameter::OptionalKeyword(_)
        )
    })
}

fn rest_positional(signature: &[Parameter]) -> Option<usize> {
    signature.iter().position(|parameter| {
        matches!(
            parameter,
            Parameter::RestPositional(_) | Parameter::Forward(_)
        )
    })
}

fn rest_keyword(signature: &[Parameter]) -> Option<usize> {
    signature
        .iter()
        .position(|parameter| matches!(parameter, Parameter::RestKeyword(_)))
}

/// A parameter's name as rubydex interned it. `None` where the string has gone, which is the
/// same "nothing to match" a name nobody wrote would be.
fn spelled<'g>(graph: &'g Graph, parameter: &Parameter) -> Option<&'g str> {
    Some(graph.strings().get(parameter.inner().str())?.as_str())
}
