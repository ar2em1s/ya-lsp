//! Locale files: the keys `I18n.t` looks up in the project's main locale, and what each holds
//! (backlog 46).
//!
//! # One locale
//!
//! Only the main locale (`[i18n] locale`, `en` by default) is read. What other locales hold, and
//! whether they agree with it, is the project's business (ruled 2026-09-28): a key the main locale
//! lacks answers nothing on every surface.
//!
//! # Read as Ruby reads it
//!
//! i18n loads a `.yml` with Psych, which reads YAML 1.1: a plain `yes` is `true`, `12` an
//! `Integer`, `:other` a `Symbol` (a link to another key), `2024-01-01` a `Date`. [`scalar`] is
//! Psych's own scalar rule (`ScalarScanner#tokenize`), so a key is a `String` only where Psych
//! makes one. A `.rb` locale file is a Ruby hash literal ([`read_ruby`]): a string is a `String`,
//! a nested hash a tree, and anything else (a lambda, a constant) a value only Ruby knows.
//!
//! **A scanner, not a YAML parser**, like the `database.yml` and `structure.sql` readers: block
//! mappings and sequences by indentation, the three scalar styles and block scalars, flow
//! sequences and mappings, anchors, aliases and `<<:` merges, which is what locale files write.
//! Anything it cannot read is [`Held::Other`], which every surface declines.

use std::collections::{BTreeMap, HashMap};

/// Where a project's own locale files are, unless `[i18n] paths` says otherwise: every
/// `config/locales` directory, the application's and each engine's or plugin's it holds.
pub const DEFAULT_LOCALE_PATHS: [&str; 2] =
    ["**/config/locales/**/*.yml", "**/config/locales/**/*.rb"];

use ruby_prism::Node as Ruby;

/// The members that look a key up, by the declaration they are: `(owner, method, html-safe,
/// yields)`. The view's and a controller's `t` make an `_html` key's value html-safe
/// (`ActiveSupport::HtmlSafeTranslation`); `I18n.t` never does. The view's hands the translation
/// to a block where the call writes one, and returns the block's value.
pub const LOOKUPS: [(&str, &str, bool, bool); 8] = [
    ("I18n::Base", "t", false, false),
    ("I18n::Base", "translate", false, false),
    ("I18n::Base", "t!", false, false),
    ("I18n::Base", "translate!", false, false),
    ("AbstractController::Translation", "t", true, false),
    ("AbstractController::Translation", "translate", true, false),
    ("ActionView::Helpers::TranslationHelper", "t", true, true),
    (
        "ActionView::Helpers::TranslationHelper",
        "translate",
        true,
        true,
    ),
];

/// i18n's own returns, which its bodies do not say to a reader: `(owner, method, arm, more
/// arms)`, each arm `(parameters, returns)`, written as an overload set in this order, each
/// parameter named as the gems' `def`s name it (`l` is an `alias`, whose card reads only these). The current locale is a
/// `Symbol` (`Config#locale`); `with_locale` hands back its block's value; a model's human name is
/// a `String`, since it is looked up with `count: 1` and a default that is one.
///
/// `localize` formats a date or time with `strftime`, a `String`, except that a `nil` object with a
/// `default:` is that default, whatever it is: the first arm takes every call writing one, and
/// answers nothing. Rails' view and controller `localize` (and their `l`) hand `**options` on, which
/// reaches both arms, so they are rows of their own.
pub const RETURNS: [Returned; 9] = [
    ("I18n::Base", "locale", ("()", "Symbol"), &[]),
    ("I18n::Base", "localize", DEFAULTED, LOCALIZED),
    ("I18n::Base", "l", DEFAULTED, LOCALIZED),
    (
        "AbstractController::Translation",
        "localize",
        DEFAULTED,
        LOCALIZED,
    ),
    ("AbstractController::Translation", "l", DEFAULTED, LOCALIZED),
    (
        "ActionView::Helpers::TranslationHelper",
        "localize",
        DEFAULTED,
        LOCALIZED,
    ),
    (
        "ActionView::Helpers::TranslationHelper",
        "l",
        DEFAULTED,
        LOCALIZED,
    ),
    (
        "I18n::Base",
        "with_locale",
        ("[T] (?untyped tmp_locale) { () -> T }", "T"),
        &[],
    ),
    (
        "ActiveModel::Name",
        "human",
        ("(?untyped options)", "String"),
        &[],
    ),
];

/// A lookup's parameters, named as the gems' `def`s name them, since a card prints the names and
/// `t` is an `alias` whose card reads nothing else: `I18n.t(key = nil, **options)`, but
/// `translate!` and Rails' own `t(key, **options)` require the key.
#[must_use]
pub fn lookup_parameters(owner: &str, method: &str) -> &'static str {
    if owner == "I18n::Base" && matches!(method, "t" | "translate") {
        "(?untyped key, **untyped options)"
    } else {
        "(untyped key, **untyped options)"
    }
}

/// One row of [`RETURNS`]: the owner, the method, its first arm and the rest.
pub type Returned = (&'static str, &'static str, Arm, &'static [Arm]);

/// An overload's `(parameters, returns)`.
pub type Arm = (&'static str, &'static str);

/// `localize` with a `default:`: what a `nil` object answers, which may be anything.
const DEFAULTED: Arm = (
    "(untyped object, default: untyped, **untyped options)",
    "untyped",
);

/// `localize` otherwise: `strftime`'s `String`.
const LOCALIZED: &[Arm] = &[("(untyped object, **untyped options)", "String")];

/// The Rails libraries whose own locale files are first on the load path, and where each is.
pub const RAILS_OWN: [(&str, &str); 4] = [
    ("activesupport", "lib/active_support/locale"),
    ("activemodel", "lib/active_model/locale"),
    ("activerecord", "lib/active_record/locale"),
    ("actionview", "lib/action_view/locale"),
];

/// The namespaces above every owner in [`LOOKUPS`] and [`RETURNS`], which must be spellable for a
/// row to be written.
pub const NAMESPACES: [&str; 5] = [
    "I18n",
    "AbstractController",
    "ActionView",
    "ActionView::Helpers",
    "ActiveModel",
];

/// What a member that looks a key up does with it, by its declaration's name (`I18n::Base#t()`):
/// whether it makes an `_html` key html-safe, and whether it hands the translation to a block.
/// `None` for any other member.
#[must_use]
pub fn lookup(member: &str) -> Option<(bool, bool)> {
    let (owner, method) = member.rsplit_once('#')?;
    let method = method.trim_end_matches("()");
    LOOKUPS
        .iter()
        .find(|(held, name, _, _)| *held == owner && *name == method)
        .map(|(_, _, html_safe, yields)| (*html_safe, *yields))
}

/// What a key holds, as i18n reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Held {
    /// A `String`, and its text.
    Text(String),
    /// A mapping: a subtree of keys, a `Hash` with `Symbol` keys where it is looked up whole.
    Tree,
    /// A sequence, and whether every item is a `String`.
    List { texts: bool },
    /// Anything else: a number, a boolean, `nil`, a date, a `Symbol` (a link to another key), a
    /// value only Ruby knows, or YAML this reader does not follow.
    Other,
}

/// One key the main locale writes: what it holds, where its name is written, and the keys under
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub held: Held,
    /// Which file wrote it ([`Translations::files`]), the last to in load order.
    pub file: usize,
    /// The byte span of the key's name in that file.
    pub at: (u32, u32),
    pub children: BTreeMap<String, Entry>,
    /// A list's items, where every one is a `String` ([`Held::List`] `{ texts: true }`), for a card
    /// to show; empty otherwise.
    pub items: Vec<String>,
}

/// The plural categories CLDR names, which i18n picks among by `count:`.
const PLURALS: [&str; 6] = ["zero", "one", "two", "few", "many", "other"];

impl Entry {
    /// Whether this is a plural subtree: every key a plural category, every value a `String`.
    #[must_use]
    pub fn plural(&self) -> bool {
        self.held == Held::Tree
            && !self.children.is_empty()
            && self.children.iter().all(|(name, child)| {
                PLURALS.contains(&name.as_str()) && matches!(child.held, Held::Text(_))
            })
    }
}

/// How many lines of a key's YAML a card shows before it stops.
pub const SHOWN_LINES: usize = 10;

