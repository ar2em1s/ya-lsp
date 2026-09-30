//! Which templates a text renders: the calls that hand a template its instance variables.
//!
//! Rails renders `app/views/stories/show.html.erb` from `StoriesController#show` by convention
//! ([`super::controller_of`]), but any class can render it by name, and the template then reads
//! *that* object's variables. A partial is rendered from views, and reads the variables of
//! whichever controller rendered the view. So `analysis::types` asks, for a template, every call
//! that can name it. This module reads one text's calls; which object each runs on is the caller's
//! question.
//!
//! # What a call names
//!
//! A template is named as `directory/name`, extensions left off, as [`super::template_of`] spells
//! a path.
//!
//! - **In a controller or any other class:** `render "stories/show"` and `template:` name a
//!   template; `partial:` names a partial; a name without a `/`, a symbol and `action:` name the
//!   class's own directory, which the convention answers, and are not reported.
//! - **In a view or a helper** (`view`): a positional string names a partial, and only `template:`
//!   names a template.
//! - **An interpolated name** (`render "pages/#{page}"`): every name its literal parts allow.
//! - **A partial named without a `/`** is relative to a directory this module cannot see: any
//!   partial.
//! - **Anything else** (`render options`, `render(**args)`, `render @record`): any template or
//!   partial.
//! - **`mail(template_path: "x", template_name: "y")`**: a mailer rendering `x/y`.
//!
//! A call with a receiver written (`ApplicationController.render(…)`) renders with variables no
//! class here writes (`assigns:`), and is reported as such, but only in the shapes ActionView's
//! own API takes: a path string, or `template:`, `partial:`, `file:`, `assigns:` or `locals:`. Any
//! other `x.render(y)` is another library's (`Liquid::Template#render`, a Markdown renderer).
//!
//! In a view or helper, `render x` with an object renders the object's partial
//! (`to_partial_path`), never a template.
//!
//! # What a call hands the partial (backlog 56)
//!
//! Each call also says which locals it passes ([`Locals`]): the keys of `render "x", k: v` and of
//! `locals: { k: v }`, the object of `object:` and the elements of `collection:` under `as:` or
//! the partial's own name ([`Name::Own`]), with `_counter` and `_iteration` beside a collection,
//! and the object `render @post` renders. A partial named without a `/` is looked up in a
//! directory only running Ruby knows, so it is any partial of that name.

use ruby_prism::{CallNode, Node, Visit};

use super::inflect::{pluralize, underscore};
use super::syntax::{keyword, symbol_or_string};

/// Every call name [`read_renders`] reads.
///
/// A text writing none of these renders nothing, so a caller asking rubydex's call index for these
/// names finds every document that can render a template, without reading one.
pub const RENDER_CALLS: [&str; 5] = ["render", "render_to_string", "mail", "partial!", "array!"];

/// The local jbuilder's template handler defines, and the class it holds: `json`, a
/// `JbuilderTemplate` (backlog 57). Its `partial!`, `array!` and any key written with `partial:`
/// render a jbuilder partial.
pub const JBUILDER: (&str, &str) = ("json", "JbuilderTemplate");

/// One call that can render a template or a partial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Render {
    /// Where the call's name is written: the graph places the class it is written in.
    pub at: u32,
    /// Which names it can render.
    pub target: Target,
    /// Whether those are templates, partials, or either.
    pub kind: Kind,
    /// A receiver is written: the call renders on another object, with variables no class here
    /// writes.
    pub elsewhere: bool,
    /// What it hands the partial it renders as locals (backlog 56).
    pub locals: Locals,
    /// Written on jbuilder's `json` (backlog 57): a JSON lookup, which finds jbuilder partials
    /// alone.
    pub json: bool,
}

/// The locals a render call hands a partial (backlog 56).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Locals {
    /// Each local the call names, with what it holds. Empty for a call that passes none.
    Named(Vec<Local>),
    /// Locals no reading of the text can list: `locals: options`, `**opts`, a key that is no
    /// symbol, an `as:` that is no literal.
    Unread,
}

/// One local a render call passes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    pub name: Name,
    pub value: Value,
}

/// A local's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Name {
    /// Written in the call: a `locals:` key, or `as:` with its suffix.
    Written(String),
    /// The name of the partial rendered (`posts/_post` hands `post`), which only the partial says,
    /// with this suffix: `""`, `"_counter"` or `"_iteration"`.
    Own(&'static str),
}

impl Name {
    /// The name as the partial `logical` (`posts/post`) reads it.
    #[must_use]
    pub fn in_partial(&self, logical: &str) -> String {
        match self {
            Self::Written(name) => name.clone(),
            Self::Own(suffix) => {
                let own = logical.rsplit('/').next().unwrap_or(logical);
                format!("{own}{suffix}")
            }
        }
    }
}

/// What a local holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value {
    /// The expression written at this span.
    Written((u32, u32)),
    /// One element of the collection written at this span (`collection:`).
    Element((u32, u32)),
    /// The object written at this span, which also names the partial (`to_partial_path`): the
    /// object itself, or one element where it is a collection (`render @posts`).
    Object((u32, u32)),
    /// `_counter` beside a collection: an `Integer`.
    Counter,
    /// `_iteration` beside a collection: an `ActionView::PartialIteration`.
    Iteration,
    /// The expression written at this span, or one element where it is a collection (it answers
    /// `to_ary`): what jbuilder hands a partial under `as:` (backlog 57).
    Either((u32, u32)),
}

