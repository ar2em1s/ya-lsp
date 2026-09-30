//! The half-dozen Prism shapes every reader in this directory starts from.
//!
//! A symbol or string literal and its span, the value of one keyword argument, the header of a call
//! without the `do ... end` a whole-node location would drag in, and the spelling of the constant a
//! `class` keyword names.
//!
//! Nothing here decides anything: each answers `None` for a shape it does not understand, and *what
//! that means* is the caller's call. For [`super::schema`] an interpolated table name is a table it
//! declines; for [`super::models`] a `class_name:` that is not a literal is an association it
//! declines.

use ruby_prism::{CallNode, DefNode, Node, ParametersNode, StatementsNode};

/// A call's first argument, when it is a symbol or a plain string.
///
/// The macros take `:user`; the schema takes `"stories"`. Both spellings are legal for both, and
/// neither is worth a second reader.
pub(super) fn first_symbol_or_string(
    source: &str,
    node: &CallNode<'_>,
) -> Option<(String, (u32, u32))> {
    symbol_or_string(source, &node.arguments()?.arguments().iter().next()?)
}

/// The constant a `class` or `module` keyword names, exactly as written.
///
/// Sliced, not walked, because the spelling is what is wanted: `class Admin::Setting` nests one
/// name, not two, and joining the stack with `::` reproduces what rubydex calls the same class. A
/// leading `::` is dropped: `class ::Tag` is `Tag`.
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
/// The shape every reader in this file starts from: a table's name, a column's name, the table
/// `self.table_name` renames to. `None` covers a call with no arguments, a first argument that is a
/// symbol or a number, and (the case that matters) an interpolated string, which only running Ruby
/// can read.
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
/// `helper_method :"#{prefix}_user"` name none this reader can see, the direction every reader here
/// errs in. Keyword arguments are skipped for [`keyword`]'s reason, not as a precaution:
/// `helper_method` takes none, so a hash written there belongs to somebody else's macro of the same
/// name, and its keys are not method names.
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
/// `with_options class_name: 'Account' do belongs_to :approved_by_account end` is a `belongs_to`
/// whose class is `Account`; without this it names an `ApprovedByAccount` no application defines,
/// so the merge is the difference between a block's declarations and none. The call's own keyword
/// wins over an enclosing one: `ActiveSupport::OptionMerger`'s own `deep_merge` order, not a
/// choice.
///
/// Two readers ask it ([`super::models`] for `class_name:` and `through:`, [`super::delegates`] for
/// `to:` and `prefix:`), so it lives here, not in either.
pub(super) fn inherited<'pr>(
    node: &CallNode<'pr>,
    hosts: &[CallNode<'pr>],
    name: &str,
) -> Option<Node<'pr>> {
    keyword(node, name).or_else(|| hosts.iter().rev().find_map(|host| keyword(host, name)))
}

/// Whether a constant is written from the top (`::Spree`, `::Spree::Core`), which names that constant
/// wherever it is written: Ruby's own escape from the lexical walk. [`constant_spelling`] drops the
/// `::`, so the path is asked: its leftmost segment has no parent.
pub(super) fn absolute(node: &Node<'_>) -> bool {
    let Some(mut path) = node.as_constant_path_node() else {
        return false;
    };
    while let Some(parent) = path.parent() {
        let Some(outer) = parent.as_constant_path_node() else {
            return false;
        };
        path = outer;
    }
    true
}

/// A call's own name plus its arguments (`create_table "stories", force: :cascade`), without the
/// `do ... end` a whole-node location would drag in.
pub(super) fn header(node: &CallNode<'_>) -> Option<(u32, u32)> {
    let message = node.message_loc()?;
    let arguments = node.arguments()?.location();
    Some((message.start_offset() as u32, arguments.end_offset() as u32))
}

/// The name of a block's first required parameter: the `t` of `with_options … do |t| … end`.
///
/// `with_options` yields an option merger, and a block that takes it calls the macros *on* it:
/// `t.has_many :comments`. A block that takes nothing is `instance_eval`'d instead, and the macros
/// inside are receiverless. Both spellings are live (the second far more common), so the first
/// costs one `Option<String>`, not a second reader.
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
/// `do |t|`, so the shape is a `LocalVariableReadNode`. A `with_options` whose block parameter is
/// shadowed, or a receiver that is a method call of the same name, must not pass, which is why this
/// checks the node's kind, not its text.
pub(super) fn reads_local(node: &Node<'_>, name: &str) -> bool {
    node.as_local_variable_read_node()
        .is_some_and(|read| read.name().as_slice() == name.as_bytes())
}