/// What the main locale holds under `key`, as the YAML a card shows: the key, then its value as
/// YAML writes it, a subtree nested, cut after [`SHOWN_LINES`] lines with `# …`.
///
/// **Rendered from what was read, not copied from a file**: a subtree two files merge is shown
/// whole, and an alias as what it names. A value this reader keeps no text of (a number, a boolean,
/// a `Symbol`, a list holding anything but strings) is a YAML comment saying what it is, so it can
/// never read as a value it is not.
#[must_use]
pub fn yaml(key: &str, entry: &Entry) -> String {
    let mut lines = Vec::new();
    write_yaml(key, entry, 0, &mut lines);
    if lines.len() > SHOWN_LINES {
        lines.truncate(SHOWN_LINES);
        lines.push("# …".to_owned());
    }
    lines.join("\n")
}

/// One key and what it holds, `depth` levels in. Stops once more lines are written than a card
/// shows, so a key holding a whole locale costs [`SHOWN_LINES`], not its size.
fn write_yaml(key: &str, entry: &Entry, depth: usize, lines: &mut Vec<String>) {
    if lines.len() > SHOWN_LINES {
        return;
    }
    let indent = "  ".repeat(depth);
    match &entry.held {
        Held::Text(text) if text.contains('\n') => {
            lines.push(format!("{indent}{key}: |"));
            lines.extend(text.lines().map(|line| format!("{indent}  {line}")));
        }
        Held::Text(text) => lines.push(format!("{indent}{key}: {}", as_scalar(text))),
        Held::Tree if entry.children.is_empty() => lines.push(format!("{indent}{key}: {{}}")),
        Held::Tree => {
            lines.push(format!("{indent}{key}:"));
            for (name, child) in &entry.children {
                write_yaml(name, child, depth + 1, lines);
            }
        }
        Held::List { texts: true } if entry.items.is_empty() => {
            lines.push(format!("{indent}{key}: []"));
        }
        Held::List { texts: true } => {
            lines.push(format!("{indent}{key}:"));
            lines.extend(
                entry
                    .items
                    .iter()
                    .map(|item| format!("{indent}  - {}", as_scalar(item))),
            );
        }
        Held::List { texts: false } => lines.push(format!("{indent}{key}: # a list")),
        Held::Other => lines.push(format!("{indent}{key}: # not a string")),
    }
}

/// A `String` as YAML writes one: bare where the reader above would read it back as that very
/// `String` ([`scalar`]), double-quoted otherwise, as `%{count} stories`, `yes` and `12` must be.
fn as_scalar(text: &str) -> String {
    let hazards = [
        text.starts_with([
            ' ', '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"',
            '%', '@', '`',
        ]),
        text.ends_with([' ', ':']),
        text.contains(": "),
        text.contains(" #"),
        text.contains(['\n', '\t', '\r']),
    ];
    // What survives the hazards has no whitespace for `scalar` to trim, so a `Text` it reads back
    // is this very string.
    if hazards.contains(&true) || !matches!(scalar(text), Node::Text(_)) {
        format!("{text:?}")
    } else {
        text.to_owned()
    }
}

/// The locales a `.yml` file writes, by its top-level keys, read without parsing it: only a file
/// that writes the main locale is parsed ([`read_locale`]).
#[must_use]
pub fn locales_in(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in source.lines() {
        let Some(first) = line.chars().next() else {
            continue;
        };
        if first.is_whitespace() || matches!(first, '#' | '-' | '.' | '%' | '{' | '}') {
            continue;
        }
        if let Some((name, _)) = split_key(line) {
            found.push(name);
        }
    }
    found
}

/// The main locale's keys in one `.yml` file, or `None` where the file writes none of it.
#[must_use]
pub fn read_locale(source: &str, locale: &str, file: usize) -> Option<BTreeMap<String, Entry>> {
    let mut reader = Reader::new(source);
    let Node::Map(top) = reader.block(0) else {
        return None;
    };
    top.into_iter()
        .rev()
        .find(|(name, _)| name.text == locale)
        .map(|(_, node)| children_of(node, file))
}

/// The main locale's keys in one `.rb` locale file: the hash literal it evaluates to. A locale
/// written twice is the last, as Ruby builds the hash.
#[must_use]
pub fn read_ruby(source: &str, locale: &str, file: usize) -> Option<BTreeMap<String, Entry>> {
    ruby_locales(source, Some((locale, file)))
        .into_iter()
        .rev()
        .find_map(|(name, children)| (name == locale).then_some(children))
}

/// The locales a `.rb` file writes: the keys of the hash literal it evaluates to, which i18n's
/// `load_rb` takes as the file's value.
#[must_use]
pub fn ruby_locales_in(source: &str) -> Vec<String> {
    ruby_locales(source, None)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Each top-level key of a `.rb` locale file, in written order, with the keys under it where it is
/// the locale `reading` names (read with that file index), and none under any other.
///
/// Not generic on purpose: coverage merges a generic's instantiations by the best one, not by the
/// union of the arms each takes.
fn ruby_locales(
    source: &str,
    reading: Option<(&str, usize)>,
) -> Vec<(String, BTreeMap<String, Entry>)> {
    let parsed = ruby_prism::parse(source.as_bytes());
    let Some(top) = parsed
        .node()
        .as_program_node()
        .and_then(|program| program.statements().body().iter().last())
        .and_then(|last| last.as_hash_node())
    else {
        return Vec::new();
    };
    top.elements()
        .iter()
        .filter_map(|element| {
            let pair = element.as_assoc_node()?;
            let (name, _) = ruby_key(&pair.key())?;
            let children = match reading {
                Some((locale, file)) if locale == name => ruby_children(&pair.value(), file),
                _ => BTreeMap::new(),
            };
            Some((name, children))
        })
        .collect()
}

/// A Ruby hash's key, as i18n files it: a symbol or a string, and where it is written.
fn ruby_key(key: &Ruby<'_>) -> Option<(String, (u32, u32))> {
    let (text, at) = if let Some(symbol) = key.as_symbol_node() {
        (symbol.unescaped().to_vec(), symbol.value_loc()?)
    } else {
        let string = key.as_string_node()?;
        (string.unescaped().to_vec(), string.content_loc())
    };
    Some((
        String::from_utf8(text).ok()?,
        (at.start_offset() as u32, at.end_offset() as u32),
    ))
}

/// What a Ruby locale hash holds under one key.
fn ruby_children(value: &Ruby<'_>, file: usize) -> BTreeMap<String, Entry> {
    let mut children = BTreeMap::new();
    let Some(hash) = value.as_hash_node() else {
        return children;
    };
    for element in hash.elements().iter() {
        let Some(pair) = element.as_assoc_node() else {
            continue;
        };
        let Some((name, at)) = ruby_key(&pair.key()) else {
            continue;
        };
        let value = pair.value();
        let (held, under) = if let Some(string) = value.as_string_node() {
            (
                Held::Text(String::from_utf8_lossy(string.unescaped()).into_owned()),
                BTreeMap::new(),
            )
        } else if value.as_hash_node().is_some() {
            (Held::Tree, ruby_children(&value, file))
        } else {
            (Held::Other, BTreeMap::new())
        };
        children.insert(
            name,
            Entry {
                held,
                file,
                at,
                children: under,
                items: Vec::new(),
            },
        );
    }
    children
}

/// Every file's keys, merged as i18n merges its load path: a later file's key replaces an
/// earlier one's, and two subtrees merge key by key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Translations {
    /// Each file read, by its index in [`Entry::file`], as a graph URI.
    pub files: Vec<String>,
    pub root: BTreeMap<String, Entry>,
}

impl Translations {
    /// Merge one file's keys, read with `file` as its index, over everything read before.
    pub fn merge(&mut self, keys: BTreeMap<String, Entry>) {
        merge_into(&mut self.root, keys);
    }

    /// What `key` names, a dotted path under the locale.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Entry> {
        let mut parts = key.split('.');
        let mut entry = self.root.get(parts.next()?)?;
        for part in parts {
            entry = entry.children.get(part)?;
        }
        Some(entry)
    }

    /// The keys directly under `prefix` (`""` for the top), for completion.
    #[must_use]
    pub fn under(&self, prefix: &str) -> Option<&BTreeMap<String, Entry>> {
        if prefix.is_empty() {
            return Some(&self.root);
        }
        Some(&self.get(prefix)?.children)
    }
}