/// What a render call's name is the name of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Template,
    Partial,
    /// A target this cannot read may be either.
    Either,
}

/// The names one call can render, as a `directory/name` pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Every name that starts with `head` and ends with `tail`. A literal name is all head.
    Like {
        head: String,
        tail: String,
        exact: bool,
    },
    /// Any name: it is not written where this can read it.
    Anything,
}

impl Render {
    /// Whether the call writes the name it renders, whole or as an interpolation's literal parts,
    /// rather than one only running Ruby knows (`render options`, `render template: page`).
    ///
    /// A layout is chosen by the layout lookup, not named by a render, so it counts only a call
    /// that writes its name: a nested layout's `render template: "layouts/application"`.
    #[must_use]
    pub fn writes_the_name(&self) -> bool {
        !matches!(self.target, Target::Anything)
    }

    /// Whether this call can render the template, or the partial where `partial` is set, that
    /// [`super::template_of`] spells `logical`.
    #[must_use]
    pub fn names(&self, logical: &str, partial: bool) -> bool {
        let kind = match self.kind {
            Kind::Template => !partial,
            Kind::Partial => partial,
            Kind::Either => true,
        };
        kind && self.target.names(logical)
    }
}

impl Target {
    /// Whether `logical` (`stories/show`) can be the name this call renders.
    #[must_use]
    pub fn names(&self, logical: &str) -> bool {
        match self {
            Self::Like { head, tail, exact } => {
                if *exact {
                    return head == logical;
                }
                logical.len() >= head.len() + tail.len()
                    && logical.starts_with(head.as_str())
                    && logical.ends_with(tail.as_str())
            }
            Self::Anything => true,
        }
    }
}

/// Every call in `source` that can render a template or partial, read as a view or helper reads
/// it where `view` is set.
#[must_use]
pub fn read_renders(source: &str, view: bool) -> Vec<Render> {
    let result = ruby_prism::parse(source.as_bytes());
    let mut walk = Walk {
        source,
        view,
        found: Vec::new(),
    };
    walk.visit(&result.node());
    walk.found
}

struct Walk<'s> {
    source: &'s str,
    view: bool,
    found: Vec<Render>,
}

impl<'pr> Visit<'pr> for Walk<'_> {
    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        let name = node.name();
        let elsewhere = node
            .receiver()
            .is_some_and(|receiver| !matches!(receiver, Node::SelfNode { .. }));
        let json = on_json(node);
        let named = match name.as_slice() {
            _ if json => jbuildered(self.source, node)
                .map(|(target, locals)| (target, Kind::Partial, locals)),
            b"render" | b"render_to_string" if !elsewhere || action_view_shaped(node) => {
                rendered(self.source, node, self.view)
            }
            b"mail" => {
                mailed(node).map(|target| (target, Kind::Template, Locals::Named(Vec::new())))
            }
            _ => None,
        };
        if let Some((target, kind, locals)) = named {
            self.found.push(Render {
                at: node
                    .message_loc()
                    .map_or(node.location().start_offset(), |at| at.start_offset())
                    as u32,
                target,
                kind,
                elsewhere: elsewhere && !json,
                locals,
                json,
            });
        }
        ruby_prism::visit_call_node(self, node);
    }
}

/// What a `render` names, and the locals it passes, or `None` for the class's own template or a
/// response.
fn rendered(source: &str, node: &CallNode<'_>, view: bool) -> Option<(Target, Kind, Locals)> {
    let written: Vec<Node<'_>> = node
        .arguments()
        .map(|arguments| arguments.arguments().iter().collect())
        .unwrap_or_default();
    // `render` alone: the action's own template.
    let first = written.first()?;
    if let Some(hash) = first.as_keyword_hash_node() {
        if let Some(template) = keyword(node, "template").or_else(|| keyword(node, "file")) {
            return Some((
                named(&template, true),
                Kind::Template,
                passed(source, keyword(node, "locals").as_ref()),
            ));
        }
        if let Some(partial) = keyword(node, "partial") {
            let literal = partial.as_string_node().is_some()
                || partial.as_interpolated_string_node().is_some()
                || partial.as_symbol_node().is_some();
            // `partial: @post` is the object's own partial, and the object is its local. Beside
            // `object:` or `collection:`, it can only be the partial's name.
            let alone = keyword(node, "object").is_none() && keyword(node, "collection").is_none();
            let object = (!literal && alone).then(|| span_of(&partial));
            return Some((
                partial_named(&partial),
                Kind::Partial,
                handed(source, node, object),
            ));
        }
        // `render collection: @posts`: each element's own partial.
        if let Some(collection) = keyword(node, "collection") {
            return Some((
                Target::Anything,
                Kind::Partial,
                handed(source, node, Some(span_of(&collection))),
            ));
        }
        // `render(**options)`, or a key written as a string: keys nobody can read.
        let unreadable = hash.elements().iter().any(|element| {
            element.as_assoc_node().is_none_or(|assoc| {
                let key = assoc.key();
                key.as_symbol_node()
                    .and_then(|symbol| symbol.value_loc())
                    .and_then(|at| source.get(at.start_offset()..at.end_offset()))
                    .is_none()
            })
        });
        // Otherwise a response, an `action:` of the class's own, or only options.
        return unreadable.then_some((Target::Anything, Kind::Either, Locals::Unread));
    }
    if first.as_symbol_node().is_some() {
        return None;
    }
    // `render "x", k: v` and `render @post, k: v` pass the second argument's keys.
    let locals = match written.get(1) {
        None => Locals::Named(Vec::new()),
        Some(second) => passed(source, Some(second)),
    };
    let readable =
        first.as_string_node().is_some() || first.as_interpolated_string_node().is_some();
    if !readable {
        // An object in a view renders its partial; in a class it may be a name or options too.
        let kind = if view { Kind::Partial } else { Kind::Either };
        let locals = match locals {
            Locals::Named(mut named) => {
                named.push(Local {
                    name: Name::Own(""),
                    value: Value::Object(span_of(first)),
                });
                Locals::Named(named)
            }
            Locals::Unread => Locals::Unread,
        };
        return Some((Target::Anything, kind, locals));
    }
    if view {
        return Some((partial_named(first), Kind::Partial, locals));
    }
    let target = named(first, false);
    // A literal with no `/` is an action of the class's own directory.
    match &target {
        Target::Like {
            head, exact: true, ..
        } if !head.contains('/') => None,
        _ => Some((target, Kind::Template, locals)),
    }
}

