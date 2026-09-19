//! What a human wrote the type down as: a Sorbet `sig` block, or a YARD `@return` tag.
//!
//! The only generator that reads a *claim*, not a fact. `db/schema.rb` is what the database is, and
//! `belongs_to :user` is what ActiveRecord will do. A `sig` or a `@return` is what somebody
//! believed when writing the line, and nothing checks it unless a type checker runs. So both are
//! **derived** answers, and the provenance comment says which one was read.
//!
//! # Why one module reads both
//!
//! Same shape: find a method, find a type written next to it, produce RBS. Only the syntax differs,
//! and not in a way that reaches the output:
//! - a `sig` is a Ruby AST Prism already parsed;
//! - a YARD tag is a comment above a `def`;
//! - both end at `Declarations::declare`.
//!
//! Splitting them would duplicate the parameter renderer, the half of this module with the real
//! detail.
//!
//! Sorbet wins where both are present. A `sig` is machine-checked by `srb`, written in Ruby the
//! parser validates, and rots loudly when the method changes. A comment rots quietly.
//!
//! # Nothing here is a place
//!
//! Every declaration this module writes is unmapped, on purpose. The user's own `def` is already
//! indexed at the offset the editor should jump to, so this adds a *type* and nothing else. A span
//! would point a second definition at the same line and list that location twice in
//! go-to-definition.
//!
//! # What is deliberately not read
//!
//! - Sorbet: `T.any`, `T.all`, `T.proc`, `T.self_type`, `T.type_parameter`.
//! - YARD: duck types (`[#read]`), `Hash{Symbol=>String}`, `@!method`, `@!attribute`.
//!
//! Each needs a type representation `Types` lacks, or names something that is not a class. The
//! safety argument: a type this module cannot spell exactly is a method it declares nothing about.
//!
//! Positional parameter *names* are dropped too, as a risk trade. They would buy a nicer
//! signature-help line, but a parameter called `type` or `class` is an RBS keyword that would take
//! the whole file's declarations down. Types and arity are kept, which is what `types.rs`' arity
//! partition reads.

use std::collections::BTreeMap;

use ruby_prism::{CallNode, DefNode, Node, ParametersNode};

use crate::generated::{Declared, Facts, Owner, Source};

/// Sorbet's own namespace. Everything under it is a type constructor, not a class.
const SORBET: &str = "T";

/// The Sorbet generics whose RBS spelling is the same name without the `T::`.
const SORBET_GENERICS: [&str; 6] = ["Array", "Hash", "Set", "Range", "Enumerable", "Enumerator"];

/// What a YARD tag can say that is not the name of a class.
const YARD_ALIASES: [(&str, &str); 4] = [
    ("Boolean", "bool"),
    ("bool", "bool"),
    ("true", "bool"),
    ("false", "bool"),
];

/// Which of the two said so. It reaches a hover card, so it is a sentence, not a tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wrote {
    Sorbet,
    Yard,
}

impl Wrote {
    fn spelled(self) -> &'static str {
        match self {
            Self::Sorbet => "a Sorbet `sig` block",
            Self::Yard => "a YARD `@return` tag",
        }
    }
}

/// Read every method in `source` that a `sig` block or a `@return` tag gives a return type.
///
/// `file` is how the source is spelled to a reader, and goes into every provenance line. Text in,
/// no graph and no I/O.
#[must_use]
pub fn read(source: &str, file: &str) -> Facts {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut reader = Reader {
        source,
        file,
        nesting: Vec::new(),
        out: Facts::default(),
    };
    reader.walk(
        parsed
            .node()
            .as_program_node()
            .map(|program| program.statements().as_node()),
    );
    reader.out
}

struct Reader<'src> {
    source: &'src str,
    file: &'src str,
    /// The class and module bodies open around the cursor, and whether each is a `module`.
    ///
    /// **The flag is load-bearing.** Declaring a `def self.label` in `module Admin` as
    /// `class Admin` says `Admin` is a class. rubydex holds one kind of declaration per constant,
    /// so the module would lose whatever else it declares, as a joined `class A::B::C` costs `A::B`
    /// its members. [`Owner::ModuleSingleton`] is picked instead.
    nesting: Vec<(String, bool)>,
    out: Facts,
}