/// `from` over `into`, key by key.
fn merge_into(into: &mut BTreeMap<String, Entry>, from: BTreeMap<String, Entry>) {
    for (name, entry) in from {
        match into.get_mut(&name) {
            Some(held) if held.held == Held::Tree && entry.held == Held::Tree => {
                held.file = entry.file;
                held.at = entry.at;
                merge_into(&mut held.children, entry.children);
            }
            _ => {
                into.insert(name, entry);
            }
        }
    }
}

/// A key as a call to `t` writes it, and what else the call says: what [`Translations::returns`]
/// decides a type from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    /// The key, as written: dotted, without the locale.
    pub key: String,
    /// The `scope:` written as literals, outermost first, prepended to the key.
    pub scope: Vec<String>,
    /// Whether the call passes `count:`.
    pub count: bool,
    /// Whether the call passes a `default:` that is not a `String` literal: a `Symbol` is another
    /// lookup, and anything else is whatever it is.
    pub other_default: bool,
    /// Whether the view's or a controller's `t` answers, which makes an `_html` key's `String` an
    /// `ActiveSupport::SafeBuffer` (`ActiveSupport::HtmlSafeTranslation`). `I18n.t` never does.
    pub html_safe: bool,
}

impl Asked {
    /// The whole key, scope first.
    #[must_use]
    pub fn path(&self) -> String {
        let mut parts = self.scope.clone();
        parts.push(self.key.clone());
        parts.join(".")
    }
}

impl Translations {
    /// The RBS type of what a call to `t` hands back, where the main locale says so for certain.
    ///
    /// - **A `String`**, or a plural subtree the call passes `count:` to, is `String`; an `_html`
    ///   key's through a view's or a controller's `t` is `ActiveSupport::SafeBuffer`.
    /// - **A subtree** is `Hash[Symbol, untyped]`: i18n hands the whole tree back. Through the
    ///   html-safe `t` an `_html` subtree is changed into something else, and `count:` picks one of
    ///   its keys (or raises), so neither answers.
    /// - **A sequence** is an `Array`, of `String` where every item is one.
    /// - **Anything else answers nothing**: a key no file read writes (another file may), a number,
    ///   a `Symbol` link, a value only Ruby knows, or a `default:` that is not a `String`.
    #[must_use]
    pub fn returns(&self, asked: &Asked) -> Option<&'static str> {
        if asked.other_default {
            return None;
        }
        let path = asked.path();
        let entry = self.get(&path)?;
        let html = asked.html_safe
            && path
                .rsplit('.')
                .next()
                .is_some_and(|last| last == "html" || last.ends_with("_html"));
        match &entry.held {
            Held::Text(_) if html => Some("ActiveSupport::SafeBuffer"),
            Held::Text(_) => Some("String"),
            Held::Tree if asked.count && entry.plural() => Some(if html {
                "ActiveSupport::SafeBuffer"
            } else {
                "String"
            }),
            // `count:` on a subtree that is not a plural picks a key by the count, or raises.
            Held::Tree if html || asked.count => None,
            Held::Tree => Some("Hash[Symbol, untyped]"),
            Held::List { texts: true } => Some("Array[String]"),
            Held::List { texts: false } => Some("Array[untyped]"),
            Held::Other => None,
        }
    }
}

// ---------------------------------------------------------------------------
// The scanner
// ---------------------------------------------------------------------------

/// A YAML value, as far as a locale needs it.
#[derive(Debug, Clone)]
enum Node {
    /// A mapping, in written order, each key with where its name is.
    Map(Vec<(Name, Node)>),
    Seq(Vec<Node>),
    /// A `String`.
    Text(String),
    /// Anything that is not a `String`, a mapping or a sequence, or that was not read.
    Other,
}

/// A mapping's key and where its text is.
#[derive(Debug, Clone)]
struct Name {
    text: String,
    at: (u32, u32),
}

/// A mapping's values as [`Entry`]s, or none for any other node.
fn children_of(node: Node, file: usize) -> BTreeMap<String, Entry> {
    let mut children = BTreeMap::new();
    if let Node::Map(entries) = node {
        for (name, value) in entries {
            // A later key of the same name replaces an earlier one, as Psych reads a mapping.
            children.insert(name.text, entry_of(value, name.at, file));
        }
    }
    children
}

fn entry_of(node: Node, at: (u32, u32), file: usize) -> Entry {
    let mut items = Vec::new();
    let (held, children) = match node {
        Node::Text(text) => (Held::Text(text), BTreeMap::new()),
        Node::Map(_) => (Held::Tree, children_of(node, file)),
        Node::Seq(written) => {
            let texts: Option<Vec<String>> = written
                .into_iter()
                .map(|item| match item {
                    Node::Text(text) => Some(text),
                    _ => None,
                })
                .collect();
            let whole = texts.is_some();
            items = texts.unwrap_or_default();
            (Held::List { texts: whole }, BTreeMap::new())
        }
        Node::Other => (Held::Other, BTreeMap::new()),
    };
    Entry {
        held,
        file,
        at,
        children,
        items,
    }
}

/// One line of the file: where it starts, its indentation, and what follows it.
#[derive(Debug, Clone, Copy)]
struct Line<'s> {
    start: u32,
    indent: usize,
    text: &'s str,
}

struct Reader<'s> {
    lines: Vec<Line<'s>>,
    at: usize,
    anchors: HashMap<String, Node>,
}

impl<'s> Reader<'s> {
    fn new(source: &'s str) -> Self {
        let mut lines = Vec::new();
        let mut start = 0_u32;
        for raw in source.split_inclusive('\n') {
            let text = raw.trim_end_matches(['\n', '\r']);
            let content = text.trim_start_matches(' ');
            lines.push(Line {
                start,
                indent: text.len() - content.len(),
                text: content,
            });
            start += raw.len() as u32;
        }
        Self {
            lines,
            at: 0,
            anchors: HashMap::new(),
        }
    }