/// A node's span.
fn span_of(node: &Node<'_>) -> (u32, u32) {
    let location = node.location();
    (location.start_offset() as u32, location.end_offset() as u32)
}

/// The locals a hash written as `render`'s second argument or `locals:` passes: each symbol key
/// and its value. Anything else, or a hash with a `**` or a key that is no symbol, is
/// [`Locals::Unread`].
fn passed(source: &str, hash: Option<&Node<'_>>) -> Locals {
    let Some(hash) = hash else {
        return Locals::Named(Vec::new());
    };
    let elements: Vec<Node<'_>> = if let Some(found) = hash.as_keyword_hash_node() {
        found.elements().iter().collect()
    } else if let Some(found) = hash.as_hash_node() {
        found.elements().iter().collect()
    } else {
        return Locals::Unread;
    };
    let mut named = Vec::new();
    for element in &elements {
        let Some(assoc) = element.as_assoc_node() else {
            return Locals::Unread;
        };
        let key = assoc.key();
        let Some(name) = key
            .as_symbol_node()
            .and_then(|symbol| symbol.value_loc())
            .and_then(|at| source.get(at.start_offset()..at.end_offset()))
        else {
            return Locals::Unread;
        };
        // `{ story: }` writes the value as the key: the local or method of that name.
        let value = assoc.value();
        let value = match value.as_implicit_node() {
            Some(implicit) => implicit.value(),
            None => value,
        };
        named.push(Local {
            name: Name::Written(name.to_owned()),
            value: Value::Written(span_of(&value)),
        });
    }
    Locals::Named(named)
}

/// The locals a `render` written with options hands: `locals:`, and `object:` and `collection:`
/// under `as:` or the partial's own name. `object` is the object the call renders by itself
/// (`partial: @post`, `collection:` alone), whose partial its type names.
fn handed(source: &str, node: &CallNode<'_>, object: Option<(u32, u32)>) -> Locals {
    let Locals::Named(mut named) = passed(source, keyword(node, "locals").as_ref()) else {
        return Locals::Unread;
    };
    let alias = match keyword(node, "as") {
        None => None,
        Some(written) => match symbol_or_string(source, &written) {
            Some((alias, _)) => Some(alias),
            None => return Locals::Unread,
        },
    };
    let name = |suffix: &'static str| match &alias {
        Some(alias) => Name::Written(format!("{alias}{suffix}")),
        None => Name::Own(suffix),
    };
    if let Some(object) = object {
        named.push(Local {
            name: name(""),
            value: Value::Object(object),
        });
    } else if let Some(written) = keyword(node, "object") {
        named.push(Local {
            name: name(""),
            value: Value::Written(span_of(&written)),
        });
    }
    if object.is_none()
        && let Some(collection) = keyword(node, "collection")
    {
        named.extend([
            Local {
                name: name(""),
                value: Value::Element(span_of(&collection)),
            },
            Local {
                name: name("_counter"),
                value: Value::Counter,
            },
            Local {
                name: name("_iteration"),
                value: Value::Iteration,
            },
        ]);
    }
    Locals::Named(named)
}

/// Whether a call is written on jbuilder's `json` ([`JBUILDER`]): the local its template handler
/// defines, which the indexed Ruby reads as a call.
fn on_json(node: &CallNode<'_>) -> bool {
    node.receiver().is_some_and(|receiver| {
        let spelled = match (
            receiver.as_local_variable_read_node(),
            receiver.as_call_node(),
        ) {
            (Some(read), _) => read.name().as_slice().to_vec(),
            (None, Some(call))
                if call.receiver().is_none()
                    && call.arguments().is_none()
                    && call.block().is_none() =>
            {
                call.name().as_slice().to_vec()
            }
            _ => return false,
        };
        spelled == JBUILDER.0.as_bytes()
    })
}