impl Reader<'_> {
    /// One body, and then the class and module bodies written as statements of it.
    ///
    /// **Statements, not a walk of the whole tree.** This is a depth bound. A generic `Visit`
    /// descends into every method body, which on a large file means thousands of frames on a thread
    /// with Rust's default 2 MiB stack: a real crash. Recursing on class nesting keeps the depth to
    /// `module A; module B; class C`. It also says what every reader here says: a `class` inside an
    /// `if` is not a statement of the enclosing body, and neither is a macro inside a block.
    fn walk(&mut self, body: Option<Node<'_>>) {
        let Some(statements) = body.and_then(|body| body.as_statements_node()) else {
            return;
        };
        self.annotated(&statements);
        for statement in statements.body().iter() {
            let (path, inner, module) = if let Some(class) = statement.as_class_node() {
                (class.constant_path(), class.body(), false)
            } else if let Some(module) = statement.as_module_node() {
                (module.constant_path(), module.body(), true)
            } else {
                continue;
            };
            self.nesting.push((spelling(self.source, &path), module));
            self.walk(inner);
            self.nesting.pop();
        }
    }

    /// The annotated methods written as statements of one class body.
    ///
    /// A `sig` binds to the statement right after it, as in Sorbet. Anything in between breaks the
    /// binding instead of being skipped: a `sig` followed by a constant assignment is a file this
    /// reader does not understand, and misreading it is how a method gets its neighbour's type.
    fn annotated(&mut self, statements: &ruby_prism::StatementsNode<'_>) {
        if self.nesting.is_empty() {
            return;
        }
        let mut facts = Facts::default();
        let mut sig = None;
        for statement in statements.body().iter() {
            if let Some(call) = statement.as_call_node()
                && call.name().as_slice() == b"sig"
                && call.block().is_some()
            {
                sig = Some(statement);
                continue;
            }
            if let Some(def) = statement.as_def_node() {
                self.method(&mut facts, &def, sig.as_ref());
            }
            sig = None;
        }
        // Into a local and then merged, not straight into `self.out`, because `self.method` borrows
        // the reader. The order is the same: one body's methods, as written, before anything nested
        // inside.
        self.out.extend(facts);
    }

    /// One `def`, and the type somebody wrote beside it. Writes nothing when nobody did.
    fn method(&self, into: &mut Facts, def: &DefNode<'_>, sig: Option<&Node<'_>>) {
        let block = sig.and_then(|node| block_body(&node.as_call_node()?));
        let sorbet = block.as_ref().and_then(|body| result(body));
        let (returns, wrote) = match sorbet {
            Some(returns) => (returns, Wrote::Sorbet),
            None => match self.yard_return(def) {
                Some(returns) => (returns, Wrote::Yard),
                None => return,
            },
        };
        let types = match wrote {
            Wrote::Sorbet => block.as_ref().map(|body| params(body)).unwrap_or_default(),
            Wrote::Yard => self.yard_params(def),
        };

        let name = String::from_utf8_lossy(def.name().as_slice()).into_owned();
        // A `def` whose name is not a plain identifier (`def ==`, `def []=`) is legal RBS, but not
        // with the parameter list this renders. So the surface is kept to names an annotation is
        // realistically written above.
        let stem = name.trim_end_matches(['?', '!', '=']);
        if !stem.starts_with(|first: char| first.is_ascii_lowercase() || first == '_')
            || !stem
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return;
        }
        let singleton = def
            .receiver()
            .is_some_and(|receiver| receiver.as_self_node().is_some());
        let class = self
            .nesting
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join("::");
        // The innermost body decides: it is the one this `def` is written in.
        let module = self.nesting.last().is_some_and(|(_, module)| *module);
        into.declare(Declared {
            owner: match (singleton, module) {
                (false, false) => Owner::Instance(class),
                (false, true) => Owner::Module(class),
                (true, false) => Owner::Singleton(class),
                (true, true) => Owner::ModuleSingleton(class),
            },
            name: name.clone(),
            returns,
            parameters: signature(def.parameters().as_ref(), &types),
            because: format!(
                "From `{}`, {} on `{}{name}`.",
                self.file,
                wrote.spelled(),
                if singleton { "self." } else { "" }
            ),
            at: None,
            from: Source::Annotated,
            overloads: Vec::new(),
        });
    }

    /// The `@return` tag in the comment block above `def`, as an RBS type.
    fn yard_return(&self, def: &DefNode<'_>) -> Option<String> {
        self.comments(def)
            .iter()
            .rev()
            .find_map(|line| yard_type(line.strip_prefix("@return ")?.trim()))
    }

    /// Every `@param` tag above `def`, by the parameter it names.
    ///
    /// Both orders YARD allows: `@param count [Integer] how many` and `@param [Integer] count`.
    fn yard_params(&self, def: &DefNode<'_>) -> BTreeMap<String, String> {
        let mut types = BTreeMap::new();
        for line in self.comments(def) {
            let Some(rest) = line.strip_prefix("@param ") else {
                continue;
            };
            let rest = rest.trim();
            let Some((name, written)) = tagged(rest) else {
                continue;
            };
            if let Some(spelled) = yard_type(written) {
                types.insert(name.to_owned(), spelled);
            }
        }
        types
    }
}

/// A `@param` tag split into the parameter it names and the type it gives.
///
/// Both orders YARD allows; the name is the first word after or before the brackets.
fn tagged(rest: &str) -> Option<(&str, &str)> {
    let (name, written) = if rest.starts_with('[') {
        let end = rest.find(']')?;
        (rest.get(end + 1..)?, rest.get(..=end)?)
    } else {
        let at = rest.find('[')?;
        (rest.get(..at)?, rest.get(at..)?)
    };
    Some((name.split_whitespace().next()?, written))
}