    /// The next line that holds something, skipping blank lines, comments and document markers.
    fn peek(&mut self) -> Option<Line<'s>> {
        while let Some(line) = self.lines.get(self.at) {
            let text = line.text;
            if text.is_empty()
                || text.starts_with('#')
                || text == "---"
                || text == "..."
                || text.starts_with("--- ")
                || text.starts_with('%')
            {
                self.at += 1;
                continue;
            }
            return Some(*line);
        }
        None
    }

    /// The block value that starts at the next line, if it is indented at least `least`.
    fn block(&mut self, least: usize) -> Node {
        match self.peek() {
            Some(line) if line.indent >= least => {
                if is_item(line.text) {
                    self.sequence(line.indent)
                } else {
                    self.mapping(line.indent)
                }
            }
            _ => Node::Other,
        }
    }

    /// The mapping whose keys are written at `indent`.
    fn mapping(&mut self, indent: usize) -> Node {
        let mut entries: Vec<(Name, Node)> = Vec::new();
        let mut merged: Vec<Node> = Vec::new();
        while let Some(line) = self.peek() {
            if line.indent < indent || (line.indent == indent && is_item(line.text)) {
                break;
            }
            self.at += 1;
            if line.indent > indent {
                // A line deeper than any key that opened it: skipped rather than guessed at.
                continue;
            }
            let Some((name, rest)) = keyed(line.text) else {
                continue;
            };
            let key_at = line.start + line.indent as u32;
            let at = (key_at, key_at + key_width(line.text));
            let value = self.value(rest, indent, true);
            let Some(name) = name else {
                // `yes:` is `true` to Psych: a key no dotted path names, read to pass its value.
                continue;
            };
            if name == "<<" {
                merged.push(value);
                continue;
            }
            entries.push((Name { text: name, at }, value));
        }
        // `<<: *base` (or a list of them): the anchored keys a mapping's own do not already write.
        for merge in merged {
            let maps = match merge {
                Node::Seq(items) => items,
                other => vec![other],
            };
            for map in maps {
                if let Node::Map(from) = map {
                    for (name, value) in from {
                        if !entries.iter().any(|(held, _)| held.text == name.text) {
                            entries.push((name, value));
                        }
                    }
                }
            }
        }
        Node::Map(entries)
    }

    /// The sequence whose `-` items are written at `indent`.
    fn sequence(&mut self, indent: usize) -> Node {
        let mut items = Vec::new();
        while let Some(line) = self.peek() {
            if line.indent != indent || !is_item(line.text) {
                break;
            }
            let rest = line.text[1..].trim_start_matches(' ');
            let column = line.indent + line.text.len() - rest.len();
            if rest.is_empty() {
                self.at += 1;
                items.push(self.block(indent + 1));
            } else if keyed(rest).is_some() {
                // `- key: value` (`- "key": value` too): a mapping whose first key is on the item's
                // own line. The line is read again as that key, at the column it is written at.
                self.lines[self.at] = Line {
                    start: line.start,
                    indent: column,
                    text: rest,
                };
                items.push(self.mapping(column));
            } else {
                self.at += 1;
                items.push(self.value(rest, indent, false));
            }
        }
        Node::Seq(items)
    }

    /// The value written after a key or a `-`, `rest` being the rest of its line. `indent` is the
    /// key's; `after_key` says a key wrote it, whose value may be a
    /// sequence at the key's own indentation.
    fn value(&mut self, rest: &'s str, indent: usize, after_key: bool) -> Node {
        let rest = rest.trim();
        if let Some(named) = rest.strip_prefix('&') {
            let (name, value) = named.split_once([' ', '\t']).unwrap_or((named, ""));
            let node = self.value(value, indent, after_key);
            self.anchors.insert(name.to_owned(), node.clone());
            return node;
        }
        if let Some(alias) = rest.strip_prefix('*') {
            return self
                .anchors
                .get(uncommented(alias).trim())
                .cloned()
                .unwrap_or(Node::Other);
        }
        if let Some(tagged) = rest.strip_prefix('!') {
            // `!!str` is a string whatever it looks like; any other tag is a class Psych builds.
            let (tag, value) = tagged.split_once(' ').unwrap_or((tagged, ""));
            let node = self.value(value, indent, after_key);
            // `!!str` and the bare non-specific `!` make a scalar a string whatever it looks like.
            return match (tag, node) {
                ("!str" | "", Node::Text(text)) => Node::Text(text),
                ("!str" | "", Node::Other) if !value.trim().is_empty() => {
                    Node::Text(uncommented(value).trim().to_owned())
                }
                _ => Node::Other,
            };
        }
        let rest = uncommented_value(rest);
        if rest.is_empty() {
            return match self.peek() {
                // A value written on the line below its key that is not a block of keys or items: a
                // flow collection, a quoted scalar, and the like.
                Some(line)
                    if line.indent > indent
                        && !is_item(line.text)
                        && keyed(line.text).is_none() =>
                {
                    self.at += 1;
                    self.value(line.text, line.indent.saturating_sub(1), false)
                }
                Some(line) if line.indent > indent => self.block(indent + 1),
                Some(line) if after_key && line.indent == indent && is_item(line.text) => {
                    self.sequence(indent)
                }
                // A key with nothing under it is `nil`.
                _ => Node::Other,
            };
        }
        match rest.as_bytes()[0] {
            b'|' | b'>' => self.block_scalar(rest, indent),
            b'"' | b'\'' => self.quoted(rest, indent),
            b'[' | b'{' => self.flow(rest, indent),
            _ => self.plain(rest, indent),
        }
    }

    /// A `|` or `>` block scalar: every following line deeper than the key, or blank, kept as YAML
    /// keeps it. `header` is the indicator (`|`, `>-`, `|+`): literal or folded, and whether the
    /// last line break is clipped to one, stripped or kept.
    fn block_scalar(&mut self, header: &str, indent: usize) -> Node {
        let folded = header.starts_with('>');
        let mut lines: Vec<(usize, &str)> = Vec::new();
        while let Some(line) = self.lines.get(self.at) {
            if !line.text.is_empty() && line.indent <= indent {
                break;
            }
            self.at += 1;
            lines.push((line.indent, line.text));
        }
        let base = lines
            .iter()
            .filter(|(_, text)| !text.is_empty())
            .map(|(indent, _)| *indent)
            .min()
            .unwrap_or(0);
        let mut text = String::new();
        // In a folded scalar, a line break between two lines at the base indentation is a space;
        // an empty line, or one indented further, keeps its break.
        let mut previous_plain = false;
        for (at, (indent, line)) in lines.iter().enumerate() {
            let extra = indent.saturating_sub(base);
            let plain = !line.is_empty() && extra == 0;
            if at > 0 {
                if folded && plain && previous_plain {
                    text.push(' ');
                } else if !(folded && line.is_empty() && previous_plain) {
                    text.push('\n');
                }
            }
            if !line.is_empty() {
                text.push_str(&" ".repeat(extra));
                text.push_str(line);
            }
            previous_plain = plain;
        }
        let body = text.trim_end_matches('\n');
        let breaks = text.len() - body.len();
        let text = if header.contains('-') {
            body.to_owned()
        } else if header.contains('+') {
            format!("{body}{}", "\n".repeat(breaks + 1))
        } else if body.is_empty() {
            String::new()
        } else {
            format!("{body}\n")
        };
        Node::Text(text)
    }

    /// A quoted scalar, which may go on over the lines below it: always a `String`. A line break is
    /// a space, an empty line a break, and in double quotes a `\` before the break joins the lines.
    fn quoted(&mut self, first: &str, indent: usize) -> Node {
        let quote = first.as_bytes()[0];
        let mut text = first[1..].to_owned();
        let mut breaks = 0_usize;
        loop {
            if let Some(end) = closing(&text, quote) {
                text.truncate(end);
                return Node::Text(unquote(&text, quote));
            }
            // Folded onto the next line, which Psych (libyaml) takes at the key's own indentation too.
            match self.lines.get(self.at) {
                Some(line) if line.text.is_empty() => {
                    self.at += 1;
                    breaks += 1;
                }
                Some(line) if line.indent >= indent => {
                    self.at += 1;
                    let trimmed = text.trim_end_matches([' ', '\t']).len();
                    text.truncate(trimmed);
                    let escaped =
                        quote == b'"' && (text.len() - text.trim_end_matches('\\').len()) % 2 == 1;
                    if escaped {
                        text.pop();
                    } else if breaks > 0 {
                        text.push_str(&"\n".repeat(breaks));
                    } else {
                        text.push(' ');
                    }
                    breaks = 0;
                    text.push_str(line.text.trim_end());
                }
                _ => return Node::Other,
            }
        }
    }

    /// A flow `[…]` or `{…}`, which may go on over the lines below it.
    fn flow(&mut self, first: &str, indent: usize) -> Node {
        let mut text = first.to_owned();
        while !balanced(&text) {
            match self.lines.get(self.at) {
                Some(line) if line.text.is_empty() || line.indent > indent => {
                    self.at += 1;
                    text.push(' ');
                    text.push_str(uncommented(line.text));
                }
                _ => return Node::Other,
            }
        }
        flow_node(text.trim(), &self.anchors)
    }

    /// A plain scalar, which may go on over the deeper lines below it, as Psych resolves it.
    fn plain(&mut self, first: &str, indent: usize) -> Node {
        let mut text = first.to_owned();
        while let Some(line) = self.lines.get(self.at) {
            if line.text.is_empty()
                || line.indent <= indent
                || line.text.starts_with('#')
                || keyed(line.text).is_some()
            {
                break;
            }
            self.at += 1;
            text.push(' ');
            text.push_str(uncommented_value(line.text));
        }
        scalar(&text)
    }
}

/// Whether a line is a sequence item: `-` alone, or followed by a space.
fn is_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

/// A line's key and the text after its `:`, where the line is `key: value` or `key:`.
///
/// A key Psych reads as something other than a string (`yes:`, `12:`) is not one a dotted path can
/// name: [`keyed`] is `None` for it, though the line is still a key.
fn split_key(text: &str) -> Option<(String, &str)> {
    let (name, rest) = keyed(text)?;
    Some((name?, rest))
}

