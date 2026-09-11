//! The half-dozen Prism shapes every reader in this directory starts from.
//!
//! A symbol or a string literal and its span, the value of one keyword argument, the header of
//! a call without the `do ... end` a whole-node location would drag in, and the spelling of the
//! constant a `class` keyword names. Nothing here decides anything: each answers `None` for a
//! shape it does not understand, and *what that means* is the caller's — for
//! [`super::schema`] an interpolated table name is a table it declines, and for
//! [`super::models`] a `class_name:` that is not a literal is an association it declines.

use ruby_prism::{CallNode, DefNode, Node, ParametersNode};

/// A call's first argument, when it is a symbol or a plain string.
///
/// The macros take `:user`; the schema takes `"stories"`. Both spellings are legal for both and
/// neither is worth a second reader.
pub(super) fn first_symbol_or_string(
    source: &str,
    node: &CallNode<'_>,
) -> Option<(String, (u32, u32))> {
    symbol_or_string(source, &node.arguments()?.arguments().iter().next()?)
}

/// The constant a `class` or `module` keyword names, exactly as it is written.
///
/// Sliced rather than walked, because what is wanted is the spelling: `class Admin::Setting`
/// nests one name and not two, and joining the stack with `::` then reproduces what rubydex
/// would call the same class. A leading `::` is dropped — `class ::Tag` is `Tag`.
pub(super) fn constant_spelling(source: &str, node: &Node<'_>) -> String {
    let location = node.location();
    source
        .get(location.start_offset()..location.end_offset())
        .unwrap_or_default()
        .trim_start_matches("::")
        .to_owned()
}

/// A call's first argument, when it is a plain string literal.
///
/// The one shape every reader in this file starts from: a table's name, a column's name, the
/// table `self.table_name` renames to. `None` covers a call with no arguments, a call whose
/// first argument is a symbol or a number, and — the case that matters — an interpolated
/// string, which is Ruby that only runs.
pub(super) fn first_string(source: &str, node: &CallNode<'_>) -> Option<(String, (u32, u32))> {
    string_literal(source, &node.arguments()?.arguments().iter().next()?)
}

/// The text and span of a string literal, quotes excluded. `None` for anything else.
pub(super) fn string_literal(source: &str, node: &Node<'_>) -> Option<(String, (u32, u32))> {
    let location = node.as_string_node()?.content_loc();
    let (start, end) = (location.start_offset(), location.end_offset());
    Some((
        source.get(start..end)?.to_owned(),
        (start as u32, end as u32),
    ))
}

/// The same, for a keyword's value written either way: `id: :uuid` and `id: "uuid"`.
pub(super) fn symbol_or_string(source: &str, node: &Node<'_>) -> Option<(String, (u32, u32))> {
    let Some(symbol) = node.as_symbol_node() else {
        return string_literal(source, node);
    };
    let location = symbol.value_loc()?;
    let (start, end) = (location.start_offset(), location.end_offset());
    Some((
        source.get(start..end)?.to_owned(),
        (start as u32, end as u32),
    ))
}

/// Every positional argument of a call that is a symbol or a plain string.
///
/// `helper_method :current_user, :logged_in?` names two methods; `helper_method(*EXPORTS)` and
/// `helper_method :"#{prefix}_user"` name none this reader can see, and that is the direction
/// every reader in this directory errs in. Keyword arguments are skipped for [`keyword`]'s
/// reason and not as a precaution: `helper_method` takes none, so a hash written there is
/// somebody else's macro of the same name and its keys are not method names.
pub(super) fn positional_names(source: &str, node: &CallNode<'_>) -> Vec<String> {
    let Some(arguments) = node.arguments() else {
        return Vec::new();
    };
    arguments
        .arguments()
        .iter()
        .filter(|argument| argument.as_keyword_hash_node().is_none())
        .filter_map(|argument| symbol_or_string(source, &argument).map(|(name, _)| name))
        .collect()
}