/// What a call on `json` renders and the locals it passes (backlog 57), as `JbuilderTemplate`
/// does:
///
/// - `json.partial! "x", k: v`: every option but `partial:`, `as:`, `collection:` and `cached:`
///   is a local, unless it writes `locals:`; `json.partial! partial: "x", …` the same;
/// - `json.partial! @post`: the object's own partial, with it under the partial's name;
/// - `json.array! xs, partial: "x", as: :k` and `json.key xs, partial: "x", as: :k`: each element,
///   or the object, under `as:` (the partial's own name without it).
///
/// `None` for any other call on `json`: a key it sets.
fn jbuildered(source: &str, node: &CallNode<'_>) -> Option<(Target, Locals)> {
    let written: Vec<Node<'_>> = node
        .arguments()
        .map(|arguments| arguments.arguments().iter().collect())
        .unwrap_or_default();
    let first = written.first()?;
    if node.name().as_slice() == b"partial!" {
        if first.as_keyword_hash_node().is_some() {
            let partial = keyword(node, "partial")?;
            return Some((partial_named(&partial), optioned(source, node, None)));
        }
        if first.as_string_node().is_none() && first.as_interpolated_string_node().is_none() {
            return Some((
                Target::Anything,
                Locals::Named(vec![Local {
                    name: Name::Own(""),
                    value: Value::Object(span_of(first)),
                }]),
            ));
        }
        return Some((partial_named(first), optioned(source, node, None)));
    }
    let partial = keyword(node, "partial")?;
    let each = (first.as_keyword_hash_node().is_none()).then(|| span_of(first));
    Some((partial_named(&partial), optioned(source, node, each)))
}

/// The options jbuilder reads rather than passes ([`jbuildered`]).
const JBUILDER_OPTIONS: [&str; 4] = ["partial", "as", "collection", "cached"];

/// The locals a jbuilder call's options pass, with `collection:` (or `each`, the first argument of
/// `array!` and a key) under `as:`.
fn optioned(source: &str, node: &CallNode<'_>, each: Option<(u32, u32)>) -> Locals {
    let options = node.arguments().and_then(|arguments| {
        arguments
            .arguments()
            .iter()
            .find(|argument| argument.as_keyword_hash_node().is_some())
    });
    let mut named = match keyword(node, "locals") {
        Some(locals) => passed(source, Some(&locals)),
        None => passed(source, options.as_ref()),
    };
    if let Locals::Named(named) = &mut named {
        named.retain(|local| {
            !matches!(&local.name, Name::Written(name) if JBUILDER_OPTIONS.contains(&name.as_str()))
        });
    }
    let Locals::Named(mut named) = named else {
        return Locals::Unread;
    };
    let alias = match keyword(node, "as") {
        None => None,
        Some(written) => match symbol_or_string(source, &written) {
            Some((alias, _)) => Some(alias),
            None => return Locals::Unread,
        },
    };
    if let Some(span) = keyword(node, "collection")
        .map(|collection| span_of(&collection))
        .or(each)
    {
        named.push(Local {
            name: alias.map_or(Name::Own(""), Name::Written),
            value: Value::Either(span),
        });
    }
    Locals::Named(named)
}

/// Whether a call with a receiver takes one of the shapes ActionView's `render` does: a path
/// string, or one of its keywords.
fn action_view_shaped(node: &CallNode<'_>) -> bool {
    let Some(first) = node
        .arguments()
        .and_then(|arguments| arguments.arguments().iter().next())
    else {
        return false;
    };
    first.as_string_node().is_some()
        || first.as_interpolated_string_node().is_some()
        || ["template", "partial", "file", "assigns", "locals"]
            .iter()
            .any(|key| keyword(node, key).is_some())
}

/// A partial's name: relative to a directory this cannot see unless it holds a `/`, so any
/// partial of that name (`row` is `*/row`), or of that ending where the name is built and its
/// written head holds no directory.
fn partial_named(node: &Node<'_>) -> Target {
    match named(node, true) {
        Target::Like {
            head, exact: true, ..
        } if !head.contains('/') => Target::Like {
            head: String::new(),
            tail: format!("/{head}"),
            exact: false,
        },
        Target::Like { head, .. } if !head.is_empty() && !head.contains('/') => Target::Anything,
        target => target,
    }
}

/// What a `mail` names: `template_path:` with the action or `template_name:` in it, or nothing but
/// the mailer's own directory.
fn mailed(node: &CallNode<'_>) -> Option<Target> {
    let path = keyword(node, "template_path")?;
    let Some(name) = keyword(node, "template_name") else {
        // The action's own name, in that directory.
        return Some(match named(&path, true) {
            Target::Like {
                head, exact: true, ..
            } => Target::Like {
                head: format!("{}/", head.trim_end_matches('/')),
                tail: String::new(),
                exact: false,
            },
            _ => Target::Anything,
        });
    };
    match (named(&path, true), named(&name, true)) {
        (
            Target::Like {
                head: path,
                exact: true,
                ..
            },
            Target::Like {
                head: name,
                exact: true,
                ..
            },
        ) => Some(Target::Like {
            head: format!("{}/{name}", path.trim_end_matches('/')),
            tail: String::new(),
            exact: true,
        }),
        _ => Some(Target::Anything),
    }
}