/// [`split_key`] with the key's name `None` where Psych reads it as something other than a string.
///
/// A quoted key is read to its closing quote. A plain key ends at the first `: ` or a `:` that ends
/// the line.
fn keyed(text: &str) -> Option<(Option<String>, &str)> {
    let first = *text.as_bytes().first()?;
    if first == b'"' || first == b'\'' {
        let end = closing(&text[1..], first)? + 1;
        let rest = text[end + 1..].trim_start().strip_prefix(':')?;
        if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
            return None;
        }
        return Some((Some(unquote(&text[1..end], first)), rest));
    }
    if matches!(
        first,
        b'-' | b'[' | b'{' | b'#' | b'&' | b'*' | b'!' | b'|' | b'>' | b'?'
    ) {
        return None;
    }
    let split = text
        .char_indices()
        .find(|(at, character)| {
            *character == ':'
                && text[at + 1..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
        })
        .map(|(at, _)| at)?;
    let key = text[..split].trim_end();
    if key == "<<" {
        return Some((Some(key.to_owned()), &text[split + 1..]));
    }
    let name = match scalar(key) {
        Node::Text(name) => Some(name),
        _ => None,
    };
    Some((name, &text[split + 1..]))
}

/// How many bytes the key's own text takes on its line: the name a jump selects.
fn key_width(text: &str) -> u32 {
    let first = text.as_bytes()[0];
    if first == b'"' || first == b'\'' {
        return closing(&text[1..], first).map_or(0, |end| end as u32 + 2);
    }
    text.find(": ")
        .or_else(|| text.strip_suffix(':').map(str::len))
        .map_or(0, |end| text[..end].trim_end().len() as u32)
}

/// Where a quoted scalar's text closes, `text` starting past the opening quote.
fn closing(text: &str, quote: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' if quote == b'"' => at += 2,
            b'\'' if quote == b'\'' && bytes.get(at + 1) == Some(&b'\'') => at += 2,
            byte if byte == quote => return Some(at),
            _ => at += 1,
        }
    }
    None
}

/// A quoted scalar's text, its escapes undone.
fn unquote(text: &str, quote: u8) -> String {
    if quote == b'\'' {
        return text.replace("''", "'");
    }
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        // YAML's escapes, and a code point written in hex.
        let hex = |characters: &mut std::str::Chars<'_>, width: usize| {
            let digits: String = characters.by_ref().take(width).collect();
            u32::from_str_radix(&digits, 16)
                .ok()
                .and_then(char::from_u32)
        };
        let escaped = match characters.next() {
            Some('n') => Some('\n'),
            Some('t') => Some('\t'),
            Some('r') => Some('\r'),
            Some('0') => Some('\0'),
            Some('a') => Some('\u{7}'),
            Some('b') => Some('\u{8}'),
            Some('e') => Some('\u{1b}'),
            Some('f') => Some('\u{c}'),
            Some('v') => Some('\u{b}'),
            Some('_') => Some('\u{a0}'),
            Some('N') => Some('\u{85}'),
            Some('L') => Some('\u{2028}'),
            Some('P') => Some('\u{2029}'),
            Some('x') => hex(&mut characters, 2),
            Some('u') => hex(&mut characters, 4),
            Some('U') => hex(&mut characters, 8),
            other => other,
        };
        out.extend(escaped);
    }
    out
}

/// A line's text before a comment: a `#` that starts it, or follows a space.
fn uncommented(text: &str) -> &str {
    if text.starts_with('#') {
        return "";
    }
    text.find(" #").map_or(text, |at| &text[..at])
}

/// [`uncommented`] for a value that may be quoted, whose `#` inside the quotes is text.
fn uncommented_value(text: &str) -> &str {
    match text.as_bytes().first() {
        Some(b'"' | b'\'') => text,
        _ => uncommented(text).trim_end(),
    }
}

/// Whether every bracket a flow value opens is closed, outside quotes.
fn balanced(text: &str) -> bool {
    let mut depth = 0_i32;
    let mut quote: Option<char> = None;
    for character in text.chars() {
        match (quote, character) {
            (Some(open), close) if open == close => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(character),
            (None, '[' | '{') => depth += 1,
            (None, ']' | '}') => depth -= 1,
            _ => {}
        }
    }
    depth <= 0
}

/// A flow sequence's items or a flow mapping's pairs.
fn flow_node(text: &str, anchors: &HashMap<String, Node>) -> Node {
    let (open, inner) = (text.as_bytes()[0], &text[1..]);
    let inner = inner.trim_end().strip_suffix([']', '}']).unwrap_or(inner);
    let items = split_flow(inner);
    if open == b'[' {
        return Node::Seq(
            items
                .iter()
                .map(|item| flow_scalar(item, anchors))
                .collect(),
        );
    }
    let mut entries = Vec::new();
    for item in items {
        let Some((name, rest)) = split_key(item) else {
            return Node::Other;
        };
        entries.push((
            Name {
                text: name,
                at: (0, 0),
            },
            flow_scalar(rest.trim(), anchors),
        ));
    }
    Node::Map(entries)
}

/// One item of a flow collection: a quoted or plain scalar or an alias, and anything nested is
/// not read.
fn flow_scalar(item: &str, anchors: &HashMap<String, Node>) -> Node {
    if let Some(alias) = item.strip_prefix('*') {
        return anchors.get(alias).cloned().unwrap_or(Node::Other);
    }
    match item.as_bytes().first() {
        Some(quote @ (b'"' | b'\'')) => closing(&item[1..], *quote).map_or(Node::Other, |end| {
            Node::Text(unquote(&item[1..=end], *quote))
        }),
        Some(b'[' | b'{') => Node::Other,
        _ => scalar(item),
    }
}

/// A flow collection's items, split at its top-level commas.
fn split_flow(inner: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let (mut depth, mut quote, mut start) = (0_i32, None::<char>, 0);
    for (at, character) in inner.char_indices() {
        match (quote, character) {
            (Some(open), close) if open == close => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(character),
            (None, '[' | '{') => depth += 1,
            (None, ']' | '}') => depth -= 1,
            (None, ',') if depth == 0 => {
                items.push(inner[start..at].trim());
                start = at + 1;
            }
            _ => {}
        }
    }
    let last = inner[start..].trim();
    if !last.is_empty() {
        items.push(last);
    }
    items
}

/// A plain scalar as Psych resolves it (`Psych::ScalarScanner#tokenize`): a `String`, or
/// [`Node::Other`] for `nil`, a boolean, a number, a date or time, and a `Symbol`.
fn scalar(text: &str) -> Node {
    let text = text.trim();
    if text.is_empty() {
        return Node::Other;
    }
    // Psych's first test: text that starts like a word is a `String`, unless it is short and one
    // of YAML 1.1's null and boolean words.
    if starts_like_a_word(text) {
        let lower = text.to_ascii_lowercase();
        let special = text.len() <= 5
            && matches!(
                lower.as_str(),
                "~" | "null" | "yes" | "true" | "on" | "no" | "false" | "off"
            );
        return if special {
            Node::Other
        } else {
            Node::Text(text.to_owned())
        };
    }
    if is_time_or_number(text) {
        Node::Other
    } else {
        Node::Text(text.to_owned())
    }
}

/// `^[^\d.:-]?[[:alpha:]_\s!@#$%^&*(){}<>|/\\~;=]+`, Psych's test for a word. Its other test, a
/// line break, never holds here: a plain scalar's lines are joined with spaces before it is read.
fn starts_like_a_word(text: &str) -> bool {
    let wordish = |character: char| {
        character.is_alphabetic()
            || character.is_whitespace()
            || "_!@#$%^&*(){}<>|/\\~;=".contains(character)
    };
    let mut characters = text.chars();
    let first = characters.next();
    let second = characters.next();
    first.is_some_and(wordish)
        || (first.is_some_and(|character| {
            !character.is_ascii_digit() && !matches!(character, '.' | ':' | '-')
        }) && second.is_some_and(wordish))
}

/// Whether Psych reads the text as a time, a date, an infinity or a `NaN`, a `Symbol`, a
/// sexagesimal, a float or an integer: its `tokenize` past the word test, pattern by pattern.
fn is_time_or_number(text: &str) -> bool {
    let signed = text.strip_prefix(['+', '-']).unwrap_or(text);
    let lower = signed.to_ascii_lowercase();
    (text.len() > 1 && text.starts_with(':'))
        || is_time(text)
        || matches!(lower.as_str(), ".inf" | ".nan")
        || is_sexagesimal(signed)
        || is_float(signed)
        || is_integer(signed)
}