impl Reader<'_> {
    /// The comment block directly above `def`, with `#` and leading space stripped.
    ///
    /// Read from the text, not Prism's comment list, because "the lines right above this one" is a
    /// fact about layout: a blank line ends the block, and a comment separated from the `def` by
    /// anything belongs to something else.
    fn comments(&self, def: &DefNode<'_>) -> Vec<String> {
        let at = def.def_keyword_loc().start_offset();
        let start = self.source.get(..at).map_or(0, |before| {
            before.rfind('\n').map_or(0, |newline| newline + 1)
        });
        let mut block = Vec::new();
        let mut rest = self
            .source
            .get(..start.saturating_sub(1))
            .unwrap_or_default();
        while !rest.is_empty() {
            let line_at = rest.rfind('\n').map_or(0, |newline| newline + 1);
            let line = rest.get(line_at..).unwrap_or_default().trim();
            let Some(comment) = line.strip_prefix('#') else {
                break;
            };
            block.push(comment.trim().to_owned());
            rest = rest.get(..line_at.saturating_sub(1)).unwrap_or_default();
        }
        block.reverse();
        block
    }
}

/// The statement inside `sig { ... }`.
fn block_body<'pr>(call: &CallNode<'pr>) -> Option<Node<'pr>> {
    call.block()?
        .as_block_node()?
        .body()?
        .as_statements_node()?
        .body()
        .iter()
        .next()
}

/// What a `sig` chain says the method returns.
///
/// Read from the outside in: `params(...).returns(String)` is a `returns` call whose receiver is a
/// `params` call. So an unknown modifier (`abstract`, `overridable`, `checked(:never)`) is walked
/// past, not tripped over.
fn result(node: &Node<'_>) -> Option<String> {
    let call = node.as_call_node()?;
    match call.name().as_slice() {
        b"returns" => sorbet_type(&call.arguments()?.arguments().iter().next()?),
        b"void" => Some("void".to_owned()),
        _ => result(&call.receiver()?),
    }
}

/// What a `sig` chain says the parameters are, by name.
fn params(node: &Node<'_>) -> BTreeMap<String, String> {
    let mut types = BTreeMap::new();
    let mut at = Some(node.as_call_node());
    while let Some(Some(call)) = at {
        if call.name().as_slice() == b"params"
            && let Some(arguments) = call.arguments()
        {
            for assoc in arguments
                .arguments()
                .iter()
                .filter_map(|argument| argument.as_keyword_hash_node())
                .flat_map(|hash| hash.elements().iter().collect::<Vec<_>>())
                .filter_map(|element| element.as_assoc_node())
            {
                if let Some(key) = assoc.key().as_symbol_node()
                    && let Some(written) = sorbet_type(&assoc.value())
                {
                    types.insert(
                        String::from_utf8_lossy(key.unescaped()).into_owned(),
                        written,
                    );
                }
            }
        }
        at = Some(call.receiver().and_then(|receiver| receiver.as_call_node()));
    }
    types
}

/// One Sorbet type, as RBS. `None` for everything this crate cannot spell exactly.
fn sorbet_type(node: &Node<'_>) -> Option<String> {
    if node.as_constant_read_node().is_some() || node.as_constant_path_node().is_some() {
        let written = written(node);
        return match written.as_str() {
            "T::Boolean" => Some("bool".to_owned()),
            path if path.starts_with("T::") => None,
            path => Some(path.to_owned()),
        };
    }
    let call = node.as_call_node()?;
    let name = String::from_utf8_lossy(call.name().as_slice()).into_owned();
    let receiver = call.receiver()?;
    // `T::Array[String]` is an index call on the constant `T::Array`.
    if name == "[]" {
        let head = written(&receiver);
        let element = head
            .strip_prefix("T::")
            .filter(|head| SORBET_GENERICS.contains(head))?;
        let arguments: Option<Vec<String>> = call
            .arguments()?
            .arguments()
            .iter()
            .map(|argument| sorbet_type(&argument))
            .collect();
        return Some(format!("{element}[{}]", arguments?.join(", ")));
    }
    if written(&receiver) != SORBET {
        return None;
    }
    match name.as_str() {
        "untyped" => Some("untyped".to_owned()),
        "nilable" => {
            let inner = sorbet_type(&call.arguments()?.arguments().iter().next()?)?;
            Some(optional(&inner))
        }
        _ => None,
    }
}

/// One YARD type list (the text between the brackets, brackets included) as RBS.
///
/// `[String, nil]` is the one union worth reading: it is how YARD spells optional, and most unions
/// written are that. Any other union is declined: `Types` keys an answer by one declaration, so
/// `String | Integer` has no honest representation.
fn yard_type(written: &str) -> Option<String> {
    let inner = written.trim().strip_prefix('[')?;
    let inner = inner.get(..inner.find(']')?)?;
    let mut parts: Vec<&str> = split(inner, ',');
    let nilable = parts.contains(&"nil");
    parts.retain(|part| *part != "nil");
    // `[true, false]` is YARD's other spelling of a boolean, and neither half is a class.
    if parts.len() == 2
        && parts
            .iter()
            .all(|part| YARD_ALIASES.iter().any(|(yard, _)| yard == part))
    {
        return Some("bool".to_owned());
    }
    let [only] = parts.as_slice() else {
        return None;
    };
    let spelled = yard_class(only)?;
    Some(if nilable { optional(&spelled) } else { spelled })
}