/// The body of every `class` or `module` a file writes under the name `wanted`, reached through the
/// class and module bodies written as statements above it.
///
/// `nesting` is the path walked so far, empty at the top of a file. The name is the whole path, not
/// its last segment, and a class body is descended into as well as a module's, because the wanted
/// body may be nested in one (`Random::Formatter`). A matching body is not descended into again.
///
/// Statements only, not a generic visitor, `models::Models::walk`'s reason: one that descends into
/// every method body in a large file is thousands of frames on a 2 MiB stack.
pub(super) fn bodies_named<'pr>(
    source: &str,
    statements: Option<StatementsNode<'pr>>,
    wanted: &str,
    nesting: &mut Vec<String>,
    found: &mut Vec<StatementsNode<'pr>>,
) {
    let Some(statements) = statements else {
        return;
    };
    if !nesting.is_empty() && nesting.join("::") == wanted {
        found.push(statements);
        return;
    }
    for statement in statements.body().iter() {
        let (path, body) = if let Some(module) = statement.as_module_node() {
            (module.constant_path(), module.body())
        } else if let Some(class) = statement.as_class_node() {
            (class.constant_path(), class.body())
        } else {
            continue;
        };
        nesting.push(constant_spelling(source, &path));
        bodies_named(
            source,
            body.and_then(|body| body.as_statements_node()),
            wanted,
            nesting,
            found,
        );
        nesting.pop();
    }
}

/// The whole `def … end` a generated member was read from.
///
/// **Not just the header.** The protocol's `targetRange` is *the range enclosing this symbol*, and
/// `targetSelectionRange` is the name inside it: exactly the pair `locator::spans` returns for
/// every `def` a file writes. A header-only span would make a mailer action or a concern's class
/// method open a peek view holding one line of a method the same jump shows whole when the `def` is
/// reached directly.
///
/// [`header`] keeps the narrow shape, rightly: a macro's construct **is** its call line.
pub(super) fn def_span(node: &DefNode<'_>) -> (u32, u32) {
    let at = node.location();
    (at.start_offset() as u32, at.end_offset() as u32)
}

/// A keyword parameter's own name: the `subject` of `subject:` and of `subject: nil`.
///
/// **Sliced, not matched**, for the reason `annotations.rs` records for the arm it could not cover:
/// Prism's keyword list holds exactly two node kinds, so asking which one this is would add a third
/// arm no Ruby reaches. Both spell the name the same way and start with it, so the name is the text
/// up to the first `:`, which also makes the required and optional spellings one case.
pub(super) fn keyword_name<'src>(source: &'src str, node: &Node<'_>) -> &'src str {
    let location = node.location();
    let text = source
        .get(location.start_offset()..location.end_offset())
        .unwrap_or_default();
    text.split_once(':').map_or(text, |(name, _)| name)
}

/// Whether RBS can spell this method name.
///
/// Every operator Ruby lets a `def` name (`<=>`, `[]`, `+`) reaches here, and the whole file's
/// declarations ride on the answer: `Synthesized::record` parses a generated document whole and
/// refuses all of it if any line fails, so one unspellable name would take every other action in a
/// mailer with it. An action Rails routes to is a plain identifier by construction, because it must
/// also be a template's file name.
///
/// **A trailing `=` is spellable.** RBS writes `def primary_key=: (untyped) -> untyped`, and this
/// crate already declares such names: `Affix::Around("", "=")` is how every `mattr_writer` in
/// [`super::tail`] renders. Refusing it would drop the writers a concern's `ClassMethods` declares,
/// so `self.primary_key = :id` would fall from answered to guessed. `==` and `[]=` are still
/// refused, because stripping the one `=` leaves a first character that is not a letter.
pub(super) fn spellable(name: &str) -> bool {
    let mut characters = name.strip_suffix(['?', '!', '=']).unwrap_or(name).chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|rest| rest.is_ascii_alphanumeric() || rest == '_')
}

/// The RBS parameter list a `def`'s parameters imply, every type `untyped`.
///
/// The shape, not the types: all a convention can know, and all it needs, because arity is what
/// `types.rs` matches on, and a keyword is not part of it. `perform_later` taking exactly what
/// `perform` takes is the whole requirement: it keeps a two-argument call from being rejected
/// against a zero-argument declaration.
pub(super) fn parameters_of(source: &str, node: Option<&ParametersNode<'_>>) -> String {
    let Some(node) = node else {
        return "()".to_owned();
    };
    let mut spelled: Vec<String> = Vec::new();
    spelled.extend(node.requireds().iter().map(|_| "untyped".to_owned()));
    spelled.extend(node.optionals().iter().map(|_| "?untyped".to_owned()));
    // A `def`'s rest is a `*rest` or nothing: Prism's `ImplicitRestNode` (the `|a,|` of a block)
    // cannot appear in a method's parameters, so asking which kind this is would add an arm no Ruby
    // reaches.
    if node.rest().is_some() {
        spelled.push("*untyped".to_owned());
    }
    // Trailing positionals are required exactly like the leading ones; RBS keeps them in their own
    // list only to say where the optional ones went.
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
        // `**nil` is the third kind this can be: it says the method takes no keywords at all, so it
        // is exactly the one that adds nothing.
        if rest.as_forwarding_parameter_node().is_some() {
            spelled.push("*untyped".to_owned());
            spelled.push("**untyped".to_owned());
        } else if rest.as_keyword_rest_parameter_node().is_some() {
            spelled.push("**untyped".to_owned());
        }
    }
    format!("({})", spelled.join(", "))
}