/// The value a call passes for keyword `name`, when it passes one.
pub(super) fn keyword<'pr>(node: &CallNode<'pr>, name: &str) -> Option<Node<'pr>> {
    node.arguments()?
        .arguments()
        .iter()
        .filter_map(|argument| argument.as_keyword_hash_node())
        .flat_map(|hash| hash.elements().iter().collect::<Vec<_>>())
        .filter_map(|element| element.as_assoc_node())
        .find_map(|assoc| {
            (assoc.key().as_symbol_node()?.unescaped() == name.as_bytes()).then(|| assoc.value())
        })
}

/// The value a call passes for `name`, or the innermost enclosing block that passes one.
///
/// `hosts` is the `with_options` calls the statement is written inside, innermost last.
/// `with_options class_name: 'Account' do belongs_to :approved_by_account end` is a
/// `belongs_to` whose class is `Account`, and without this it names an `ApprovedByAccount` no
/// application defines — so the merge is not a refinement, it is the difference between eleven
/// declarations and none. The call's own keyword wins over an enclosing one, which is
/// `ActiveSupport::OptionMerger`'s own `deep_merge` order rather than a choice.
///
/// Two readers ask it — [`super::models`] for `class_name:` and `through:`, [`super::delegates`]
/// for `to:` and `prefix:` — which is why it is here rather than in either.
pub(super) fn inherited<'pr>(
    node: &CallNode<'pr>,
    hosts: &[CallNode<'pr>],
    name: &str,
) -> Option<Node<'pr>> {
    keyword(node, name).or_else(|| hosts.iter().rev().find_map(|host| keyword(host, name)))
}

/// A call's own name plus its arguments — `create_table "stories", force: :cascade` — without
/// the `do ... end` that a whole-node location would drag in.
pub(super) fn header(node: &CallNode<'_>) -> Option<(u32, u32)> {
    let message = node.message_loc()?;
    let arguments = node.arguments()?.location();
    Some((message.start_offset() as u32, arguments.end_offset() as u32))
}

/// The name of a block's first required parameter — the `t` of `with_options … do |t| … end`.
///
/// `with_options` yields an option merger, and a block that takes it calls the macros *on* it:
/// `t.has_many :comments`. A block that takes nothing is `instance_eval`'d instead and the
/// macros inside it are receiverless. Both spellings are live and the second is eleven of the
/// corpus' twelve, so the first is one `Option<String>` rather than a second reader.
pub(super) fn block_parameter(node: &CallNode<'_>) -> Option<String> {
    let parameters = node.block()?.as_block_node()?.parameters()?;
    let name = parameters
        .as_block_parameters_node()?
        .parameters()?
        .requireds()
        .iter()
        .next()?
        .as_required_parameter_node()?
        .name();
    Some(String::from_utf8_lossy(name.as_slice()).into_owned())
}

/// Whether `node` is nothing but a read of the local variable `name`.
///
/// The receiver test for the block-parameter spelling above. Prism knows `t` is a local inside
/// `do |t|`, so the shape is a `LocalVariableReadNode` — but a `with_options` whose block
/// parameter is shadowed, or a receiver that is a method call of the same name, must not pass,
/// which is why this asks for the node's kind rather than comparing its text.
pub(super) fn reads_local(node: &Node<'_>, name: &str) -> bool {
    node.as_local_variable_read_node()
        .is_some_and(|read| read.name().as_slice() == name.as_bytes())
}

/// A `def`'s own line — `def perform(story_id)` — without the body a whole-node location drags in.
///
/// The same shape [`header`] has for a macro, and it is what an editor shows as the target: the
/// signature, not the method. Three spellings end at three different places — `def welcome(user)`
/// at the closing paren, `def welcome user` at the end of the parameters, and `def welcome` at
/// the end of the name.
pub(super) fn def_header(node: &DefNode<'_>) -> (u32, u32) {
    let end = node
        .rparen_loc()
        .or_else(|| node.parameters().map(|parameters| parameters.location()))
        .map_or_else(|| node.name_loc().end_offset(), |at| at.end_offset());
    (node.def_keyword_loc().start_offset() as u32, end as u32)
}