/// One YARD class name, as RBS.
fn yard_class(written: &str) -> Option<String> {
    if let Some((_, rbs)) = YARD_ALIASES.iter().find(|(yard, _)| *yard == written) {
        return Some((*rbs).to_owned());
    }
    // **`Object` is YARD's way of writing "anything", and this crate already has one.** RBS spells
    // that `untyped`, and `Types::harvest` drops `untyped` so a claim about nothing displaces no
    // rung below it. `Object` is a real class in the graph: believing it would answer every chain
    // off it with `Kernel`'s members and remove the name rung. In real code, `@return [Object]`
    // never means the class.
    if written == "Object" {
        return None;
    }
    // A duck type names a method and `Hash{Symbol=>String}` names a shape. Neither is a class.
    if written.starts_with('#') || written.contains('{') {
        return None;
    }
    if let Some(open) = written.find('<') {
        let head = constant(written.get(..open)?)?;
        let inner = written.get(open + 1..written.strip_suffix('>')?.len())?;
        let arguments: Option<Vec<String>> =
            split(inner, ',').into_iter().map(yard_class).collect();
        return Some(format!("{head}[{}]", arguments?.join(", ")));
    }
    constant(written)
}

/// `written` if it is a constant path and nothing else. Every spelled type passes this gate.
fn constant(written: &str) -> Option<String> {
    let path = written.trim().trim_start_matches("::");
    (path.starts_with(|first: char| first.is_ascii_uppercase())
        && path.split("::").all(|segment| {
            segment.starts_with(|first: char| first.is_ascii_uppercase())
                && segment
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        }))
    .then(|| path.to_owned())
}

/// The RBS parameter list and block, from what the `def` wrote and what the annotation said.
///
/// Arity must be exact. An answer is partitioned by how many positional arguments the *call* wrote,
/// so a wrong arity does not merely display wrongly: it answers nothing, or answers for a call
/// never written. Anything this cannot render exactly falls back to `(*untyped)`, which is variadic
/// and fits every arity.
fn signature(parameters: Option<&ParametersNode<'_>>, types: &BTreeMap<String, String>) -> String {
    let Some(parameters) = parameters else {
        return "()".to_owned();
    };
    let mut parts: Vec<String> = Vec::new();
    let named = |node: &Node<'_>, default: &str| -> String {
        node.as_required_parameter_node()
            .map(|required| String::from_utf8_lossy(required.name().as_slice()).into_owned())
            .and_then(|name| types.get(&name).cloned())
            .unwrap_or_else(|| default.to_owned())
    };
    for required in parameters.requireds().iter() {
        parts.push(named(&required, "untyped"));
    }
    for optional in parameters.optionals().iter() {
        let name = optional
            .as_optional_parameter_node()
            .map(|node| String::from_utf8_lossy(node.name().as_slice()).into_owned());
        let written = name
            .and_then(|name| types.get(&name).cloned())
            .unwrap_or_else(|| "untyped".to_owned());
        parts.push(format!("?{written}"));
    }
    if parameters.rest().is_some() {
        parts.push("*untyped".to_owned());
    }
    for post in parameters.posts().iter() {
        parts.push(named(&post, "untyped"));
    }
    for keyword in parameters.keywords().iter() {
        let (name, optional) = match (
            keyword.as_required_keyword_parameter_node(),
            keyword.as_optional_keyword_parameter_node(),
        ) {
            (Some(required), _) => (required.name(), false),
            (_, Some(optional)) => (optional.name(), true),
            _ => continue,
        };
        let name = String::from_utf8_lossy(name.as_slice()).into_owned();
        let written = types
            .get(&name)
            .cloned()
            .unwrap_or_else(|| "untyped".to_owned());
        parts.push(format!(
            "{}{name}: {written}",
            if optional { "?" } else { "" }
        ));
    }
    if let Some(keyword_rest) = parameters.keyword_rest() {
        // `def go(...)` forwards everything, and Prism files all of it here. There is no arity to
        // state, so the honest signature is the variadic one that fits every call.
        if keyword_rest.as_forwarding_parameter_node().is_some() {
            return "(*untyped)".to_owned();
        }
        // `**nil` says there are none, which RBS spells by saying nothing.
        if keyword_rest.as_no_keywords_parameter_node().is_none() {
            parts.push("**untyped".to_owned());
        }
    }
    let block = if parameters.block().is_some() {
        " ?{ (*untyped) -> untyped }"
    } else {
        ""
    };
    format!("({}){block}", parts.join(", "))
}

/// `String` -> `String?`, and `untyped` -> `untyped`, which already includes `nil`.
fn optional(written: &str) -> String {
    if written == "untyped" || written.ends_with('?') {
        return written.to_owned();
    }
    format!("{written}?")
}