/// The templates a written name can be: a literal, an interpolation's literal parts, or anything.
/// `symbols` also reads a symbol, which `template:` takes and a positional argument does not mean.
fn named(node: &Node<'_>, symbols: bool) -> Target {
    let text = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .trim_start_matches('/')
            .to_owned()
    };
    if let Some(string) = node.as_string_node() {
        return exactly(text(string.unescaped()));
    }
    if symbols && let Some(symbol) = node.as_symbol_node() {
        return exactly(text(symbol.unescaped()));
    }
    let Some(string) = node.as_interpolated_string_node() else {
        // `A_symbol` positional, a variable, a call, a hash: nothing this can read.
        return Target::Anything;
    };
    let parts: Vec<Node<'_>> = string.parts().iter().collect();
    let literal = |part: &Node<'_>| {
        part.as_string_node()
            .map(|found| String::from_utf8_lossy(found.unescaped()).into_owned())
    };
    let head: String = parts.iter().map_while(literal).collect();
    let tail: Vec<String> = parts.iter().rev().map_while(literal).collect();
    Target::Like {
        head: head.trim_start_matches('/').to_owned(),
        tail: tail.into_iter().rev().collect(),
        exact: false,
    }
}

fn exactly(name: String) -> Target {
    Target::Like {
        head: name,
        tail: String::new(),
        exact: true,
    }
}

/// A partial's strict-locals comment (Rails 7.1): `<%# locals: (post:, compact: false) -%>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrictLocals {
    /// Each local it declares, with its default.
    pub declared: Vec<(String, Default)>,
    /// A `**rest` takes any other local too.
    pub open: bool,
}

/// A strict local's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Default {
    /// None: every render must pass it.
    Required,
    /// A literal, by the class of its value.
    Literal(&'static str),
    /// An expression only running Ruby can value.
    Unread,
}

/// The strict-locals comment in a partial's markup, where it has one (Rails 7.1).
///
/// `ActionView::Template` reads the first match of `/\#\s+locals:\s+\((.*)\)/` in the source,
/// and the parentheses are a method's keyword parameters. `None` where there is no such comment,
/// or its parameters are not keywords only (Rails raises), so the locals are what the renders pass.
#[must_use]
pub fn strict_locals(markup: &str) -> Option<StrictLocals> {
    let (_, after) = markup.split_once("#")?;
    let mut rest = after;
    let parameters = loop {
        let spaced = rest.trim_start_matches([' ', '\t']);
        if spaced.len() < rest.len()
            && let Some(inside) = spaced.strip_prefix("locals:")
        {
            let inside = inside.trim_start_matches([' ', '\t']);
            if inside.len() < spaced.len() - "locals:".len()
                && let Some(inside) = inside.strip_prefix('(')
            {
                let line = inside.split('\n').next().unwrap_or_default();
                break line.get(..line.rfind(')')?)?;
            }
        }
        rest = rest.split_once('#')?.1;
    };
    let source = format!("def locals({parameters}); end");
    let result = ruby_prism::parse(source.as_bytes());
    let statements = result.node().as_program_node()?.statements();
    let def = statements.body().iter().next()?.as_def_node()?;
    let Some(written) = def.parameters() else {
        return Some(StrictLocals {
            declared: Vec::new(),
            open: false,
        });
    };
    // A trailing positional follows a rest or an optional one, so these three cover every
    // positional.
    if written.requireds().iter().next().is_some()
        || written.optionals().iter().next().is_some()
        || written.rest().is_some()
    {
        return None;
    }
    let mut declared = Vec::new();
    for keyword in written.keywords().iter() {
        declared.push(match keyword.as_optional_keyword_parameter_node() {
            Some(optional) => (
                String::from_utf8_lossy(optional.name().as_slice()).into_owned(),
                literal_class(&optional.value()),
            ),
            // The other kind a keyword list holds, written `name:`.
            None => {
                let location = keyword.location();
                let written = &source.as_bytes()[location.start_offset()..location.end_offset()];
                (
                    String::from_utf8_lossy(written)
                        .trim_end_matches(':')
                        .to_owned(),
                    Default::Required,
                )
            }
        });
    }
    Some(StrictLocals {
        declared,
        open: written.keyword_rest().is_some(),
    })
}

/// The class of a literal default, or [`Default::Unread`].
fn literal_class(value: &Node<'_>) -> Default {
    Default::Literal(match value {
        Node::NilNode { .. } => "NilClass",
        Node::TrueNode { .. } => "TrueClass",
        Node::FalseNode { .. } => "FalseClass",
        Node::IntegerNode { .. } => "Integer",
        Node::FloatNode { .. } => "Float",
        Node::StringNode { .. } | Node::InterpolatedStringNode { .. } => "String",
        Node::SymbolNode { .. } => "Symbol",
        Node::ArrayNode { .. } => "Array",
        Node::HashNode { .. } => "Hash",
        _ => return Default::Unread,
    })
}