/// A keyword parameter's own name — the `subject` of `subject:` and of `subject: nil`.
///
/// **Sliced rather than matched**, and the reason is the one `annotations.rs` records for the
/// arm it could not cover: Prism's keyword list holds exactly two node kinds, so asking which
/// one this is would put a third arm here that no Ruby ever reaches. Both spell the name the
/// same way and both start with it, so the name is the text up to the first `:` — which is also
/// what makes the required and the optional spelling one case rather than two.
pub(super) fn keyword_name<'src>(source: &'src str, node: &Node<'_>) -> &'src str {
    let location = node.location();
    let text = source
        .get(location.start_offset()..location.end_offset())
        .unwrap_or_default();
    text.split_once(':').map_or(text, |(name, _)| name)
}

/// Whether RBS can spell this method name.
///
/// Every operator Ruby lets a `def` name — `<=>`, `[]`, `+` — reaches here, and the whole file's
/// declarations ride on the answer: `Synthesized::record` parses a generated document whole and
/// refuses all of it if any line does not, so one unspellable name would take a mailer's other
/// eleven actions with it. An action Rails routes to is a plain identifier by construction,
/// because it has to be a template's file name too.
///
/// **A trailing `=` is spellable and the corpus is why.** RBS writes `def primary_key=: (untyped)
/// -> untyped` and this crate already declares such names — `Affix::Around("", "=")` is how every
/// `mattr_writer` in [`super::tail`] renders. Leaving it out cost the writers a concern's
/// `ClassMethods` declares: `self.primary_key = :id` in lobsters' `StoryText` went from answered to
/// guessed, found by a tier diff over 1,200 cursors rather than by the suite. `==` and `[]=` are
/// still refused, because stripping the one `=` leaves a first character that is not a letter.
pub(super) fn spellable(name: &str) -> bool {
    let mut characters = name.strip_suffix(['?', '!', '=']).unwrap_or(name).chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|rest| rest.is_ascii_alphanumeric() || rest == '_')
}

/// The RBS parameter list a `def`'s parameters imply, every type `untyped`.
///
/// The shape and not the types, which is all a convention can know — and all it needs to know,
/// because arity is what `types.rs` matches on and a keyword is not part of it. `perform_later`
/// taking exactly what `perform` takes is the whole requirement, and it is what keeps a
/// two-argument call from being rejected against a zero-argument declaration.
pub(super) fn parameters_of(source: &str, node: Option<&ParametersNode<'_>>) -> String {
    let Some(node) = node else {
        return "()".to_owned();
    };
    let mut spelled: Vec<String> = Vec::new();
    spelled.extend(node.requireds().iter().map(|_| "untyped".to_owned()));
    spelled.extend(node.optionals().iter().map(|_| "?untyped".to_owned()));
    // A `def`'s rest is a `*rest` or nothing: Prism's `ImplicitRestNode` — the `|a,|` of a
    // block — cannot appear in a method's parameters, so asking which kind this is would put an
    // arm here that no Ruby reaches.
    if node.rest().is_some() {
        spelled.push("*untyped".to_owned());
    }
    // Trailing positionals are required exactly as the leading ones are; RBS keeps them in
    // their own list only so it can say where the optional ones went.
    spelled.extend(node.posts().iter().map(|_| "untyped".to_owned()));
    for keyword in node.keywords().iter() {
        let optional = keyword.as_optional_keyword_parameter_node().is_some();
        let name = keyword_name(source, &keyword);
        spelled.push(format!(
            "{}{name}: untyped",
            if optional { "?" } else { "" }
        ));
    }
    if let Some(rest) = node.keyword_rest() {
        // `**nil` is the third kind this can be, and it says the method takes no keywords at
        // all — so it is exactly the one that adds nothing.
        if rest.as_forwarding_parameter_node().is_some() {
            spelled.push("*untyped".to_owned());
            spelled.push("**untyped".to_owned());
        } else if rest.as_keyword_rest_parameter_node().is_some() {
            spelled.push("**untyped".to_owned());
        }
    }
    format!("({})", spelled.join(", "))
}