/// Split on `separator`, ignoring one nested in `<>` or `{}`, as in `Hash<Symbol, Array<String>>`.
fn split(written: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (at, character) in written.char_indices() {
        match character {
            '<' | '{' | '(' => depth += 1,
            '>' | '}' | ')' => depth = depth.saturating_sub(1),
            _ if character == separator && depth == 0 => {
                parts.push(written[start..at].trim());
                start = at + character.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(written[start..].trim());
    parts
}

/// A node's own text, which for a constant is the way it was spelled.
fn written(node: &Node<'_>) -> String {
    let location = node.location();
    String::from_utf8_lossy(location.as_slice()).into_owned()
}

/// The same, from the source, with a leading `::` dropped: `class ::Tag` is `Tag`.
fn spelling(source: &str, node: &Node<'_>) -> String {
    let location = node.location();
    source
        .get(location.start_offset()..location.end_offset())
        .unwrap_or_default()
        .trim_start_matches("::")
        .to_owned()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::testing::*;
    use crate::generated::declaring;

    /// The RBS one snippet declares, with the provenance lines dropped.
    ///
    /// `modules` names what the snippet's own file writes `module` for. `Facts::render` needs it to
    /// decide whether a namespace above an owner may be joined onto it.
    fn declared_by(source: &str, modules: &[&str]) -> String {
        read(source, "app/widget.rb")
            .render(&declaring(modules))
            .rbs
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .map(|line| format!("{line}\n"))
            .collect()
    }

    /// The same, for the snippets whose owners are all top level.
    fn declared(source: &str) -> String {
        declared_by(source, &[])
    }

    /// The return type one annotated `def` gets, or `None` where it gets none.
    fn returns(annotation: &str) -> Option<String> {
        let source = format!("class W\n  {annotation}\n  def go\n  end\nend\n");
        let rbs = read(&source, "app/widget.rb").render(&declaring(&[])).rbs;
        Some(
            rbs.lines()
                .find(|line| line.trim_start().starts_with("def go:"))?
                .split(" -> ")
                .nth(1)?
                .to_owned(),
        )
    }

    #[test]
    fn the_rbs_an_annotated_class_declares() {
        // Pinned whole: the class it reopens, the provenance line, the singleton spelling.
        assert_eq!(
            read(
                "class Widget\n  \
                 sig { returns(String) }\n  \
                 def name\n  end\n\n  \
                 # @return [Integer]\n  \
                 def self.count\n  end\n\
                 end\n",
                "app/widget.rb"
            )
            .render(&declaring(&[]))
            .rbs,
            "\
class Widget
  # From `app/widget.rb`, a Sorbet `sig` block on `name`.
  def name: () -> String
  # From `app/widget.rb`, a YARD `@return` tag on `self.count`.
  def self.count: () -> Integer
end
"
        );
    }

    #[test]
    fn what_a_sig_block_says_a_method_returns() {
        assert_eq!(
            returns("sig { returns(String) }").as_deref(),
            Some("String")
        );
        assert_eq!(
            returns("sig { returns(Admin::Setting) }").as_deref(),
            Some("Admin::Setting")
        );
        assert_eq!(
            returns("sig { returns(T.nilable(String)) }").as_deref(),
            Some("String?")
        );
        assert_eq!(
            returns("sig { returns(T::Boolean) }").as_deref(),
            Some("bool")
        );
        assert_eq!(
            returns("sig { returns(T.untyped) }").as_deref(),
            Some("untyped")
        );
        assert_eq!(
            returns("sig { returns(T.nilable(T.untyped)) }").as_deref(),
            Some("untyped")
        );
        assert_eq!(
            returns("sig { returns(T.nilable(T.nilable(String))) }").as_deref(),
            Some("String?")
        );
        assert_eq!(
            returns("sig { returns(T::Array[String]) }").as_deref(),
            Some("Array[String]")
        );
        assert_eq!(
            returns("sig { returns(T::Hash[Symbol, String]) }").as_deref(),
            Some("Hash[Symbol, String]")
        );
        assert_eq!(returns("sig { void }").as_deref(), Some("void"));
        // An unknown modifier is walked past, not tripped over.
        assert_eq!(
            returns("sig { abstract.returns(String) }").as_deref(),
            Some("String")
        );
        assert_eq!(
            returns("sig(:final) { returns(String).checked(:never) }").as_deref(),
            Some("String")
        );
    }

    #[test]
    fn what_a_sig_block_declares_nothing_about() {
        // Each needs a type representation `Types` lacks, or names something that is not a class. A
        // method this cannot spell exactly is one it stays silent about.
        for annotation in [
            "sig { returns(T.any(String, Integer)) }",
            "sig { returns(T.all(Comparable, Enumerable)) }",
            "sig { returns(T.self_type) }",
            "sig { returns(T.type_parameter(:U)) }",
            "sig { returns(T::Enumerator::Lazy[String]) }",
            "sig { returns(T::Something) }",
            "sig { returns(Foo.bar) }",
            "sig { returns(T::Array[T.any(String, Integer)]) }",
            "sig { returns(:symbol) }",
            "sig { returns }",
            "sig { }",
            "sig",
            "sig { params(x: Integer) }",
            "checked { returns(String) }",
        ] {
            assert_eq!(returns(annotation), None, "{annotation}");
        }
    }

    #[test]
    fn a_params_list_this_cannot_read_costs_the_parameter_and_not_the_method() {
        // A `params` block decorates an answer already given: the return type is what types a
        // chain. A parameter this cannot spell falls back to `untyped` instead of taking the method
        // down.
        for annotation in [
            "sig { params.returns(String) }",
            "sig { params(\"x\" => Integer).returns(String) }",
            "sig { params(x: T.any(A, B)).returns(String) }",
        ] {
            assert_eq!(
                returns(annotation).as_deref(),
                Some("String"),
                "{annotation}"
            );
        }
        assert_eq!(
            declared(
                "class W\n  \
                 sig { params(x: T.any(A, B)).returns(String) }\n  \
                 def go(x)\n  end\n\
                 end\n"
            ),
            "class W\n  def go: (untyped) -> String\nend\n"
        );
    }

    #[test]
    fn what_a_yard_tag_says_a_method_returns() {
        assert_eq!(returns("# @return [String]").as_deref(), Some("String"));
        assert_eq!(
            returns("# @return [Admin::Setting]").as_deref(),
            Some("Admin::Setting")
        );
        assert_eq!(
            returns("# @return [String, nil]").as_deref(),
            Some("String?")
        );
        assert_eq!(
            returns("# @return [nil, String]").as_deref(),
            Some("String?")
        );
        assert_eq!(returns("# @return [Boolean]").as_deref(), Some("bool"));
        assert_eq!(
            returns("# @return [Some_Thing]").as_deref(),
            Some("Some_Thing")
        );
        assert_eq!(returns("# @return [true, false]").as_deref(), Some("bool"));
        assert_eq!(
            returns("# @return [Array<String>] the names").as_deref(),
            Some("Array[String]")
        );
        assert_eq!(
            returns("# @return [Hash<Symbol, Array<String>>]").as_deref(),
            Some("Hash[Symbol, Array[String]]")
        );
        // The whole block above the `def` is read, and the *last* tag wins. That is YARD's rule,
        // and it matters when a doc comment restates a tag.
        assert_eq!(
            returns("# Does a thing.\n  #\n  # @param x [Integer] how many\n  # @return [String]")
                .as_deref(),
            Some("String")
        );
    }

    #[test]
    fn what_a_yard_tag_declares_nothing_about() {
        for annotation in [
            "# @return [#read]",
            "# @return [Hash{Symbol=>String}]",
            "# @return [String, Integer]",
            "# @return [nil]",
            "# @return [void]",
            "# @return [self]",
            "# @return [lowercase]",
            "# @return [Array<#read>]",
            "# @return [Array<]",
            "# @return [Foo::bar]",
            "# @return String",
            "# @return",
            "# just prose",
            "",
            // `Object` is YARD's "anything", and this crate already has one: RBS `untyped`, which
            // `Types::harvest` drops so a claim about nothing displaces no lower rung. `Object` is
            // a real class in the graph and would displace them.
            "# @return [Object]",
            "# @return [Object, nil]",
        ] {
            assert_eq!(returns(annotation), None, "{annotation:?}");
        }
    }

    #[test]
    fn a_comment_that_is_not_directly_above_the_def_is_somebody_elses() {
        // A blank line ends the block. That is YARD's rule, and why this reads the text, not
        // Prism's comment list: the question is about layout.
        assert_eq!(
            read(
                "class W\n  # @return [String]\n\n  def go\n  end\nend\n",
                "app/widget.rb"
            )
            .render(&declaring(&[]))
            .rbs,
            String::new()
        );
    }

    #[test]
    fn a_sig_binds_to_the_statement_that_follows_it_and_to_nothing_else() {
        // What Sorbet does. Anything in between breaks the binding instead of being skipped,
        // because misreading it is how a method gets its neighbour's type.
        assert_eq!(
            declared(
                "class W\n  \
                 sig { returns(String) }\n  \
                 CONSTANT = 1\n  \
                 def go\n  end\n\
                 end\n"
            ),
            String::new()
        );
        assert_eq!(
            declared(
                "class W\n  \
                 sig { returns(String) }\n  \
                 def first\n  end\n  \
                 def second\n  end\n\
                 end\n"
            ),
            "class W\n  def first: () -> String\nend\n"
        );
    }

    #[test]
    fn every_parameter_shape_ruby_has_and_the_rbs_it_becomes() {
        // Arity must be exact (an answer is partitioned by the *call's* positional argument count),
        // so this is pinned shape by shape.
        for (parameters, rendered) in [
            ("", "()"),
            ("()", "()"),
            ("(a)", "(untyped)"),
            ("(a, b = 1)", "(untyped, ?untyped)"),
            ("(a, *rest)", "(untyped, *untyped)"),
            ("(*rest, last)", "(*untyped, untyped)"),
            ("(key:)", "(key: untyped)"),
            ("(key: 1)", "(?key: untyped)"),
            ("(**kw)", "(**untyped)"),
            ("(**nil)", "()"),
            ("(&block)", "() ?{ (*untyped) -> untyped }"),
            ("((a, b))", "(untyped)"),
            ("(...)", "(*untyped)"),
        ] {
            let source =
                format!("class W\n  # @return [String]\n  def go{parameters}\n  end\nend\n");
            assert_eq!(
                declared(&source),
                format!("class W\n  def go: {rendered} -> String\nend\n"),
                "{parameters:?}"
            );
        }
    }

    #[test]
    fn a_parameter_that_was_given_a_type_keeps_it() {
        // Both annotations, and both YARD orders. An untyped parameter is `untyped`, not absent:
        // dropping it would change the arity.
        assert_eq!(
            declared(
                "class W\n  \
                 sig { params(a: Integer, c: T.nilable(String)).returns(String) }\n  \
                 def go(a, b, c: nil)\n  end\n\
                 end\n"
            ),
            "class W\n  def go: (Integer, untyped, ?c: String?) -> String\nend\n"
        );
        assert_eq!(
            declared(
                "class W\n  \
                 # @param a [Integer] how many\n  \
                 # @param [Symbol] b\n  \
                 # @param [#read] c\n  \
                 # @return [String]\n  \
                 def go(a, b, c)\n  end\n\
                 end\n"
            ),
            "class W\n  def go: (Integer, Symbol, untyped) -> String\nend\n"
        );
    }

    #[test]
    fn a_malformed_param_tag_is_read_past_rather_than_read_wrongly() {
        assert_eq!(
            declared(
                "class W\n  \
                 # @param\n  \
                 # @param a\n  \
                 # @param [Integer]\n  \
                 # @return [String]\n  \
                 def go(a)\n  end\n\
                 end\n"
            ),
            "class W\n  def go: (untyped) -> String\nend\n"
        );
    }

    #[test]
    fn a_sorbet_sig_wins_over_a_yard_tag_beside_it() {
        assert_eq!(
            declared(
                "class W\n  \
                 # @return [Integer]\n  \
                 sig { returns(String) }\n  \
                 def go\n  end\n\
                 end\n"
            ),
            "class W\n  def go: () -> String\nend\n"
        );
    }

    #[test]
    fn a_method_name_this_cannot_spell_in_rbs_is_left_alone() {
        // Ruby identifiers are Unicode and RBS identifiers are not, so both halves of the check are
        // reachable: a name not starting with an ASCII lower-case letter, and one that does and
        // then stops being ASCII.
        for name in ["==", "[]", "<=>", "+", "Capitalized", "имя", "go_имя"] {
            let source =
                format!("class W\n  # @return [String]\n  def {name}(other)\n  end\nend\n");
            assert_eq!(declared(&source), String::new(), "{name}");
        }
        // The three suffixes Ruby allows are fine, and so are ordinary name characters.
        assert_eq!(
            declared("class W\n  # @return [Boolean]\n  def ok?\n  end\nend\n"),
            "class W\n  def ok?: () -> bool\nend\n"
        );
        assert_eq!(
            declared("class W\n  # @return [String]\n  def _goX2\n  end\nend\n"),
            "class W\n  def _goX2: () -> String\nend\n"
        );
    }

    #[test]
    fn a_module_and_a_nested_class_are_spelled_the_way_rubydex_spells_them() {
        // `module Admin`, not `class Admin`, for the singleton half. A `def self.` in a module is
        // `Owner::ModuleSingleton`; declaring it as a class would say `Admin` is one and cost the
        // module whatever else is declared on it.
        assert_eq!(
            declared_by(
                "module Admin\n  \
                 # @return [String]\n  \
                 def self.label\n  end\n\n  \
                 class Panel\n    \
                 # @return [Integer]\n    \
                 def size\n    end\n  \
                 end\n\
                 end\n",
                &["Admin"]
            ),
            "module Admin\n  def self.label: () -> String\nend\n\
             module Admin\nclass Panel\n  def size: () -> Integer\nend\nend\n"
        );
        // The same member, from a file that writes the namespace into the name. Here nothing
        // declares `Admin` at all, so the name is written one body per segment: a joined
        // `class Admin::Panel` would introduce `Admin` itself and cost whatever else hangs off it.
        // A conjured segment gets `class`, as the joined name said.
        assert_eq!(
            declared_by(
                "class Admin::Panel\n  \
                 # @return [Integer]\n  \
                 def size\n  end\n\
                 end\n",
                &[]
            ),
            "class Admin::Panel\n  def size: () -> Integer\nend\n"
        );
        // The instance half of the same rule: a `def` in a module body is `Owner::Module`, and an
        // includer reaches it. That is what a concern is.
        assert_eq!(
            declared_by(
                "module Greetable\n  \
                 # @return [String]\n  \
                 def greeting\n  end\n\
                 end\n",
                &["Greetable"]
            ),
            "module Greetable\n  def greeting: () -> String\nend\n"
        );
    }

    #[test]
    fn a_file_with_nothing_annotated_in_it_declares_nothing() {
        assert!(read("class W\n  def go\n  end\nend\n", "app/widget.rb").is_empty());
        assert!(read("puts 'hello'\n", "app/widget.rb").is_empty());
        assert!(read("class W\nend\n", "app/widget.rb").is_empty());
        assert!(read("class W; end\n", "app/widget.rb").is_empty());
        // A `def` on the file's first line: nothing to walk back over for a comment block, the one
        // way that loop runs zero times.
        assert!(read("class W; def go; end; end\n", "app/widget.rb").is_empty());
    }

    #[test]
    fn a_sig_block_and_a_yard_tag_each_type_a_receiver_and_say_which_they_read() {
        // Both halves of an annotation. A card says which one it read, because a comment is not a
        // signature and the tiers already have a place to say so.
        let source = "Widget.new.name.upcase\nWidget.new.label.upcase\n";
        let (mut harness, _story, uri) = models_project(source);
        let widget = harness.write(
            "app/models/widget.rb",
            "class Widget\n  \
             extend T::Sig\n\n  \
             sig { returns(String) }\n  \
             def name\n    \"x\"\n  end\n\n  \
             # @return [String]\n  \
             def label\n    \"y\"\n  end\n\
             end\n",
        );
        harness.watch(&[&widget]);

        let sorbet = card(&mut harness, &uri, source, "name");
        assert!(sorbet.contains("a Sorbet `sig` block"), "{sorbet}");
        assert!(sorbet.contains("app/models/widget.rb"), "{sorbet}");

        let yard = card(&mut harness, &uri, source, "label");
        assert!(yard.contains("a YARD `@return` tag"), "{yard}");

        // Both type the chain, which is the only reason to read either.
        let chained = card(&mut harness, &uri, "Widget.new.name.upcase\n", "upcase");
        assert!(chained.contains("String#upcase"), "{chained}");
    }

    #[test]
    fn an_annotation_is_not_a_second_place_the_method_is_declared() {
        // The `def` is already in the graph at the offset an editor should jump to, so an
        // annotation adds a *type* and nothing else. A span here would list the same location twice
        // in go-to-definition.
        let source = "Widget.new.name\n";
        let (mut harness, _story, uri) = models_project(source);
        let widget = harness.write(
            "app/models/widget.rb",
            "class Widget\n  # @return [String]\n  def name\n    \"x\"\n  end\nend\n",
        );
        harness.watch(&[&widget]);

        let definition = harness.definition_at(&uri, source, "name");
        assert_eq!(definition.as_array().map(Vec::len), Some(1), "{definition}");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(widget.as_str()),
            "{definition}"
        );
    }

    #[test]
    fn a_sorbet_sig_wins_over_a_yard_tag_that_disagrees_with_it() {
        // Both mean "a human wrote the type down", and one is machine-checked: a `sig` is Ruby the
        // parser validates and `srb` checks. A comment rots quietly.
        let (mut harness, _story, _uri) = models_project("");
        let widget = harness.write(
            "app/models/widget.rb",
            "class Widget\n  \
             # @return [Integer]\n  \
             sig { returns(String) }\n  \
             def name\n    \"x\"\n  end\n\
             end\n",
        );
        harness.watch(&[&widget]);

        let rbs = harness.generated_rbs("app/models/widget.rb");
        assert!(rbs.contains("def name: () -> String"), "{rbs}");
    }

    #[test]
    fn an_annotation_naming_something_that_is_not_a_class_declares_nothing() {
        // The decline list, as behaviour: a union `Types` cannot key, a duck type naming a method,
        // and a shape. None has an exact spelling here, and a method this cannot spell exactly gets
        // nothing.
        let (mut harness, _story, _uri) = models_project("");
        let widget = harness.write(
            "app/models/widget.rb",
            "class Widget\n  \
             sig { returns(T.any(String, Integer)) }\n  \
             def either\n  end\n\n  \
             # @return [#read]\n  \
             def duck\n  end\n\n  \
             # @return [Hash{Symbol=>String}]\n  \
             def shape\n  end\n\n  \
             # @param count [Integer] how many\n  \
             def untagged(count)\n  end\n\
             end\n",
        );
        harness.watch(&[&widget]);

        assert_eq!(
            harness.generated_rbs("app/models/widget.rb"),
            String::new(),
            "a type this crate cannot spell exactly is a method it says nothing about"
        );
    }

    #[test]
    fn an_annotated_method_keeps_the_arity_it_was_written_with() {
        // An answer is partitioned by the *call's* positional argument count, so a wrong generated
        // arity does not merely display wrongly: it answers nothing, or answers for a call nobody
        // made. Every parameter shape Ruby has, rendered, then asked the one question that matters.
        let source = "Widget.new.go(1, 2, 3, key: 4).upcase\n";
        let (mut harness, _story, uri) = models_project(source);
        let widget = harness.write(
            "app/models/widget.rb",
            "class Widget\n  \
             # @return [String]\n  \
             def go(a, b = 1, *rest, key:, opt: 2, **kw, &block)\n  end\n\
             end\n",
        );
        harness.watch(&[&widget]);

        let rbs = harness.generated_rbs("app/models/widget.rb");
        assert!(
            rbs.contains(
                "def go: (untyped, ?untyped, *untyped, key: untyped, ?opt: untyped, **untyped) \
                 ?{ (*untyped) -> untyped } -> String"
            ),
            "{rbs}"
        );
        let card = card(&mut harness, &uri, source, "upcase");
        assert!(card.contains("String#upcase"), "{card}");
    }
}