/// The partial a record of `class` renders as (`to_partial_path`, backlog 56): `Admin::Post` is
/// `admin/posts/post`, `ActiveModel::Name`'s collection and element. `None` where the name does
/// not inflect.
#[must_use]
pub fn partial_of(class: &str) -> Option<String> {
    let segments: Vec<String> = class
        .split("::")
        .map(underscore)
        .collect::<Option<Vec<_>>>()?;
    let (element, namespace) = segments.split_last()?;
    let mut logical = namespace.join("/");
    if !logical.is_empty() {
        logical.push('/');
    }
    logical.push_str(&pluralize(element));
    logical.push('/');
    logical.push_str(element);
    Some(logical)
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    fn exactly(name: &str) -> Target {
        Target::Like {
            head: name.to_owned(),
            tail: String::new(),
            exact: true,
        }
    }

    fn like(head: &str, tail: &str) -> Target {
        Target::Like {
            head: head.to_owned(),
            tail: tail.to_owned(),
            exact: false,
        }
    }

    /// Each call in `source`, as the target it names, its kind and whether a receiver was written.
    fn targets(source: &str, view: bool) -> Vec<(Target, Kind, bool)> {
        read_renders(source, view)
            .into_iter()
            .map(|render| (render.target, render.kind, render.elsewhere))
            .collect()
    }

    /// [`RENDER_CALLS`] is the reader's own list: each name in it is read, and a name outside it
    /// is not, so a caller that finds documents through the list misses none the reader would read.
    #[test]
    fn the_call_names_are_the_ones_the_reader_reads() {
        for (name, source, partial) in RENDER_CALLS
            .iter()
            .zip([
                ("render \"stories/show\"", false),
                ("render_to_string \"stories/show\"", false),
                (
                    "mail(template_path: \"stories\", template_name: \"show\")",
                    false,
                ),
                ("json.partial! \"stories/show\"", true),
                ("json.array! list, partial: \"stories/show\"", true),
            ])
            .map(|(name, (source, partial))| (name, source, partial))
        {
            let found = read_renders(source, false);
            assert_eq!(found.len(), 1, "{name}: {found:?}");
            assert!(found[0].names("stories/show", partial), "{name}");
            assert!(
                !found[0].names("stories/show", !partial),
                "{name} names one kind"
            );
            assert_eq!(found[0].json, partial, "{name}");
        }
        assert!(read_renders("render_later \"stories/show\"", false).is_empty());
    }

    #[test]
    fn a_controller_names_a_template_by_its_directory_and_name() {
        let source = "\
class FeedsController
  def show(page, options, args)
    render
    render :edit
    render \"edit\"
    render json: {}
    render status: 404
    render partial: \"stories/row\"
    render partial: \"row\"
    render \"stories/show\"
    render \"/stories/show\", status: 422
    render template: \"stories/index\"
    render template: :home
    render file: path
    render \"pages/#{page}\"
    render_to_string(\"stories/#{page}_card\")
    render options
    render(**args)
    render(\"template\" => \"x\")
    render @story
    self.render \"stories/show\"
    ApplicationController.render(template: \"stories/show\", assigns: {})
  end
end
";
        use Kind::{Either, Partial, Template};
        assert_eq!(
            targets(source, false),
            vec![
                (exactly("stories/row"), Partial, false),
                (like("", "/row"), Partial, false),
                (exactly("stories/show"), Template, false),
                (exactly("stories/show"), Template, false),
                (exactly("stories/index"), Template, false),
                (exactly("home"), Template, false),
                (Target::Anything, Template, false),
                (like("pages/", ""), Template, false),
                (like("stories/", "_card"), Template, false),
                (Target::Anything, Either, false),
                (Target::Anything, Either, false),
                (Target::Anything, Either, false),
                (Target::Anything, Either, false),
                (exactly("stories/show"), Template, false),
                (exactly("stories/show"), Template, true),
            ]
        );
        assert_eq!(
            read_renders("render \"stories/show\"\n", false)[0].at,
            0,
            "the call's name, where the graph places its class"
        );
    }

    #[test]
    fn a_view_names_a_partial_unless_it_says_template() {
        let source = "\
render \"stories/row\"
render \"row\"
render \"stories/#{kind}_row\"
render template: \"stories/show\"
render @stories
render :row
";
        use Kind::{Partial, Template};
        // An object renders its own partial (`to_partial_path`).
        assert_eq!(
            targets(source, true),
            vec![
                (exactly("stories/row"), Partial, false),
                (like("", "/row"), Partial, false),
                (like("stories/", "_row"), Partial, false),
                (exactly("stories/show"), Template, false),
                (Target::Anything, Partial, false),
            ]
        );
    }

    #[test]
    fn a_render_on_another_object_counts_only_in_the_shapes_action_view_takes() {
        let source = "\
Liquid::Template.parse(body).render(drops)
Katex.render(formula)
renderer.render
ApplicationController.render(\"stories/show\")
ApplicationController.render(\"stories/#{kind}\")
controller.render_to_string(\"stories/card\")
engine.render_to_string(body)
renderer.render(partial: \"stories/row\", locals: {})
ApplicationController.render(assigns: {story: s})
";
        use Kind::{Partial, Template};
        // `assigns:` with nothing to render names no template.
        assert_eq!(
            targets(source, false),
            vec![
                (exactly("stories/show"), Template, true),
                (like("stories/", ""), Template, true),
                (exactly("stories/card"), Template, true),
                (exactly("stories/row"), Partial, true),
            ]
        );
    }

    #[test]
    fn a_mailer_names_a_template_by_its_path_and_name() {
        let source = "\
class DigestMailer
  def weekly(name, path)
    mail(to: \"a\")
    mail(template_path: \"notifications\", template_name: \"weekly\")
    mail(template_path: \"notifications/\")
    mail(template_path: path)
    mail(template_path: \"notifications\", template_name: name)
    user.mail
  end
end
";
        let template = Kind::Template;
        assert_eq!(
            targets(source, false),
            vec![
                (exactly("notifications/weekly"), template, false),
                (like("notifications/", ""), template, false),
                (Target::Anything, template, false),
                (Target::Anything, template, false),
            ]
        );
    }

    /// Each call's locals, as `(name, value)` with each span replaced by its text.
    fn locals(source: &str, view: bool) -> Vec<Option<Vec<(String, String)>>> {
        let text = |span: (u32, u32)| source[span.0 as usize..span.1 as usize].to_owned();
        read_renders(source, view)
            .into_iter()
            .map(|render| match render.locals {
                Locals::Unread => None,
                Locals::Named(named) => Some(
                    named
                        .into_iter()
                        .map(|local| {
                            let name = match local.name {
                                Name::Written(name) => name,
                                Name::Own(suffix) => format!("<own>{suffix}"),
                            };
                            let value = match local.value {
                                Value::Written(span) => text(span),
                                Value::Element(span) => format!("each {}", text(span)),
                                Value::Object(span) => format!("object {}", text(span)),
                                Value::Counter => "counter".to_owned(),
                                Value::Iteration => "iteration".to_owned(),
                                Value::Either(span) => format!("either {}", text(span)),
                            };
                            (name, value)
                        })
                        .collect(),
                ),
            })
            .collect()
    }

    fn pairs(written: &[(&str, &str)]) -> Option<Vec<(String, String)>> {
        Some(
            written
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        )
    }

    /// Backlog 56: what each shape of call hands a partial as its locals.
    #[test]
    fn a_render_call_says_which_locals_it_passes() {
        let source = "\
render \"stories/row\", story: @story, compact: true
render \"stories/row\", { story: }
render \"stories/row\"
render \"stories/row\", options
render \"stories/row\", **options
render \"stories/row\", \"story\" => 1
render partial: \"stories/row\", locals: { story: s }
render partial: \"stories/row\", locals: options
render partial: \"stories/row\", object: s
render partial: \"stories/row\", object: s, as: :item
render partial: \"stories/row\", collection: list
render partial: \"stories/row\", collection: list, as: \"item\"
render partial: \"stories/row\", collection: list, as: kind
render partial: @story
render partial: \"stories/#{kind}\", locals: { story: s }
render partial: name, collection: list
render collection: list
render @story, compact: true
render @story, options
render template: \"stories/show\", locals: { story: s }
render(**args)
";
        assert_eq!(
            locals(source, true),
            vec![
                pairs(&[("story", "@story"), ("compact", "true")]),
                // The shorthand's implied value spans its key.
                pairs(&[("story", "story:")]),
                pairs(&[]),
                None,
                None,
                None,
                pairs(&[("story", "s")]),
                None,
                pairs(&[("<own>", "s")]),
                pairs(&[("item", "s")]),
                pairs(&[
                    ("<own>", "each list"),
                    ("<own>_counter", "counter"),
                    ("<own>_iteration", "iteration"),
                ]),
                pairs(&[
                    ("item", "each list"),
                    ("item_counter", "counter"),
                    ("item_iteration", "iteration"),
                ]),
                None,
                pairs(&[("<own>", "object @story")]),
                pairs(&[("story", "s")]),
                pairs(&[
                    ("<own>", "each list"),
                    ("<own>_counter", "counter"),
                    ("<own>_iteration", "iteration"),
                ]),
                pairs(&[("<own>", "object list")]),
                pairs(&[("compact", "true"), ("<own>", "object @story")]),
                None,
                pairs(&[("story", "s")]),
                None,
            ]
        );
        // A class's `render "x", …` is a template's, whose locals are read all the same.
        assert_eq!(
            locals("render \"stories/show\", story: s\n", false),
            vec![pairs(&[("story", "s")])]
        );
        // The partial's own name is its last segment.
        assert_eq!(
            Name::Own("_counter").in_partial("stories/row"),
            "row_counter"
        );
        assert_eq!(Name::Own("").in_partial("row"), "row");
        assert_eq!(
            Name::Written("item".to_owned()).in_partial("stories/row"),
            "item"
        );
    }

    /// Backlog 57: jbuilder's calls on `json`, read as `JbuilderTemplate` reads its options.
    #[test]
    fn a_jbuilder_call_says_which_locals_it_passes() {
        let source = "\
json.partial! \"stories/story\", story: @story, cached: true
json.partial! partial: \"stories/story\", locals: { story: s }
json.partial! partial: \"stories/story\", story: s, as: :x
json.partial! @story
json.partial! \"stories/story\", collection: list, as: :story
json.partial! \"stories/story\", collection: list
json.array! list, partial: \"stories/story\", as: :story
json.comments list, partial: \"comments/comment\", as: :comment
json.title \"x\"
json.partial! \"stories/story\", **opts
json.partial! \"stories/story\", as: kind, collection: list
json.array! list
json.partial! partial: name
other.partial! \"stories/story\"
json.partial!
json.partial! \"stories/#{kind}\", story: s
json(1).partial! \"stories/story\"
json { }.partial! \"stories/story\"
json = builder
json.partial! \"stories/story\"
";
        assert_eq!(
            locals(source, true),
            vec![
                pairs(&[("story", "@story")]),
                pairs(&[("story", "s")]),
                pairs(&[("story", "s")]),
                pairs(&[("<own>", "object @story")]),
                pairs(&[("story", "either list")]),
                pairs(&[("<own>", "either list")]),
                pairs(&[("story", "either list")]),
                pairs(&[("comment", "either list")]),
                None,
                None,
                pairs(&[]),
                pairs(&[("story", "s")]),
                pairs(&[]),
            ]
        );
        let found = read_renders(source, true);
        assert!(found.iter().all(|render| render.json && !render.elsewhere));
        assert_eq!(found[0].target, exactly("stories/story"));
        assert_eq!(found[3].target, Target::Anything);
        assert_eq!(found[7].target, exactly("comments/comment"));
        assert_eq!(found[10].target, Target::Anything);
        assert_eq!(found[11].target, like("stories/", ""));
    }

    /// A partial named without a directory is any partial of that name, and a built name without a
    /// directory in its written head is any partial ending as it does.
    #[test]
    fn a_partial_named_without_a_directory_is_any_of_that_name() {
        let [bare, built, headed] = [
            "render \"row\"",
            "render \"#{kind}_row\"",
            "render \"card_#{kind}\"",
        ]
        .map(|source| read_renders(source, true).remove(0).target);
        assert_eq!(bare, like("", "/row"));
        assert!(bare.names("stories/row") && !bare.names("stories/rows"));
        assert_eq!(built, like("", "_row"));
        assert_eq!(headed, Target::Anything);
    }

    /// Rails 7.1's magic comment: keyword parameters only, a literal default by its class.
    #[test]
    fn a_strict_locals_comment_declares_the_locals() {
        assert_eq!(
            strict_locals(
                "<%# locals: (story:, compact: false, n: 1, f: 1.5, s: \"x\", t: \"#{a}\", y: :z, \
                 a: [], h: {}, none: nil, yes: true, other: story.title) -%>\n<%= story %>\n"
            ),
            Some(StrictLocals {
                declared: vec![
                    ("story".to_owned(), Default::Required),
                    ("compact".to_owned(), Default::Literal("FalseClass")),
                    ("n".to_owned(), Default::Literal("Integer")),
                    ("f".to_owned(), Default::Literal("Float")),
                    ("s".to_owned(), Default::Literal("String")),
                    ("t".to_owned(), Default::Literal("String")),
                    ("y".to_owned(), Default::Literal("Symbol")),
                    ("a".to_owned(), Default::Literal("Array")),
                    ("h".to_owned(), Default::Literal("Hash")),
                    ("none".to_owned(), Default::Literal("NilClass")),
                    ("yes".to_owned(), Default::Literal("TrueClass")),
                    ("other".to_owned(), Default::Unread),
                ],
                open: false,
            })
        );
        assert_eq!(
            strict_locals("<%# locals: () -%>\n"),
            Some(StrictLocals {
                declared: Vec::new(),
                open: false,
            })
        );
        assert_eq!(
            strict_locals("<%# locals: (story:, **rest) %>"),
            Some(StrictLocals {
                declared: vec![("story".to_owned(), Default::Required)],
                open: true,
            })
        );
        // Positional parameters are an error to Rails, and say nothing here.
        assert_eq!(strict_locals("<%# locals: (story) %>"), None);
        assert_eq!(strict_locals("<%# locals: (story = 1) %>"), None);
        assert_eq!(strict_locals("<%# locals: (*all) %>"), None);
        assert_eq!(strict_locals("<%# locals: (*all, last) %>"), None);
        // No comment, a comment of another kind, or one Rails' pattern does not match.
        assert_eq!(strict_locals("<%= story %>"), None);
        assert_eq!(
            strict_locals("<%# a note %>\n<%# locals: (story:) %>")
                .map(|found| found.declared.len()),
            Some(1)
        );
        assert_eq!(strict_locals("<%#locals: (story:) %>"), None);
        assert_eq!(strict_locals("<%# locals:(story:) %>"), None);
        assert_eq!(strict_locals("<%# locals: story %>"), None);
        assert_eq!(strict_locals("<%# locals: (story: %>"), None);
    }

    /// `to_partial_path`: the collection's directory and the element's name.
    #[test]
    fn a_record_renders_its_class_s_partial() {
        assert_eq!(partial_of("Story").as_deref(), Some("stories/story"));
        assert_eq!(
            partial_of("Admin::Person").as_deref(),
            Some("admin/people/person")
        );
        assert_eq!(partial_of("user_session"), None);
    }

    #[test]
    fn a_target_names_the_templates_its_pattern_allows() {
        assert!(exactly("stories/show").names("stories/show"));
        assert!(!exactly("stories/show").names("stories/index"));
        assert!(like("pages/", "").names("pages/about"));
        assert!(!like("pages/", "").names("stories/show"));
        assert!(!like("stories/", "_card").names("stories/show"));
        assert!(!like("stories/show", "_card").names("stories/show"));
        assert!(Target::Anything.names("anything/at_all"));
    }

    #[test]
    fn a_call_writes_the_name_it_renders_only_in_its_literal_parts() {
        let [named, interpolated, unread] = [
            "render template: \"layouts/application\"",
            "render template: \"layouts/#{theme}\"",
            "render options",
        ]
        .map(|source| read_renders(source, false).remove(0));
        assert!(named.writes_the_name());
        assert!(interpolated.writes_the_name());
        assert!(!unread.writes_the_name());
    }
}