/// `^-?\d{4}-\d{1,2}-\d{1,2}`, a date and, after a `T` or spaces, whatever a time adds: Psych
/// makes a `Date` or a `Time` of it, or keeps the `String` where it will not parse, which this
/// does not follow.
fn is_time(text: &str) -> bool {
    let text = text.strip_prefix('-').unwrap_or(text);
    let bytes = text.as_bytes();
    let digits = |from: usize, least: usize, most: usize| {
        let run = bytes[from.min(bytes.len())..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        (least..=most).contains(&run).then_some(from + run)
    };
    let Some(year) = digits(0, 4, 4) else {
        return false;
    };
    let Some(month) = (bytes.get(year) == Some(&b'-'))
        .then(|| digits(year + 1, 1, 2))
        .flatten()
    else {
        return false;
    };
    (bytes.get(month) == Some(&b'-')) && digits(month + 1, 1, 2).is_some()
}

/// `[0-9][0-9_]*(:[0-5]?[0-9]){1,2}`, with an optional `.` fraction: a base-60 number.
fn is_sexagesimal(text: &str) -> bool {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let mut parts = whole.split(':');
    let head = parts.next().unwrap_or_default();
    let tail: Vec<&str> = parts.collect();
    head.starts_with(|c: char| c.is_ascii_digit())
        && head.chars().all(|c| c.is_ascii_digit() || c == '_')
        && (1..=2).contains(&tail.len())
        && tail.iter().all(|part| {
            (1..=2).contains(&part.len())
                && part.chars().all(|c| c.is_ascii_digit())
                && (part.len() == 1 || part.as_bytes()[0] <= b'5')
        })
        && fraction.chars().all(|c| c.is_ascii_digit() || c == '_')
}

/// `([0-9][0-9_,]*)?\.[0-9]*([eE][-+][0-9]+)?`, the sign already taken off.
fn is_float(text: &str) -> bool {
    let Some((whole, rest)) = text.split_once('.') else {
        return false;
    };
    let (fraction, exponent) = match rest.find(['e', 'E']) {
        Some(at) => (&rest[..at], Some(&rest[at + 1..])),
        None => (rest, None),
    };
    (whole.is_empty()
        || (whole.starts_with(|c: char| c.is_ascii_digit())
            && whole
                .chars()
                .all(|c| c.is_ascii_digit() || c == '_' || c == ',')))
        && fraction.chars().all(|c| c.is_ascii_digit())
        && exponent.is_none_or(|exponent| {
            exponent.len() > 1
                && exponent.starts_with(['+', '-'])
                && exponent[1..].chars().all(|c| c.is_ascii_digit())
        })
        && text != "."
}

/// Psych's legacy integer: binary, octal, decimal (with `_` or `,`) or hex, the sign taken off.
fn is_integer(text: &str) -> bool {
    let all = |digits: &str, allowed: fn(char) -> bool| {
        !digits.is_empty() && digits.chars().all(|c| allowed(c) || c == '_' || c == ',')
    };
    if let Some(binary) = text.strip_prefix("0b") {
        return all(binary, |c| c == '0' || c == '1');
    }
    if let Some(hex) = text.strip_prefix("0x") {
        return all(hex, |c| c.is_ascii_hexdigit());
    }
    if let Some(octal) = text.strip_prefix('0') {
        return octal.is_empty() || all(octal, |c| ('0'..='7').contains(&c));
    }
    text.starts_with(|c: char| c.is_ascii_digit()) && all(text, |c| c.is_ascii_digit())
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    fn read(source: &str) -> Translations {
        let mut translations = Translations {
            files: vec!["file:///en.yml".to_owned()],
            ..Translations::default()
        };
        translations.merge(read_locale(source, "en", 0).expect("an en file"));
        translations
    }

    fn held(translations: &Translations, key: &str) -> Held {
        translations
            .get(key)
            .map_or(Held::Other, |entry| entry.held.clone())
    }

    fn text(value: &str) -> Held {
        Held::Text(value.to_owned())
    }

    /// A key as a card shows it: YAML that reads back as what the locale holds.
    #[test]
    fn a_key_is_shown_as_the_yaml_the_main_locale_holds() {
        let translations = read(
            "en:
  hello: Hello
  count:
    one: 1 story
    other: \"%{count} stories\"
  special: \"yes\"
  spaced: \" padded\"
  colon: \"a: b\"
  hash: \"a #b\"
  number: 12
  none: {}
  days: [Mon, Tue]
  empty: []
  mixed: [1, two]
  long: |
    one
    two
",
        );
        let shown = |key: &str| yaml(key, translations.get(key).expect(key));
        assert_eq!(shown("hello"), "hello: Hello");
        assert_eq!(
            shown("count"),
            "count:\n  one: 1 story\n  other: \"%{count} stories\""
        );
        // What would read back as something else, or not at all, is quoted.
        assert_eq!(shown("special"), "special: \"yes\"");
        assert_eq!(shown("spaced"), "spaced: \" padded\"");
        assert_eq!(shown("colon"), "colon: \"a: b\"");
        assert_eq!(shown("hash"), "hash: \"a #b\"");
        // What the reader keeps no text of says what it is, as a comment.
        assert_eq!(shown("number"), "number: # not a string");
        assert_eq!(shown("mixed"), "mixed: # a list");
        assert_eq!(shown("none"), "none: {}");
        assert_eq!(shown("days"), "days:\n  - Mon\n  - Tue");
        assert_eq!(shown("empty"), "empty: []");
        assert_eq!(shown("long"), "long: |\n  one\n  two");
    }

    /// A key holding more than a card shows stops after [`SHOWN_LINES`], and says so.
    #[test]
    fn a_large_subtree_is_cut_after_ten_lines() {
        let mut source = "en:\n  big:\n".to_owned();
        for index in 0..12 {
            source.push_str(&format!("    k{index:02}:\n      a: x\n"));
        }
        let translations = read(&source);
        let shown = yaml("big", translations.get("big").expect("big"));
        let lines: Vec<&str> = shown.lines().collect();
        assert_eq!(lines.len(), SHOWN_LINES + 1, "{shown}");
        assert_eq!(lines.last(), Some(&"# …"));
    }

    /// Psych's scalar rule, a word at a time: only what it makes a `String` is one.
    #[test]
    fn a_plain_scalar_is_a_string_only_where_psych_makes_one() {
        for (written, string) in [
            ("Hello", true),
            ("Hello %{name}", true),
            ("1 day", true),
            ("yes", false),
            ("No", false),
            ("OFF", false),
            ("null", false),
            ("~", false),
            ("yesterday", true),
            ("12", false),
            ("-3.5", false),
            ("1_000", false),
            ("0x1F", false),
            ("1e3", true),
            ("1.5e+3", false),
            ("1x", true),
            ("0b101", false),
            ("017", false),
            ("1,000", false),
            ("12:30:15.5", false),
            (".", true),
            (".inf", false),
            ("-.Inf", false),
            (".NaN", false),
            ("2024-01-02", false),
            ("2024-01-02 10:00:00 Z", false),
            ("1:30", false),
            (":other", false),
            (":", true),
            ("-", true),
            ("$5", true),
            ("5 $", true),
            ("%{count} items", true),
            ("x", true),
            ("+ x", true),
            ("2024x", true),
            ("2024-01", true),
            ("1:5", false),
            ("1:99", true),
            ("1:123", true),
            ("1:a", true),
            ("1:30.x", true),
            ("+,5.1", true),
            ("1x.5", true),
            ("1_000.5", false),
            ("1,000.5", false),
            ("1.5x", true),
            ("1.5e5", true),
            ("1.5e55", true),
            ("1.5e", true),
            ("1.5e+", true),
            ("0x", true),
            ("0b", true),
            ("0", false),
            ("", false),
        ] {
            assert_eq!(
                matches!(scalar(written), Node::Text(_)),
                string,
                "{written:?}"
            );
        }
    }

    /// Every shape a locale file writes, read to what i18n holds.
    #[test]
    fn a_locale_file_is_read_as_ruby_reads_it() {
        let translations = read(
            "# Comment\n---\nen:\n  hello: Hello  # a note\n  quoted: \"Say \\\"hi\\\" # not a comment\"\n  \
             single: 'It''s'\n  folded: \"one\n    two\"\n  literal: |\n    line one\n\n    line two\n  \
             wrapped: >-\n    a\n    b\n  plain_on: many\n    words\n  count: 12\n  flag: yes\n  \
             link: :hello\n  nothing:\n  days: [Mon, \"Tue\", 3]\n  list:\n    - one\n    - two\n  \
             mixed:\n  - one\n  - 2\n  items:\n    - name: a\n      id: b\n  pairs: {a: x, b: \"y\"}\n  \
             plural:\n    one: \"%{count} item\"\n    other: \"%{count} items\"\n  \
             \"quoted.key\": dotted\n  tagged: !!str 12\n  ruby: !ruby/regexp /x/\n  yes: skipped\n",
        );
        assert_eq!(held(&translations, "hello"), text("Hello"));
        assert_eq!(
            held(&translations, "quoted"),
            text("Say \"hi\" # not a comment")
        );
        assert_eq!(held(&translations, "single"), text("It's"));
        assert_eq!(held(&translations, "folded"), text("one two"));
        assert_eq!(
            held(&translations, "literal"),
            text("line one\n\nline two\n")
        );
        assert_eq!(held(&translations, "wrapped"), text("a b"));
        assert_eq!(held(&translations, "plain_on"), text("many words"));
        assert_eq!(held(&translations, "count"), Held::Other);
        assert_eq!(held(&translations, "flag"), Held::Other);
        assert_eq!(held(&translations, "link"), Held::Other);
        assert_eq!(held(&translations, "nothing"), Held::Other);
        assert_eq!(held(&translations, "days"), Held::List { texts: false });
        assert_eq!(held(&translations, "list"), Held::List { texts: true });
        assert_eq!(held(&translations, "mixed"), Held::List { texts: false });
        assert_eq!(held(&translations, "items"), Held::List { texts: false });
        assert_eq!(held(&translations, "pairs"), Held::Tree);
        assert_eq!(held(&translations, "pairs.b"), text("y"));
        assert_eq!(held(&translations, "plural"), Held::Tree);
        assert!(translations.get("plural").is_some_and(Entry::plural));
        assert!(!translations.get("pairs").is_some_and(Entry::plural));
        assert_eq!(held(&translations, "tagged"), text("12"));
        assert_eq!(held(&translations, "ruby"), Held::Other);
        // `yes:` is `true` to Psych, a key no dotted path names.
        assert!(!translations.root.contains_key("yes"));
        // A quoted key holding a dot is one key, which a dotted path cannot reach.
        assert!(translations.root.contains_key("quoted.key"));
    }

    /// Anchors, aliases and `<<:` merges, which the key a mapping writes itself outranks.
    #[test]
    fn an_alias_and_a_merge_read_what_the_anchor_holds() {
        let translations = read(
            "en:\n  base: &base\n    title: Title\n    body: Body\n  copy: *base\n  \
             merged:\n    <<: *base\n    body: Own\n  both:\n    <<: [*base]\n  \
             word: &word Hi\n  again: *word\n  missing: *nowhere\n",
        );
        assert_eq!(held(&translations, "copy.title"), text("Title"));
        assert_eq!(held(&translations, "merged.title"), text("Title"));
        assert_eq!(held(&translations, "merged.body"), text("Own"));
        assert_eq!(held(&translations, "both.body"), text("Body"));
        assert_eq!(held(&translations, "again"), text("Hi"));
        assert_eq!(held(&translations, "missing"), Held::Other);
    }

    /// Only the main locale is read, and a file that writes it last wins for a key two write.
    #[test]
    fn a_later_file_replaces_a_key_and_merges_a_tree() {
        assert_eq!(
            locales_in("# x\n---\nen:\n  a: b\nde:\n  a: c\n"),
            ["en", "de"]
        );
        assert!(read_locale("de:\n  a: b\n", "en", 0).is_none());
        assert!(read_locale("- a\n", "en", 0).is_none());
        let mut translations = Translations::default();
        translations
            .merge(read_locale("en:\n  a:\n    b: B\n    c: C\n  d: D\n", "en", 0).unwrap());
        translations.merge(read_locale("en:\n  a:\n    c: C2\n  d:\n    e: E\n", "en", 1).unwrap());
        assert_eq!(held(&translations, "a.b"), text("B"));
        assert_eq!(held(&translations, "a.c"), text("C2"));
        assert_eq!(held(&translations, "d"), Held::Tree);
        assert_eq!(translations.get("a").unwrap().file, 1);
        assert_eq!(
            translations
                .under("a")
                .map(|keys| keys.keys().cloned().collect::<Vec<_>>()),
            Some(vec!["b".to_owned(), "c".to_owned()])
        );
        assert!(translations.under("").is_some());
        assert!(translations.under("nowhere").is_none());
        assert!(translations.get("").is_none());
    }

    /// Where a key's name is written: what a jump selects.
    #[test]
    fn a_key_is_placed_at_its_name() {
        let source = "en:\n  title: Hello\n  \"quoted\": x\n  tree:\n    leaf: y\n";
        let translations = read(source);
        for (key, name) in [
            ("title", "title"),
            ("quoted", "\"quoted\""),
            ("tree.leaf", "leaf"),
        ] {
            let (start, end) = translations.get(key).unwrap().at;
            assert_eq!(&source[start as usize..end as usize], name, "{key}");
        }
    }

    /// A `.rb` locale file is its hash literal; a value only Ruby knows is not a string.
    #[test]
    fn a_ruby_locale_file_is_its_hash_literal() {
        let source = "{\n  en: {\n    number: {\n      nth: {\n        ordinals: ->(k, n) { \"th\" },\n        \
                      word: \"th\",\n        \"quoted\" => { deep: \"x\" }\n      }\n    }\n  },\n  de: {}\n}\n";
        let mut translations = Translations::default();
        translations.merge(read_ruby(source, "en", 0).unwrap());
        assert_eq!(held(&translations, "number.nth.ordinals"), Held::Other);
        assert_eq!(held(&translations, "number.nth.word"), text("th"));
        assert_eq!(held(&translations, "number.nth.quoted.deep"), text("x"));
        assert!(read_ruby(source, "fr", 0).is_none());
        assert!(read_ruby("x = 1\n", "en", 0).is_none());
        assert!(read_ruby("", "en", 0).is_none());
        assert!(read_ruby("{ en: 1, 2 => 3 }\n", "en", 0).is_some());
        assert!(read_ruby("{ en: { **other, a: \"b\", 1 => 2 } }\n", "en", 0).is_some());
        // Its locales are the hash's keys, however indented; a key written twice is the last.
        assert_eq!(ruby_locales_in(source), ["en", "de"]);
        assert_eq!(
            ruby_locales_in("{ :fr => {}, \"pt-BR\" => {}, **x }\n"),
            ["fr", "pt-BR"]
        );
        assert!(ruby_locales_in("x = 1\n").is_empty());
        assert!(ruby_locales_in("").is_empty());
        let twice = read_ruby("{ en: { a: \"one\" }, en: { b: \"two\" } }\n", "en", 0).unwrap();
        assert_eq!(twice.keys().collect::<Vec<_>>(), ["b"]);
    }

    /// What a call to `t` hands back, by what the main locale holds and what the call says.
    #[test]
    fn a_translation_is_the_type_its_key_holds() {
        let translations = read(
            "en:\n  title: Title\n  title_html: <b>Title</b>\n  html: <i>x</i>\n  \
             plural:\n    one: one\n    other: many\n  plural_html:\n    one: one\n    other: many\n  \
             tree:\n    a: x\n  tree_html:\n    a: x\n  days: [Mon, Tue]\n  mixed: [Mon, 1]\n  \
             count: 1\n  scoped:\n    deep: x\n",
        );
        let asked = |key: &str| Asked {
            key: key.to_owned(),
            scope: Vec::new(),
            count: false,
            other_default: false,
            html_safe: false,
        };
        let returns = |asked: Asked| translations.returns(&asked);
        assert_eq!(returns(asked("title")), Some("String"));
        assert_eq!(returns(asked("title_html")), Some("String"));
        let view = |key: &str| Asked {
            html_safe: true,
            ..asked(key)
        };
        assert_eq!(
            returns(view("title_html")),
            Some("ActiveSupport::SafeBuffer")
        );
        assert_eq!(returns(view("html")), Some("ActiveSupport::SafeBuffer"));
        assert_eq!(returns(view("title")), Some("String"));
        assert_eq!(returns(asked("plural")), Some("Hash[Symbol, untyped]"));
        let counted = |key: &str, html_safe| Asked {
            count: true,
            html_safe,
            ..asked(key)
        };
        assert_eq!(returns(counted("plural", false)), Some("String"));
        assert_eq!(
            returns(counted("plural_html", true)),
            Some("ActiveSupport::SafeBuffer")
        );
        // i18n picks `tree.one` by the count, or raises where the tree has no such key.
        assert_eq!(returns(counted("tree", false)), None);
        assert_eq!(returns(view("tree_html")), None);
        assert_eq!(returns(asked("days")), Some("Array[String]"));
        assert_eq!(returns(asked("mixed")), Some("Array[untyped]"));
        assert_eq!(returns(asked("count")), None);
        assert_eq!(returns(asked("nowhere")), None);
        let scoped = Asked {
            scope: vec!["scoped".to_owned()],
            ..asked("deep")
        };
        assert_eq!(scoped.path(), "scoped.deep");
        assert_eq!(returns(scoped), Some("String"));
        let defaulted = Asked {
            other_default: true,
            ..asked("title")
        };
        assert_eq!(returns(defaulted), None);
    }

    /// What the scanner gives up on is not a guess: a line deeper than its key, an unclosed quote
    /// or bracket, a flow mapping it cannot split, a complex key.
    #[test]
    fn what_the_scanner_cannot_read_is_nothing() {
        let translations =
            read("en:\n  a: x\n      stray: y\n  d: {a b}\n  ? complex\n  e: fine\n");
        assert_eq!(held(&translations, "a"), text("x"));
        assert_eq!(held(&translations, "d"), Held::Other);
        assert_eq!(held(&translations, "e"), text("fine"));
        // An unclosed quote or bracket runs to the end: Psych would refuse the whole file.
        let translations = read("en:\n  b: \"open\n  c: x\n");
        assert_eq!(held(&translations, "b"), Held::Other);
        let translations = read("en:\n  c: [x,\n");
        assert_eq!(held(&translations, "c"), Held::Other);
    }
    /// The rarer shapes a locale file writes, each read as Psych reads it, or declined.
    #[test]
    fn the_rarer_shapes_are_read_or_declined() {
        let is = |source: &str, key: &str, expected: Held| {
            assert_eq!(held(&read(source), key), expected, "{key} in {source:?}");
        };
        let texts = |texts| Held::List { texts };
        // Directives, document markers and blank lines are not content.
        is("%YAML 1.1\n--- # doc\nen:\n\n  a: x\n...\n", "a", text("x"));
        // An empty item, then one at the same indentation: the empty one holds nothing.
        is("en:\n  list:\n  -\n  - b\n", "list", texts(false));
        // An item line where a mapping's keys are is not one of them; the lines after it are
        // skipped rather than guessed at.
        let stray = read("en:\n  a: x\n  - stray\n  c: y\n");
        assert_eq!(held(&stray, "a"), text("x"));
        assert!(stray.get("c").is_none());
        // A key with no name is read past.
        is("en:\n  : x\n  a: y\n", "a", text("y"));
        // A merge of something that is not a mapping merges nothing.
        let merged = read("en:\n  base: &b text\n  m:\n    <<: *b\n    k: v\n");
        assert_eq!(held(&merged, "m.k"), text("v"));
        assert_eq!(merged.get("m").unwrap().children.len(), 1);
        // `- "q": 1` is a mapping, as `- q: 1` is.
        is("en:\n  items:\n    - \"q\": 1\n", "items", texts(false));
        // `!!str` with nothing after it.
        is("en:\n  a: !!str\n  b: c\n", "a", Held::Other);
        is("en:\n  a: !\n  b: c\n", "a", Held::Other);
        // A value on the line below its key.
        is("en:\n  a:\n    [x, y]\n", "a", texts(true));
        is("en:\n  b:\n    plain words\n", "b", text("plain words"));
        // An item that is only a comment, and a key whose next line is shallower: `nil`.
        is("en:\n  l:\n    - # c\n  b: x\n", "l", texts(false));
        is("en:\n  a:\nde:\n  b: x\n", "a", Held::Other);
        // Block scalars: at the end of the file, folded around an empty and a deeper line, kept,
        // and empty.
        is("en:\n  a: |\n    x", "a", text("x\n"));
        is(
            "en:\n  f: >\n    a\n    b\n\n    c\n      d\n    e\n",
            "f",
            text("a b\nc\n  d\ne\n"),
        );
        is("en:\n  k: |+\n    x\n\n  n: y\n", "k", text("x\n\n"));
        is("en:\n  e: |\n  n: y\n", "e", text(""));
        // Quoted scalars over several lines: an empty line is a break, `''` an apostrophe, a `\`
        // before the break joins the lines, and a shallower line ends what never closed.
        is("en:\n  a: \"x\n\n    y\"\n", "a", text("x\ny"));
        is("en:\n  a: 'it''s\n    two'\n", "a", text("it's two"));
        is("en:\n  a: \"x\\\n    y\"\n", "a", text("xy"));
        is("en:\n  a: \"open\nde: x\n", "a", Held::Other);
        is("en:\n  a: 'x\\y'\n", "a", text("x\\y"));
        is("en:\n  b: \"it's\"\n", "b", text("it's"));
        // A flow sequence over a blank line and a comment, one cut off by a shallower line, and one
        // nesting another, with a trailing comma.
        is("en:\n  f: [x,\n\n    # c\n    y]\n", "f", texts(true));
        is("en:\n  f: [x,\n  g: y\n", "f", Held::Other);
        is("en:\n  n: [a, [b, c], ]\n", "n", texts(false));
        // A plain scalar stops at a comment and at a blank line.
        let plain = read("en:\n  p: one\n    # c\n  q: two\n\n");
        assert_eq!(held(&plain, "p"), text("one"));
        assert_eq!(held(&plain, "q"), text("two"));
        // Quoted keys, and ones that are not keys at all.
        let quoted = read("en:\n  'single': one\n  \"k\":\n  'bad':x\n");
        assert_eq!(held(&quoted, "single"), text("one"));
        assert_eq!(quoted.get("single").unwrap().at, (6, 14));
        assert_eq!(held(&quoted, "k"), Held::Other);
        assert!(quoted.get("k").is_some());
        assert!(quoted.get("bad").is_none());
        // An empty mapping is a tree, and not a plural.
        let empty = read("en:\n  e: {}\n");
        assert_eq!(held(&empty, "e"), Held::Tree);
        assert!(!empty.get("e").unwrap().plural());
        assert!(!read("en:\n  t: x\n").get("t").unwrap().plural());
        // A tag that makes a string of what would not be one.
        is("en:\n  a: !!str yes\n", "a", text("yes"));
        is("en:\n  a: !!str plain\n", "a", text("plain"));
        is("en:\n  a: ! 12 # c\n", "a", text("12"));
        is("en:\n  a: !!int 12\n", "a", Held::Other);
        // Folded: an empty line after a deeper one keeps both breaks.
        is(
            "en:\n  f: >\n    a\n      b\n\n    c\n",
            "f",
            text("a\n  b\n\nc\n"),
        );
        // Double-quoted escapes, a code point in hex, one that is no code point, and a `\` that
        // escapes nothing YAML names.
        is(
            "en:\n  e: \"\\n\\t\\r\\0\\a\\b\\e\\f\\v\\_\\N\\L\\P\\x41\\u00e9\\U0001F600\\\"\\\\\\/\\xZZ\"\n",
            "e",
            text(
                "\n\t\r\0\u{7}\u{8}\u{1b}\u{c}\u{b}\u{a0}\u{85}\u{2028}\u{2029}A\u{e9}\u{1f600}\"\\/",
            ),
        );
        // A locale holding a scalar holds no keys.
        assert_eq!(read_locale("en: x\n", "en", 0), Some(BTreeMap::new()));
        // A string written over a tree replaces it.
        let mut replaced = read("en:\n  a:\n    b: x\n");
        replaced.merge(read_locale("en:\n  a: y\n", "en", 1).unwrap());
        assert_eq!(held(&replaced, "a"), text("y"));
        // A file's locales are its top-level keys, past blank lines and lines that are not keys.
        assert_eq!(locales_in("\nen:\nfoo\n"), ["en"]);
    }
}
