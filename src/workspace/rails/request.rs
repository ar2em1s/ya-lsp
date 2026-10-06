//! A value read straight off the request's `params`: what Rails can hand back for it, and what the
//! application's own code changes of it.
//!
//! A parsed request holds only what a parser makes of text: Rack's query and form parser gives
//! `String`, `Array` and nested hashes, a multipart body adds an `UploadedFile`, and the JSON parser
//! adds `Integer`, `Float`, `true`, `false` and `nil`. `ActionController::Parameters` hands every
//! nested hash back as a `Parameters`. So a key read straight off it is one of [`VALUE`], or `nil`.
//!
//! **That holds only while nothing else wrote into it**, and four things can:
//!
//! 1. **The code's own writes** ([`read_request_writes`]): `params[:locale] = I18n.locale` puts a
//!    `Symbol` there. A write counts for reads in the class or module it is written in and
//!    whatever has that one among its ancestors ([`Requests::value`]), the body's name resolved as
//!    Ruby resolves it ([`Requests::absorb_writes`]); one outside any counts for every read. A value the text names joins its class to the key; any other value refuses the
//!    key, and a key the text cannot bound with such a value, or a mutator, refuses them all.
//! 2. **A route's defaults** ([`read_route_values`]): `defaults: { format: :json }` is a path
//!    parameter Rails never turns into a `String`.
//! 3. **A parser of the application's own**, or `parse_json_times` ([`read_request_settings`]),
//!    which can hand back anything.
//! 4. **A callee the params are handed to** that writes into what it is handed: `normalize(params)`
//!    with `def normalize(raw) raw[:from] = Date.parse(raw[:from]) end`. Matched by the method's
//!    name, so any method of that name writing into a parameter refuses every key.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use ruby_prism::{CallNode, Node, Visit};

use super::syntax::constant_spelling;

/// What a key read off a parsed request can be, besides `nil`.
pub const VALUE: [&str; 7] = [
    "String",
    "Integer",
    "Float",
    "bool",
    "Array[untyped]",
    "ActionController::Parameters",
    "ActionDispatch::Http::UploadedFile",
];

/// What `permit(:x)` lets through of [`VALUE`]: the scalars.
pub const SCALAR: [&str; 5] = [
    "String",
    "Integer",
    "Float",
    "bool",
    "ActionDispatch::Http::UploadedFile",
];

/// A value written that is already one of [`VALUE`], so it adds nothing.
const INSIDE: &str = "";

/// What some code does to the request's params.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Touched {
    /// Something wrote a value the text cannot name under a key it cannot name, or replaced the
    /// whole object: no key can be read.
    pub everything: bool,
    /// Something wrote into a value under a key (`params[:a][:b] = …`), which only a `dig` with
    /// several keys reads.
    pub nested: bool,
    /// Each literal key written, and the class each value written there is: `None` where the text
    /// does not say, which refuses the key.
    pub keys: BTreeMap<String, BTreeSet<Option<&'static str>>>,
    /// The class each value written under a key the text cannot name is, which any key may hold.
    pub any: BTreeSet<&'static str>,
    /// Methods whose body writes into one of their own parameters, by name.
    pub mutators: BTreeSet<String>,
    /// Methods the request's params are handed to, by name (`new` is `initialize`).
    pub handed: BTreeSet<String>,
    /// Methods a value read off them is handed to, by name, which only a `dig` through several
    /// keys reads.
    pub handed_nested: BTreeSet<String>,
}

impl Touched {
    /// Some more code's.
    pub fn absorb(&mut self, other: &Self) {
        self.everything |= other.everything;
        self.nested |= other.nested;
        for (key, classes) in &other.keys {
            self.keys
                .entry(key.clone())
                .or_default()
                .extend(classes.iter().copied());
        }
        self.any.extend(other.any.iter().copied());
        self.mutators.extend(other.mutators.iter().cloned());
        self.handed.extend(other.handed.iter().cloned());
        self.handed_nested
            .extend(other.handed_nested.iter().cloned());
    }

    /// A write of values of `classes` (`None`: the text does not say) under `key` (`None`: a key the
    /// text cannot name).
    fn write(&mut self, key: Option<String>, classes: Option<BTreeSet<&'static str>>) {
        match (key, classes) {
            (Some(key), Some(classes)) => self
                .keys
                .entry(key)
                .or_default()
                .extend(classes.into_iter().map(Some)),
            (Some(key), None) => {
                self.keys.entry(key).or_default().insert(None);
            }
            (None, Some(classes)) => self.any.extend(classes),
            (None, None) => self.everything = true,
        }
    }
}

/// One read off the request's params, as the call writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked<'a> {
    /// The method's bare name: `[]`, `fetch`, `dig`, `require`, `expect`.
    pub method: &'a str,
    /// Each positional argument: its text where it is a symbol or a string literal.
    pub keys: &'a [Option<String>],
    /// Whether the call writes a block.
    pub block: bool,
    /// What every route reaching the read gives its one key, where the routes prove it
    /// (`Controllers::proven`): `String` for a required segment, a default's class otherwise.
    pub proven: Option<&'a [&'static str]>,
}

/// What every file the project loads does to the request's params, kept by where each write sits.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Requests {
    /// Writes outside any class or module: every read's.
    everywhere: Touched,
    /// What the routes give a key: every read's, except one the routes prove, whose proof already
    /// read the defaults of every route reaching it.
    routes: Touched,
    /// Writes in a class or module body, by its name.
    owned: BTreeMap<String, Touched>,
    /// Methods writing into one of their own parameters, wherever they are written.
    mutators: BTreeSet<String>,
}

impl Requests {
    /// One file's writes ([`read_request_writes`]). `resolve` names the body each sits in, from the
    /// nesting as written (`["Admin", "Base"]`), the way Ruby's constant lookup does.
    pub fn absorb_writes(
        &mut self,
        writes: &BTreeMap<Vec<String>, Touched>,
        resolve: &dyn Fn(&[String]) -> String,
    ) {
        for (nesting, touched) in writes {
            self.mutators.extend(touched.mutators.iter().cloned());
            let into = if nesting.is_empty() {
                &mut self.everywhere
            } else {
                self.owned.entry(resolve(nesting)).or_default()
            };
            into.absorb(touched);
        }
    }

    /// What some routes give a key ([`read_route_values`]), which every read holds.
    pub fn absorb_routes(&mut self, touched: &Touched) {
        self.routes.absorb(touched);
    }

    /// What a read hands back, in code whose `self` has these ancestors (its own class first, by
    /// name), as [`request_value`] answers it.
    #[must_use]
    pub fn value(&self, asked: &Asked<'_>, ancestors: &[&str]) -> Option<String> {
        request_value(asked, &self.touched(asked.proven.is_some(), ancestors))
    }

    /// What a read of each key a `permit` or `expect` lets through hands back, in code whose `self`
    /// has these ancestors, as [`permitted_reads`] answers it.
    #[must_use]
    pub fn permitted(&self, permitted: &Permitted<'_>, ancestors: &[&str]) -> Vec<KeyReads> {
        permitted_reads(permitted, &self.touched(false, ancestors))
    }

    /// Everything that may have changed the params a read in code whose `self` has these ancestors
    /// sees. A read the routes prove leaves their values out: its proof read them.
    fn touched(&self, proven: bool, ancestors: &[&str]) -> Touched {
        let mut touched = self.everywhere.clone();
        if proven {
            touched.everything |= self.routes.everything;
        } else {
            touched.absorb(&self.routes);
        }
        for ancestor in ancestors {
            if let Some(owned) = self.owned.get(*ancestor) {
                touched.absorb(owned);
            }
        }
        touched.mutators.extend(self.mutators.iter().cloned());
        touched
    }
}

/// A filter `permit` or `expect` is written with, a literal at each level: `permit(:title, tags:
/// [])` is a name and a hash pairing `tags` with an empty list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spelled {
    /// A symbol's or a string's text.
    Name(String),
    /// An array literal.
    List(Vec<Spelled>),
    /// A hash literal, by each key's text.
    Pairs(Vec<(String, Spelled)>),
}

/// What `permit` or `expect` lets through under one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// `:title`: a scalar (`permitted_scalar?`).
    Scalar,
    /// `tags: []`: an array of scalars.
    Scalars,
    /// `prefs: {}`: a hash, with whatever it holds.
    Any,
    /// `comments: [[:text]]`: hashes in an array, or a hash of them by index.
    Each,
    /// `address: [:street]`, `address: { geo: [] }`, `address: :street`: a hash, filtered by these.
    Nested(Vec<(String, Filter)>),
}

/// Each key a list of filters names, and what it lets through, as Rails reads `permit(*filters)`:
/// the list flattened, a name a scalar, a hash each of its pairs. A key named twice is each.
#[must_use]
pub fn filters(written: &[Spelled]) -> Vec<(String, Filter)> {
    let mut found = Vec::new();
    for spelled in written {
        match spelled {
            Spelled::Name(name) => found.push((name.clone(), Filter::Scalar)),
            Spelled::List(inner) => found.extend(filters(inner)),
            Spelled::Pairs(pairs) => found.extend(
                pairs
                    .iter()
                    .map(|(key, value)| (key.clone(), filter_of(value))),
            ),
        }
    }
    found
}

/// What a hash filter's value lets through (`permit_value`): `[]` scalars, `{}` anything, a list
/// holding one list hashes in an array (`array_filter?`), anything else one hash filtered by it,
/// as `Array.wrap` reads it.
fn filter_of(value: &Spelled) -> Filter {
    match value {
        Spelled::List(inner) if inner.is_empty() => Filter::Scalars,
        Spelled::Pairs(pairs) if pairs.is_empty() => Filter::Any,
        Spelled::List(inner) if matches!(inner.as_slice(), [Spelled::List(_)]) => Filter::Each,
        other => Filter::Nested(filters(std::slice::from_ref(other))),
    }
}

/// What `expect` hands back for these filters, and the key and filters of the hash it hands back
/// where that hash is filtered: one key's value, required, so never `nil` (an array for `[]` or
/// `[[…]]`, a hash for `{}` or a filtered hash, or a hash of hashes by index for `[[…]]`), and
/// several keys' values in an array. `None` for one bare name, which `expect`'s `(Symbol)` arm
/// answers, and for none.
#[must_use]
pub fn expected(filters: &[(String, Filter)]) -> Option<Expected<'_>> {
    match filters {
        [] => None,
        [(key, filter)] => match filter {
            Filter::Scalar => None,
            Filter::Scalars => Some(("Array[untyped]", None)),
            Filter::Any => Some((PARAMETERS, None)),
            Filter::Each => Some((ARRAY_OR_PARAMETERS, None)),
            Filter::Nested(inner) => Some((PARAMETERS, Some((key.as_str(), inner.as_slice())))),
        },
        _ => Some(("Array[untyped]", None)),
    }
}

/// What `expect` hands back ([`expected`]): its type, and the key and filters of the hash it
/// hands back where that hash is filtered.
pub type Expected<'f> = (&'static str, Option<(&'f str, &'f [(String, Filter)])>);

/// The class `permit` and `expect` hand back, and each hash they keep.
const PARAMETERS: &str = "ActionController::Parameters";

/// What a filtered hash or `[[…]]` lets through where the request holds an array of hashes or a
/// hash of them by index.
const ARRAY_OR_PARAMETERS: &str = "Array[untyped] | ActionController::Parameters";

/// One `permit` or `expect` off the request's params, as its receiver and its filters write it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permitted<'a> {
    /// The keys read between the params and the hash filtered: `params.require(:post)` is
    /// `["post"]`.
    pub path: &'a [String],
    /// What the hash is filtered by ([`filters`]).
    pub filters: &'a [(String, Filter)],
    /// Arrays must be written out (`expect`): a filtered hash is then a hash, never an array of
    /// them.
    pub explicit: bool,
}

/// What a read of one permitted key hands back, as RBS unions with `nil` written out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyReads {
    pub key: String,
    /// `[]` and a one-key `dig`: `nil` where the key was filtered out or never sent.
    pub read: String,
    /// One-key `fetch`, which raises where the key is absent: `nil` only where a scalar was sent
    /// as one.
    pub fetched: String,
    /// One-key `require`, which raises on a blank value.
    pub required: String,
}

/// What each key a filter lets through holds once read, from what the request can hold there
/// ([`request_value`], with what the code writes into it): a scalar filter keeps the scalars
/// (`permitted_scalar?`: every class a request or a write here gives but an array and a hash), `[]`
/// an array, `{}` a hash, `[[…]]` an array or a hash of hashes by index, and any other filter a
/// hash, or under `permit` an array of them too. A key the request's writes leave open is left
/// out.
#[must_use]
pub fn permitted_reads(permitted: &Permitted<'_>, touched: &Touched) -> Vec<KeyReads> {
    let mut by_key: BTreeMap<&str, Option<(Vec<String>, bool)>> = BTreeMap::new();
    for (key, filter) in permitted.filters {
        let let_through = held_under(permitted.path, key, touched)
            .map(|held| let_through(filter, permitted.explicit, held));
        let entry = by_key
            .entry(key.as_str())
            .or_insert(Some((Vec::new(), false)));
        *entry = match (entry.take(), let_through) {
            (Some((mut classes, null)), Some((more, also))) => {
                for class in more {
                    if !classes.contains(&class) {
                        classes.push(class);
                    }
                }
                Some((classes, null || also))
            }
            _ => None,
        };
    }
    by_key
        .into_iter()
        .filter_map(|(key, kept)| {
            // Never empty: what a request carries holds a scalar, an array and a hash.
            let (classes, null) = kept?;
            let required = classes.join(" | ");
            Some(KeyReads {
                key: key.to_owned(),
                read: format!("{required} | nil"),
                fetched: if null {
                    format!("{required} | nil")
                } else {
                    required.clone()
                },
                required,
            })
        })
        .collect()
}

/// What the request can hold under `key` below `path`, without `nil`, or `None` where the code
/// leaves it open ([`request_value`]).
fn held_under(path: &[String], key: &str, touched: &Touched) -> Option<Vec<String>> {
    let keys: Vec<Option<String>> = path
        .iter()
        .map(String::as_str)
        .chain([key])
        .map(|key| Some(key.to_owned()))
        .collect();
    let union = request_value(
        &Asked {
            method: if path.is_empty() { "[]" } else { "dig" },
            keys: &keys,
            block: false,
            proven: None,
        },
        touched,
    )?;
    Some(
        union
            .split(" | ")
            .filter(|class| *class != "nil")
            .map(str::to_owned)
            .collect(),
    )
}

/// What a filter keeps of the classes a key can hold, and whether a value kept may be `nil`.
fn let_through(filter: &Filter, explicit: bool, held: Vec<String>) -> (Vec<String>, bool) {
    let array = |class: &str| class == "Array[untyped]";
    let hash = |class: &str| class == PARAMETERS;
    match filter {
        Filter::Scalar => (
            held.into_iter()
                .filter(|class| !array(class) && !hash(class))
                .collect(),
            true,
        ),
        Filter::Scalars => (
            held.into_iter().filter(|class| array(class)).collect(),
            false,
        ),
        Filter::Any => (
            held.into_iter().filter(|class| hash(class)).collect(),
            false,
        ),
        Filter::Nested(_) if explicit => (
            held.into_iter().filter(|class| hash(class)).collect(),
            false,
        ),
        Filter::Each | Filter::Nested(_) => (
            held.into_iter()
                .filter(|class| array(class) || hash(class))
                .collect(),
            false,
        ),
    }
}

/// What a read off the request's params hands back, as an RBS union with `nil` written out, or
/// `None` where the code leaves it open.
///
/// - **`[]`, one-key `fetch` and `dig`** are the key's value or `nil`: `fetch` raises only where the
///   key is absent, and `?x` alone hands `nil` back.
/// - **One-key `require`** raises on a blank value, so it is never `nil`.
/// - **`expect(:id)`** is `permit(:id)` then `require(:id)`: a scalar, never `nil`.
/// - **A `dig` through several keys** reads values under the first, which a nested write may have
///   changed.
/// - **A key the call does not spell** may be any key: every key's writes join, and one refused key
///   refuses it.
/// - Everything else (a default for `fetch`, a block, `require` of a list) is left to the method's
///   own body.
#[must_use]
pub fn request_value(asked: &Asked<'_>, touched: &Touched) -> Option<String> {
    let escapes = touched
        .handed
        .iter()
        .any(|name| touched.mutators.contains(name));
    if touched.everything || escapes || asked.block {
        return None;
    }
    let (key, mut classes, mut nil): (Option<&str>, &[&str], bool) =
        match (asked.method, asked.keys) {
            ("[]" | "fetch", [key]) => (key.as_deref(), &VALUE, true),
            ("dig", [key, rest @ ..]) => {
                let deep_written = touched.nested
                    || touched
                        .handed_nested
                        .iter()
                        .any(|name| touched.mutators.contains(name));
                if !rest.is_empty() && deep_written {
                    return None;
                }
                (key.as_deref(), &VALUE, true)
            }
            ("require", [Some(key)]) => (Some(key.as_str()), &VALUE, false),
            ("expect", [Some(key)]) => (Some(key.as_str()), &SCALAR, false),
            _ => return None,
        };
    // A key every route reaching the read gives is what those routes give it: never absent, so
    // `nil` only where a default is `nil`.
    if let (Some(proven), Some(_)) = (asked.proven, key) {
        classes = proven;
        nil = proven.contains(&"nil") && asked.method != "require" && asked.method != "expect";
    }
    let mut written: Vec<Option<&'static str>> = touched.any.iter().copied().map(Some).collect();
    match key {
        Some(key) => written.extend(touched.keys.get(key).into_iter().flatten().copied()),
        None => written.extend(touched.keys.values().flatten().copied()),
    }
    let mut members: Vec<&str> = classes
        .iter()
        .copied()
        .filter(|class| *class != "nil")
        .collect();
    for class in written {
        let class = class?;
        if class == "nil" {
            nil |= !matches!(asked.method, "require" | "expect");
            continue;
        }
        // `expect` permits scalars alone: a `Parameters` or an `Array` written there is filtered
        // out, and `require` then raises.
        let kept = match asked.method {
            "expect" => !matches!(class, "Array[untyped]" | "ActionController::Parameters"),
            _ => true,
        };
        // A value inside the union adds nothing to the union; to a proof it adds the union.
        if class == INSIDE && asked.proven.is_some() {
            return None;
        }
        if kept && class != INSIDE && !members.contains(&class) {
            members.push(class);
        }
    }
    if nil {
        members.push("nil");
    }
    Some(members.join(" | "))
}

/// The class a literal is once it is read back off the params, or `None` where the text does not
/// say. A `Hash` comes back as a `Parameters`, which converts one on read.
fn literal_class(node: &Node<'_>) -> Option<&'static str> {
    if node.as_string_node().is_some()
        || node.as_interpolated_string_node().is_some()
        || node.as_x_string_node().is_some()
    {
        Some("String")
    } else if node.as_symbol_node().is_some() || node.as_interpolated_symbol_node().is_some() {
        Some("Symbol")
    } else if node.as_integer_node().is_some() {
        Some("Integer")
    } else if node.as_float_node().is_some() {
        Some("Float")
    } else if node.as_true_node().is_some() || node.as_false_node().is_some() {
        Some("bool")
    } else if node.as_nil_node().is_some() {
        Some("nil")
    } else if node.as_array_node().is_some() {
        Some("Array[untyped]")
    } else if node.as_hash_node().is_some() {
        Some("ActionController::Parameters")
    } else {
        None
    }
}

/// A literal key: a symbol's or a plain string's text.
fn literal_key(node: &Node<'_>) -> Option<String> {
    if let Some(symbol) = node.as_symbol_node() {
        return Some(String::from_utf8_lossy(symbol.unescaped()).into_owned());
    }
    let string = node.as_string_node()?;
    Some(String::from_utf8_lossy(string.unescaped()).into_owned())
}

fn name_of(node: &CallNode<'_>) -> String {
    String::from_utf8_lossy(node.name().as_slice()).into_owned()
}

/// What a receiver holds, as far as the text says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    /// The request's params themselves.
    Whole,
    /// A value read off them, under some key.
    Nested,
    /// A parameter of the method being read, which a caller may hand the params to.
    Handed,
    /// Request values, in an object of their own: a copy, or a value taken out. Writing into it
    /// changes nothing here.
    Copied,
}

/// Methods that change a hash's values in place, past what [`MERGING`] reads.
const REPLACING: [&str; 5] = [
    "replace",
    "transform_values!",
    "deep_transform_values!",
    "instance_variable_set",
    "default=",
];

/// Methods that write each pair they are given into the hash.
const MERGING: [&str; 6] = [
    "merge!",
    "update",
    "reverse_merge!",
    "reverse_update",
    "deep_merge!",
    "with_defaults!",
];

/// Methods that only take keys away, rename them, or mark the object permitted: what is left holds
/// values it held.
const REMOVING: [&str; 15] = [
    "delete",
    "except!",
    "slice!",
    "extract!",
    "compact!",
    "compact_blank!",
    "reject!",
    "select!",
    "filter!",
    "delete_if",
    "keep_if",
    "permit!",
    "clear",
    "transform_keys!",
    "deep_transform_keys!",
];

/// Methods that hand back a value stored under a key, the object itself and not a copy.
const READING: [&str; 5] = ["[]", "require", "required", "fetch", "dig"];

/// Methods whose value, on what holds request values, still holds only request values (or `nil`):
/// a copy, a filter, a value taken out.
const COPYING: [&str; 16] = [
    "permit",
    "permit!",
    "slice",
    "except",
    "to_h",
    "to_hash",
    "to_unsafe_h",
    "to_unsafe_hash",
    "with_indifferent_access",
    "compact",
    "compact_blank",
    "presence",
    "dup",
    "deep_dup",
    "delete",
    "values_at",
];

/// Methods whose value on any request value is one class, or a raise: `to_s` is a `String` on all
/// of them, `strip` exists on `String` alone.
const CONVERTING: [(&str, &str); 11] = [
    ("to_s", "String"),
    ("strip", "String"),
    ("downcase", "String"),
    ("upcase", "String"),
    ("squish", "String"),
    ("titleize", "String"),
    ("capitalize", "String"),
    ("to_i", "Integer"),
    ("to_f", "Float"),
    ("to_sym", "Symbol"),
    ("split", "Array[untyped]"),
];

/// Every write into the request's params one file makes, by the nesting of class and module bodies
/// it is written in, each name as written (empty outside any), and every method that writes into
/// its own parameter.
///
/// The params are `params` with no receiver or on `self`, what `request` hands back as `params`,
/// `parameters` and their parts, and a local or instance variable holding one of those. A value
/// read off them (`params[:a]`, `params.require(:a)`) is the object stored there: a write into it is
/// a nested write. `permit`, `slice` and `merge` build a new object, so a write into what they
/// return changes nothing here.
#[must_use]
pub fn read_request_writes(source: &str) -> BTreeMap<Vec<String>, Touched> {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut ivars = Ivars::default();
    ivars.visit(&parsed.node());
    let mut writes = Writes {
        source,
        ivars: ivars.holding,
        scopes: vec![HashMap::new()],
        defs: Vec::new(),
        owners: Vec::new(),
        found: BTreeMap::new(),
    };
    writes.visit(&parsed.node());
    writes.found
}

/// The instance variables a file assigns the request's params to, first pass.
#[derive(Default)]
struct Ivars {
    holding: BTreeSet<String>,
}

impl<'pr> Visit<'pr> for Ivars {
    fn visit_instance_variable_write_node(
        &mut self,
        node: &ruby_prism::InstanceVariableWriteNode<'pr>,
    ) {
        if is_request(&node.value()) {
            self.holding
                .insert(String::from_utf8_lossy(node.name().as_slice()).into_owned());
        }
        ruby_prism::visit_instance_variable_write_node(self, node);
    }

    fn visit_instance_variable_or_write_node(
        &mut self,
        node: &ruby_prism::InstanceVariableOrWriteNode<'pr>,
    ) {
        if is_request(&node.value()) {
            self.holding
                .insert(String::from_utf8_lossy(node.name().as_slice()).into_owned());
        }
        ruby_prism::visit_instance_variable_or_write_node(self, node);
    }
}

/// `params` on `self`, or what `request` hands back as its parameters.
fn is_request(node: &Node<'_>) -> bool {
    let Some(call) = node.as_call_node() else {
        return false;
    };
    let name = call.name();
    let name = name.as_slice();
    if call.arguments().is_some() || call.block().is_some() {
        return false;
    }
    match call.receiver() {
        None => name == b"params",
        Some(receiver) if receiver.as_self_node().is_some() => name == b"params",
        Some(receiver) => {
            let request = receiver.as_call_node().is_some_and(|request| {
                request.name().as_slice() == b"request"
                    && request.arguments().is_none()
                    && request
                        .receiver()
                        .is_none_or(|on| on.as_self_node().is_some())
            });
            let controller = receiver.as_call_node().is_some_and(|controller| {
                controller.name().as_slice() == b"controller"
                    && controller.arguments().is_none()
                    && controller.receiver().is_none()
            }) || receiver
                .as_instance_variable_read_node()
                .is_some_and(|ivar| {
                    matches!(ivar.name().as_slice(), b"@controller" | b"@_request")
                });
            (request
                && matches!(
                    name,
                    b"params"
                        | b"parameters"
                        | b"path_parameters"
                        | b"query_parameters"
                        | b"request_parameters"
                        | b"GET"
                        | b"POST"
                ))
                || (controller && name == b"params")
        }
    }
}

struct Writes<'s> {
    source: &'s str,
    /// Instance variables the file assigns the params to.
    ivars: BTreeSet<String>,
    /// What each local holds, innermost scope last.
    scopes: Vec<HashMap<String, Held>>,
    /// The name of each `def` being read, innermost last.
    defs: Vec<String>,
    /// The class or module bodies being read, each name as written, innermost last.
    owners: Vec<String>,
    found: BTreeMap<Vec<String>, Touched>,
}

impl Writes<'_> {
    /// What the body being read writes into: its class's or module's, or the file's outside any.
    fn here(&mut self) -> &mut Touched {
        self.found.entry(self.owners.clone()).or_default()
    }

    fn held(&self, node: &Node<'_>) -> Option<Held> {
        if is_request(node) {
            return Some(Held::Whole);
        }
        if let Some(local) = node.as_local_variable_read_node() {
            let name = String::from_utf8_lossy(local.name().as_slice()).into_owned();
            return self.scopes.last()?.get(&name).copied();
        }
        if let Some(ivar) = node.as_instance_variable_read_node() {
            let name = String::from_utf8_lossy(ivar.name().as_slice()).into_owned();
            return self.ivars.contains(&name).then_some(Held::Whole);
        }
        let call = node.as_call_node()?;
        let held = self.held(&call.receiver()?)?;
        READING
            .contains(&name_of(&call).as_str())
            .then_some(match held {
                Held::Whole | Held::Nested => Held::Nested,
                held => held,
            })
    }

    /// Whether a value holds only what the request's params hold: one of them, or what a copy,
    /// a filter or a value taken out of one hands back.
    fn derived(&self, node: &Node<'_>) -> bool {
        if matches!(
            self.held(node),
            Some(Held::Whole | Held::Nested | Held::Copied)
        ) {
            return true;
        }
        node.as_call_node().is_some_and(|call| {
            COPYING.contains(&name_of(&call).as_str())
                && call
                    .receiver()
                    .is_some_and(|receiver| self.derived(&receiver))
        })
    }

    /// The classes a value written is, once read back off the params, or `None` where the text
    /// does not say.
    fn classes(&self, node: &Node<'_>) -> Option<BTreeSet<&'static str>> {
        if self.derived(node) {
            return Some(BTreeSet::from([INSIDE]));
        }
        if let Some(call) = node.as_call_node()
            && let Some(receiver) = call.receiver()
            && self.derived(&receiver)
        {
            let name = name_of(&call);
            let (_, class) = CONVERTING.iter().find(|(method, _)| *method == name)?;
            return Some(BTreeSet::from([*class]));
        }
        // Whichever branch ran: each one's value, and `nil` for a branch nobody wrote.
        let mut classes = BTreeSet::new();
        let mut branches: Vec<Node<'_>> = Vec::new();
        if let Some(conditional) = node.as_if_node() {
            branches.push(last_statement(conditional.statements())?);
            match conditional.subsequent() {
                Some(other) => branches.push(other),
                None => {
                    classes.insert("nil");
                }
            }
        } else if let Some(conditional) = node.as_unless_node() {
            branches.push(last_statement(conditional.statements())?);
            match conditional.else_clause() {
                Some(other) => branches.push(other.as_node()),
                None => {
                    classes.insert("nil");
                }
            }
        } else if let Some(other) = node.as_else_node() {
            branches.push(last_statement(other.statements())?);
        } else if let Some(either) = node.as_or_node() {
            branches.extend([either.left(), either.right()]);
        } else if let Some(both) = node.as_and_node() {
            // The left side stands only where it is falsy: `nil` or `false`.
            classes.extend(["nil", "bool"]);
            branches.push(both.right());
        } else if let Some(group) = node.as_parentheses_node() {
            branches.push(group.body().and_then(|body| {
                body.as_statements_node()
                    .and_then(|statements| statements.body().iter().last())
            })?);
        } else {
            return Some(BTreeSet::from([literal_class(node)?]));
        }
        for branch in branches {
            classes.extend(self.classes(&branch)?);
        }
        Some(classes)
    }

    /// A write of `value` under `key` into what `receiver` holds.
    fn written(
        &mut self,
        receiver: Option<&Node<'_>>,
        key: Option<String>,
        value: Option<&Node<'_>>,
    ) {
        let Some(held) = receiver.and_then(|receiver| self.held(receiver)) else {
            return;
        };
        match held {
            Held::Whole => {
                let classes = value.and_then(|value| self.classes(value));
                self.here().nested |= value.is_some_and(holds_values);
                self.here().write(key, classes);
            }
            Held::Nested => self.here().nested = true,
            Held::Handed => self.mutated(),
            Held::Copied => {}
        }
    }

    /// The pairs a merge into the params writes: each key the text names with its value's
    /// classes, a splat or a non-literal key as a key it cannot name, and a whole argument the
    /// text cannot read as every key.
    fn merged(&mut self, arguments: &[Node<'_>]) {
        for argument in arguments {
            let elements: Vec<Node<'_>> = if let Some(hash) = argument.as_keyword_hash_node() {
                hash.elements().iter().collect()
            } else if let Some(hash) = argument.as_hash_node() {
                hash.elements().iter().collect()
            } else {
                let classes = self.derived(argument).then(|| BTreeSet::from([INSIDE]));
                self.here().write(None, classes);
                continue;
            };
            for element in elements {
                if let Some(assoc) = element.as_assoc_node() {
                    let key = literal_key(&assoc.key());
                    let classes = self.classes(&assoc.value());
                    self.here().nested |= holds_values(&assoc.value());
                    self.here().write(key, classes);
                } else {
                    let classes = element
                        .as_assoc_splat_node()
                        .and_then(|splat| splat.value())
                        .is_some_and(|value| self.derived(&value))
                        .then(|| BTreeSet::from([INSIDE]));
                    self.here().write(None, classes);
                }
            }
        }
    }

    /// The `def` being read writes into one of its parameters.
    fn mutated(&mut self) {
        // A parameter is only ever held inside the `def` it belongs to.
        let name = self.defs.last().cloned().unwrap_or_default();
        self.here().mutators.insert(name);
    }

    /// The innermost scope: the file's own one never leaves the bottom.
    fn scope(&mut self) -> &mut HashMap<String, Held> {
        let innermost = self.scopes.len() - 1;
        &mut self.scopes[innermost]
    }

    fn bind(&mut self, name: &[u8], held: Option<Held>) {
        let name = String::from_utf8_lossy(name).into_owned();
        match held {
            Some(held) => {
                self.scope().insert(name, held);
            }
            None => {
                self.scope().remove(&name);
            }
        }
    }

    fn call(&mut self, node: &CallNode<'_>) {
        let name = name_of(node);
        let arguments: Vec<Node<'_>> = node
            .arguments()
            .map(|arguments| arguments.arguments().iter().collect())
            .unwrap_or_default();
        let receiver = node.receiver();
        // `self.params = …` and `params=` replace the object.
        if name == "params="
            && receiver
                .as_ref()
                .is_none_or(|receiver| receiver.as_self_node().is_some())
        {
            self.here().everything = true;
        }
        let held = receiver.as_ref().and_then(|receiver| self.held(receiver));
        let mutating = |method: &str| {
            REPLACING.contains(&method)
                || MERGING.contains(&method)
                || (method.ends_with('!') && !REMOVING.contains(&method))
        };
        match (held, name.as_str()) {
            (Some(_), "[]=" | "store") => {
                let key = arguments.first().and_then(literal_key);
                let value = (arguments.len() == 2).then(|| &arguments[1]);
                self.written(receiver.as_ref(), key, value);
            }
            (Some(Held::Whole), method) if MERGING.contains(&method) => self.merged(&arguments),
            // `instance_eval` makes the params `self`, which this reader does not follow.
            (Some(Held::Whole), method)
                if mutating(method) || matches!(method, "instance_eval" | "instance_exec") =>
            {
                self.here().everything = true;
            }
            (Some(Held::Nested), method) if mutating(method) || method == "<<" => {
                self.here().nested = true;
            }
            (Some(Held::Handed), method) if mutating(method) => self.mutated(),
            _ => {}
        }
        // Handed on: the callee may write into what it is given.
        let mut passed: Vec<Option<Held>> = Vec::new();
        for argument in &arguments {
            match argument.as_keyword_hash_node() {
                Some(hash) => {
                    for element in hash.elements().iter() {
                        if let Some(assoc) = element.as_assoc_node() {
                            passed.push(self.held(&assoc.value()));
                        }
                    }
                }
                None => passed.push(self.held(argument)),
            }
        }
        let callee = if name == "new" { "initialize" } else { &name };
        if passed.contains(&Some(Held::Whole)) {
            let callee = callee.to_owned();
            self.here().handed.insert(callee);
        }
        if passed.contains(&Some(Held::Nested)) {
            let callee = callee.to_owned();
            self.here().handed_nested.insert(callee);
        }
    }

    /// A body named `name` (`class`, `module`), read with it as the owner of what it writes.
    fn owned(&mut self, name: &Node<'_>, body: impl FnOnce(&mut Self)) {
        self.owners.push(constant_spelling(self.source, name));
        let outer = std::mem::replace(&mut self.scopes, vec![HashMap::new()]);
        body(self);
        self.scopes = outer;
        self.owners.pop();
    }
}

/// Whether a value written under a key holds values of its own the text writes, in any branch: a
/// hash or an array literal with something in it. What a read through several keys finds below
/// that key is then those values, not what a request carries.
fn holds_values(node: &Node<'_>) -> bool {
    if let Some(hash) = node.as_hash_node() {
        return hash.elements().iter().next().is_some();
    }
    if let Some(array) = node.as_array_node() {
        return array.elements().iter().next().is_some();
    }
    let branches: Vec<Node<'_>> = if let Some(conditional) = node.as_if_node() {
        last_statement(conditional.statements())
            .into_iter()
            .chain(conditional.subsequent())
            .collect()
    } else if let Some(conditional) = node.as_unless_node() {
        last_statement(conditional.statements())
            .into_iter()
            .chain(conditional.else_clause().map(|other| other.as_node()))
            .collect()
    } else if let Some(other) = node.as_else_node() {
        last_statement(other.statements()).into_iter().collect()
    } else if let Some(either) = node.as_or_node() {
        vec![either.left(), either.right()]
    } else if let Some(both) = node.as_and_node() {
        vec![both.right()]
    } else if let Some(group) = node.as_parentheses_node() {
        group
            .body()
            .and_then(|body| body.as_statements_node())
            .and_then(|statements| statements.body().iter().last())
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };
    branches.iter().any(holds_values)
}

/// The last statement of a branch, its value.
fn last_statement(statements: Option<ruby_prism::StatementsNode<'_>>) -> Option<Node<'_>> {
    statements?.body().iter().last()
}

impl<'pr> Visit<'pr> for Writes<'_> {
    fn visit_class_node(&mut self, node: &ruby_prism::ClassNode<'pr>) {
        let name = node.constant_path();
        self.owned(&name, |this| ruby_prism::visit_class_node(this, node));
    }

    fn visit_module_node(&mut self, node: &ruby_prism::ModuleNode<'pr>) {
        let name = node.constant_path();
        self.owned(&name, |this| ruby_prism::visit_module_node(this, node));
    }

    fn visit_def_node(&mut self, node: &ruby_prism::DefNode<'pr>) {
        // Each named parameter, a destructured one aside.
        let names: Vec<String> = node
            .parameters()
            .map(|parameters| {
                let name = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
                parameters
                    .requireds()
                    .iter()
                    .filter_map(|required| {
                        required
                            .as_required_parameter_node()
                            .map(|required| name(required.name().as_slice()))
                    })
                    .chain(parameters.optionals().iter().filter_map(|optional| {
                        optional
                            .as_optional_parameter_node()
                            .map(|optional| name(optional.name().as_slice()))
                    }))
                    .chain(parameters.keywords().iter().filter_map(|keyword| {
                        keyword
                            .as_required_keyword_parameter_node()
                            .map(|keyword| name(keyword.name().as_slice()))
                            .or_else(|| {
                                keyword
                                    .as_optional_keyword_parameter_node()
                                    .map(|keyword| name(keyword.name().as_slice()))
                            })
                    }))
                    .collect()
            })
            .unwrap_or_default();
        self.scopes
            .push(names.into_iter().map(|name| (name, Held::Handed)).collect());
        self.defs
            .push(String::from_utf8_lossy(node.name().as_slice()).into_owned());
        ruby_prism::visit_def_node(self, node);
        self.defs.pop();
        self.scopes.pop();
    }

    fn visit_block_node(&mut self, node: &ruby_prism::BlockNode<'pr>) {
        // A block parameter is a new name for whatever the block is handed, which is not read here:
        // it hides a local of the same name for the block.
        let mut scope = self.scopes.last().cloned().unwrap_or_default();
        for name in block_parameters(node) {
            scope.remove(String::from_utf8_lossy(&name).as_ref());
        }
        self.scopes.push(scope);
        ruby_prism::visit_block_node(self, node);
        let inner = self.scopes.pop().unwrap_or_default();
        // A local the block assigned to the params keeps holding them where the scope around it
        // has one of that name. One the block made is gone after it, and Ruby reads the name there
        // as a call, never as this local: keeping it costs nothing.
        let outer = self.scope();
        for (name, held) in inner {
            outer.entry(name).or_insert(held);
        }
    }

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        self.call(node);
        // `params.tap { |all| … }` hands the block the params themselves, and `params.each
        // { |key, value| … }` each value under a key.
        let handing = node
            .receiver()
            .filter(|receiver| self.held(receiver) == Some(Held::Whole))
            .and_then(|_| match node.name().as_slice() {
                b"tap" | b"then" | b"yield_self" => Some((0, Held::Whole)),
                b"each" | b"each_pair" => Some((1, Held::Nested)),
                b"each_value" => Some((0, Held::Nested)),
                _ => None,
            });
        if let Some((at, held)) = handing
            && let Some(block) = node.block().and_then(|block| block.as_block_node())
            && let Some(name) = block_parameters(&block).into_iter().nth(at)
        {
            node.receiver()
                .iter()
                .for_each(|receiver| self.visit(receiver));
            node.arguments()
                .iter()
                .for_each(|arguments| self.visit_arguments_node(arguments));
            let mut scope = self.scopes.last().cloned().unwrap_or_default();
            for other in block_parameters(&block) {
                scope.remove(String::from_utf8_lossy(&other).as_ref());
            }
            scope.insert(String::from_utf8_lossy(&name).into_owned(), held);
            self.scopes.push(scope);
            block.body().iter().for_each(|body| self.visit(body));
            self.scopes.pop();
            return;
        }
        ruby_prism::visit_call_node(self, node);
    }

    fn visit_local_variable_write_node(&mut self, node: &ruby_prism::LocalVariableWriteNode<'pr>) {
        let value = node.value();
        let held = self
            .held(&value)
            .or_else(|| self.derived(&value).then_some(Held::Copied));
        self.bind(node.name().as_slice(), held);
        ruby_prism::visit_local_variable_write_node(self, node);
    }

    fn visit_local_variable_or_write_node(
        &mut self,
        node: &ruby_prism::LocalVariableOrWriteNode<'pr>,
    ) {
        if let Some(held) = self.held(&node.value()) {
            self.bind(node.name().as_slice(), Some(held));
        }
        ruby_prism::visit_local_variable_or_write_node(self, node);
    }

    fn visit_instance_variable_write_node(
        &mut self,
        node: &ruby_prism::InstanceVariableWriteNode<'pr>,
    ) {
        // Rails keeps the params it built in `@_params`.
        if node.name().as_slice() == b"@_params" {
            self.here().everything = true;
        }
        ruby_prism::visit_instance_variable_write_node(self, node);
    }

    fn visit_index_or_write_node(&mut self, node: &ruby_prism::IndexOrWriteNode<'pr>) {
        let key = first_key(node.arguments());
        let value = node.value();
        self.written(node.receiver().as_ref(), key, Some(&value));
        ruby_prism::visit_index_or_write_node(self, node);
    }

    fn visit_index_and_write_node(&mut self, node: &ruby_prism::IndexAndWriteNode<'pr>) {
        let key = first_key(node.arguments());
        let value = node.value();
        self.written(node.receiver().as_ref(), key, Some(&value));
        ruby_prism::visit_index_and_write_node(self, node);
    }

    fn visit_index_operator_write_node(&mut self, node: &ruby_prism::IndexOperatorWriteNode<'pr>) {
        // `params[:n] += 1`: what the operator makes is not read.
        let key = first_key(node.arguments());
        self.written(node.receiver().as_ref(), key, None);
        ruby_prism::visit_index_operator_write_node(self, node);
    }

    fn visit_index_target_node(&mut self, node: &ruby_prism::IndexTargetNode<'pr>) {
        // `params[:a], b = …`: the value is one position of what is assigned, not read.
        let key = first_key(node.arguments());
        self.written(Some(&node.receiver()), key, None);
        ruby_prism::visit_index_target_node(self, node);
    }

    fn visit_call_or_write_node(&mut self, node: &ruby_prism::CallOrWriteNode<'pr>) {
        self.replaced(node.receiver(), node.write_name().as_slice());
        ruby_prism::visit_call_or_write_node(self, node);
    }

    fn visit_call_and_write_node(&mut self, node: &ruby_prism::CallAndWriteNode<'pr>) {
        self.replaced(node.receiver(), node.write_name().as_slice());
        ruby_prism::visit_call_and_write_node(self, node);
    }

    fn visit_call_operator_write_node(&mut self, node: &ruby_prism::CallOperatorWriteNode<'pr>) {
        self.replaced(node.receiver(), node.write_name().as_slice());
        ruby_prism::visit_call_operator_write_node(self, node);
    }
}

impl Writes<'_> {
    /// `self.params ||= …` and its kin replace the object.
    fn replaced(&mut self, receiver: Option<Node<'_>>, write: &[u8]) {
        if write == b"params=" && receiver.is_none_or(|receiver| receiver.as_self_node().is_some())
        {
            self.here().everything = true;
        }
    }
}

/// The names a block's required parameters bind, in order.
fn block_parameters(block: &ruby_prism::BlockNode<'_>) -> Vec<Vec<u8>> {
    block
        .parameters()
        .and_then(|parameters| parameters.as_block_parameters_node())
        .and_then(|parameters| parameters.parameters())
        .map(|list| {
            list.requireds()
                .iter()
                .filter_map(|required| {
                    required
                        .as_required_parameter_node()
                        .map(|required| required.name().as_slice().to_vec())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The literal key an index write names, or `None` for any other key or for several.
fn first_key(arguments: Option<ruby_prism::ArgumentsNode<'_>>) -> Option<String> {
    let arguments = arguments?;
    let mut all = arguments.arguments().iter();
    let first = all.next()?;
    all.next().is_none().then(|| literal_key(&first)).flatten()
}

/// What a routes file says a key holds where the path leaves it out: every `key: value` it writes.
///
/// Rails keeps a route's `defaults:`, and any option it does not know (`type: :admin`), as a path
/// parameter of the Ruby value written, which it never turns into a `String`. Which option is
/// which is Rails' table, so every pair counts: a pair that is no default only widens a key nobody
/// reads. A constraint holds nothing: a `Regexp`, the pairs a `constraints` call or a
/// `constraints:` option is given. A value that is no literal refuses its key, and a `**` splat, or
/// a `defaults` given anything but a hash literal, refuses them all.
///
/// `whole` reads every statement, for a routes file and a file it draws. Otherwise only the blocks
/// handed to a route set's `draw`, `append` or `prepend` are routes: a plugin's or an engine's file
/// writes them beside code of its own.
#[must_use]
pub fn read_route_values(source: &str, whole: bool) -> Touched {
    struct Values {
        /// How many route blocks the walk is inside, or one for a whole routes file.
        inside: u32,
        found: Touched,
    }
    impl<'pr> Visit<'pr> for Values {
        fn visit_assoc_node(&mut self, node: &ruby_prism::AssocNode<'pr>) {
            let value = node.value();
            let key = literal_key(&node.key());
            // A constraint's pairs say what a segment must match, never what it holds.
            if key.as_deref() == Some("constraints") {
                return;
            }
            if self.inside > 0
                && let Some(key) = key
            {
                if key == "defaults" && value.as_hash_node().is_none() {
                    self.found.everything = true;
                } else if value.as_regular_expression_node().is_none()
                    && value.as_interpolated_regular_expression_node().is_none()
                {
                    let classes = literal_class(&value).map(|class| BTreeSet::from([class]));
                    self.found.write(Some(key), classes);
                }
            }
            ruby_prism::visit_assoc_node(self, node);
        }

        fn visit_assoc_splat_node(&mut self, node: &ruby_prism::AssocSplatNode<'pr>) {
            self.found.everything |= self.inside > 0;
            ruby_prism::visit_assoc_splat_node(self, node);
        }

        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            let drawn = matches!(node.name().as_slice(), b"draw" | b"append" | b"prepend")
                && node.receiver().is_some_and(|receiver| {
                    receiver
                        .as_call_node()
                        .is_some_and(|routes| routes.name().as_slice() == b"routes")
                });
            if drawn {
                self.inside += 1;
                ruby_prism::visit_call_node(self, node);
                self.inside -= 1;
                return;
            }
            if node.name().as_slice() == b"constraints" && node.receiver().is_none() {
                if let Some(block) = node.block() {
                    self.visit(&block);
                }
                return;
            }
            if self.inside > 0
                && node.name().as_slice() == b"defaults"
                && node.receiver().is_none()
                && node.arguments().is_some_and(|arguments| {
                    arguments.arguments().iter().any(|argument| {
                        argument.as_keyword_hash_node().is_none()
                            && argument.as_hash_node().is_none()
                    })
                })
            {
                self.found.everything = true;
            }
            ruby_prism::visit_call_node(self, node);
        }
    }
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut values = Values {
        inside: u32::from(whole),
        found: Touched::default(),
    };
    values.visit(&parsed.node());
    values.found
}

/// Whether a file gives the request a parser of its own, or turns on `parse_json_times`, either of
/// which can put any class in the params.
///
/// - **Only a write counts**: actionpack's own `self.parameter_parsers = DEFAULT_PARSERS` sets the
///   default, and activesupport's `mattr_accessor :parse_json_times` and a read of either do not.
/// - **A parser that only decodes JSON adds nothing** ([`decodes_json`]): what `JSON.parse` hands
///   back is what Rails' own JSON parser does.
#[must_use]
pub fn read_request_settings(source: &str) -> bool {
    struct Settings<'pr> {
        custom: bool,
        /// Every parser the file registers, as written.
        parsers: Vec<Node<'pr>>,
        /// What each method the file defines with no parameters hands back: its last statement.
        defs: HashMap<Vec<u8>, Node<'pr>>,
    }
    fn on_parsers(receiver: Option<Node<'_>>) -> bool {
        receiver.is_some_and(|receiver| {
            receiver
                .as_call_node()
                .is_some_and(|call| call.name().as_slice() == b"parameter_parsers")
        })
    }
    impl<'pr> Settings<'pr> {
        /// The parsers a hash literal registers, or `custom` where the argument is anything else.
        fn registered(&mut self, value: Node<'pr>) {
            let elements: Vec<Node<'pr>> = if let Some(hash) = value.as_keyword_hash_node() {
                hash.elements().iter().collect()
            } else if let Some(hash) = value.as_hash_node() {
                hash.elements().iter().collect()
            } else {
                self.custom = true;
                return;
            };
            for element in elements {
                match element.as_assoc_node() {
                    Some(assoc) => self.parsers.push(assoc.value()),
                    None => self.custom = true,
                }
            }
        }
    }
    impl<'pr> Visit<'pr> for Settings<'pr> {
        fn visit_def_node(&mut self, node: &ruby_prism::DefNode<'pr>) {
            if node.parameters().is_none()
                && let Some(last) = node.body().and_then(|body| {
                    body.as_statements_node()
                        .and_then(|statements| statements.body().iter().last())
                })
            {
                self.defs.insert(node.name().as_slice().to_vec(), last);
            }
            ruby_prism::visit_def_node(self, node);
        }

        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            let mut arguments: Vec<Node<'pr>> = node
                .arguments()
                .map(|arguments| arguments.arguments().iter().collect())
                .unwrap_or_default();
            match node.name().as_slice() {
                b"parameter_parsers=" => {
                    let default = arguments.last().is_some_and(|value| {
                        value.as_constant_read_node().is_some_and(|constant| {
                            constant.name().as_slice() == b"DEFAULT_PARSERS"
                        })
                    });
                    if !default {
                        // Ruby always writes the value, a missing one included.
                        arguments
                            .pop()
                            .into_iter()
                            .for_each(|value| self.registered(value));
                    }
                }
                b"parse_json_times=" => {
                    let off = arguments.last().is_some_and(|value| {
                        value.as_false_node().is_some() || value.as_nil_node().is_some()
                    });
                    self.custom |= !off;
                }
                b"[]=" | b"store" if on_parsers(node.receiver()) => match arguments.pop() {
                    Some(value) if arguments.len() == 1 => self.parsers.push(value),
                    _ => self.custom = true,
                },
                b"merge!" | b"update" if on_parsers(node.receiver()) => {
                    for argument in arguments {
                        self.registered(argument);
                    }
                }
                _ => {}
            }
            ruby_prism::visit_call_node(self, node);
        }

        fn visit_index_or_write_node(&mut self, node: &ruby_prism::IndexOrWriteNode<'pr>) {
            if on_parsers(node.receiver()) {
                self.parsers.push(node.value());
            }
            ruby_prism::visit_index_or_write_node(self, node);
        }
    }
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut settings = Settings {
        custom: false,
        parsers: Vec::new(),
        defs: HashMap::new(),
    };
    settings.visit(&parsed.node());
    settings.custom
        || settings
            .parsers
            .iter()
            .any(|parser| !decodes_json(parser, &settings.defs, true))
}

/// Whether a parser is a lambda or proc that only decodes JSON: every call in its body is
/// `JSON.parse` of one argument, `ActiveSupport::JSON.decode`, or one of [`JSON_SHAPING`]. A method
/// of the file with no parameters is read for the lambda it hands back, one step deep.
fn decodes_json(parser: &Node<'_>, defs: &HashMap<Vec<u8>, Node<'_>>, follow: bool) -> bool {
    if let Some(lambda) = parser.as_lambda_node() {
        return lambda.body().is_some_and(|body| only_json(&body));
    }
    let Some(call) = parser.as_call_node() else {
        return false;
    };
    let name = call.name();
    let name = name.as_slice();
    if matches!(name, b"lambda" | b"proc") && call.receiver().is_none() {
        return call
            .block()
            .and_then(|block| block.as_block_node())
            .and_then(|block| block.body())
            .is_some_and(|body| only_json(&body));
    }
    follow
        && call.arguments().is_none()
        && call.block().is_none()
        && call
            .receiver()
            .is_none_or(|receiver| receiver.as_self_node().is_some())
        && defs
            .get(name)
            .is_some_and(|returned| decodes_json(returned, defs, false))
}

/// Methods a JSON parser may call on what it decoded: each hands back the same values, under other
/// keys or wrapped in a hash.
const JSON_SHAPING: [&str; 7] = [
    "is_a?",
    "with_indifferent_access",
    "to_h",
    "symbolize_keys",
    "deep_symbolize_keys",
    "stringify_keys",
    "deep_stringify_keys",
];

/// Whether a body decodes JSON, and every call in it is a decode or one of [`JSON_SHAPING`].
fn only_json(body: &Node<'_>) -> bool {
    struct Calls {
        only: bool,
        decoded: bool,
    }
    impl<'pr> Visit<'pr> for Calls {
        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            let name = name_of(node);
            let on = node
                .receiver()
                .map(|receiver| {
                    let location = receiver.location();
                    String::from_utf8_lossy(location.as_slice()).into_owned()
                })
                .unwrap_or_default();
            let positional = node
                .arguments()
                .map_or(0, |arguments| arguments.arguments().iter().count());
            let decoding = matches!(
                (on.trim_start_matches("::"), name.as_str()),
                ("JSON", "parse") | ("ActiveSupport::JSON", "decode")
            ) && positional == 1;
            self.decoded |= decoding;
            self.only &= decoding || JSON_SHAPING.contains(&name.as_str());
            ruby_prism::visit_call_node(self, node);
        }
    }
    let mut calls = Calls {
        only: true,
        decoded: false,
    };
    calls.visit(body);
    calls.only && calls.decoded
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// One file's writes, wherever in it they sit.
    fn flat(source: &str) -> Touched {
        let mut touched = Touched::default();
        for owned in read_request_writes(source).values() {
            touched.absorb(owned);
        }
        touched
    }

    fn keys(touched: &Touched) -> Vec<(String, Vec<Option<&'static str>>)> {
        touched
            .keys
            .iter()
            .map(|(key, classes)| (key.clone(), classes.iter().copied().collect()))
            .collect()
    }

    fn asked(method: &str, keys: &[Option<&str>], touched: &Touched) -> Option<String> {
        let keys: Vec<Option<String>> = keys.iter().map(|key| key.map(str::to_owned)).collect();
        request_value(
            &Asked {
                method,
                keys: &keys,
                block: false,
                proven: None,
            },
            touched,
        )
    }

    const UNION: &str = "String | Integer | Float | bool | Array[untyped] | \
                         ActionController::Parameters | ActionDispatch::Http::UploadedFile";

    const SCALARS: &str = "String | Integer | Float | bool | ActionDispatch::Http::UploadedFile";

    fn name(text: &str) -> Spelled {
        Spelled::Name(text.to_owned())
    }

    fn pairs(pairs: &[(&str, Spelled)]) -> Spelled {
        Spelled::Pairs(
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        )
    }

    /// Each key a filter names, as Rails reads `permit(*filters)`: the list flattened, a name a
    /// scalar, each pair of a hash by what its value is.
    #[test]
    fn a_filter_is_read_as_rails_reads_permit() {
        let written = [
            name("title"),
            Spelled::List(vec![name("body"), Spelled::List(vec![name("deep")])]),
            pairs(&[
                ("tags", Spelled::List(Vec::new())),
                ("meta", pairs(&[])),
                (
                    "comments",
                    Spelled::List(vec![Spelled::List(vec![name("text")])]),
                ),
                (
                    "address",
                    Spelled::List(vec![
                        name("street"),
                        pairs(&[("geo", Spelled::List(Vec::new()))]),
                    ]),
                ),
                ("owner", name("id")),
                ("place", pairs(&[("city", name("name"))])),
            ]),
        ];
        assert_eq!(
            filters(&written),
            vec![
                ("title".to_owned(), Filter::Scalar),
                ("body".to_owned(), Filter::Scalar),
                ("deep".to_owned(), Filter::Scalar),
                ("tags".to_owned(), Filter::Scalars),
                ("meta".to_owned(), Filter::Any),
                ("comments".to_owned(), Filter::Each),
                (
                    "address".to_owned(),
                    Filter::Nested(vec![
                        ("street".to_owned(), Filter::Scalar),
                        ("geo".to_owned(), Filter::Scalars),
                    ])
                ),
                (
                    "owner".to_owned(),
                    Filter::Nested(vec![("id".to_owned(), Filter::Scalar)])
                ),
                (
                    "place".to_owned(),
                    Filter::Nested(vec![(
                        "city".to_owned(),
                        Filter::Nested(vec![("name".to_owned(), Filter::Scalar)])
                    )])
                ),
            ]
        );
        // Two lists are no `[[…]]`: a hash filtered by both.
        assert!(matches!(
            filter_of(&Spelled::List(vec![Spelled::List(Vec::new()), name("a")])),
            Filter::Nested(_)
        ));
    }

    /// `expect` hands back one key's value, required, and several keys' values in an array; one
    /// bare name is its `(Symbol)` arm's.
    #[test]
    fn expect_hands_back_one_key_s_value_or_an_array_of_several() {
        let one = |filter: Filter| vec![("post".to_owned(), filter)];
        assert_eq!(expected(&[]), None);
        assert_eq!(expected(&one(Filter::Scalar)), None);
        assert_eq!(
            expected(&one(Filter::Scalars)),
            Some(("Array[untyped]", None))
        );
        assert_eq!(expected(&one(Filter::Any)), Some((PARAMETERS, None)));
        assert_eq!(
            expected(&one(Filter::Each)),
            Some((ARRAY_OR_PARAMETERS, None))
        );
        let inner = vec![("title".to_owned(), Filter::Scalar)];
        let nested = one(Filter::Nested(inner.clone()));
        assert_eq!(
            expected(&nested),
            Some((PARAMETERS, Some(("post", inner.as_slice()))))
        );
        let several = vec![
            ("id".to_owned(), Filter::Scalar),
            ("post".to_owned(), Filter::Any),
        ];
        assert_eq!(expected(&several), Some(("Array[untyped]", None)));
    }

    fn reads(
        path: &[&str],
        filters: &[(String, Filter)],
        explicit: bool,
        touched: &Touched,
    ) -> Vec<(String, String, String, String)> {
        let path: Vec<String> = path.iter().map(|key| (*key).to_owned()).collect();
        permitted_reads(
            &Permitted {
                path: &path,
                filters,
                explicit,
            },
            touched,
        )
        .into_iter()
        .map(|read| (read.key, read.read, read.fetched, read.required))
        .collect()
    }

    /// What each permitted key hands back: a scalar filter the scalars, `nil` where absent and
    /// where a scalar was sent as one; `[]` an array; `{}` a hash; `[[…]]` an array or a hash of
    /// hashes; a filtered hash a hash, or under `permit` an array of them too.
    #[test]
    fn a_permitted_key_holds_what_its_filter_keeps_of_the_request() {
        let touched = Touched::default();
        let filters = vec![
            ("title".to_owned(), Filter::Scalar),
            ("tags".to_owned(), Filter::Scalars),
            ("meta".to_owned(), Filter::Any),
            ("comments".to_owned(), Filter::Each),
            ("address".to_owned(), Filter::Nested(Vec::new())),
            // A key named twice is each: a scalar or an array.
            ("both".to_owned(), Filter::Scalar),
            ("both".to_owned(), Filter::Scalars),
            // The same class twice is one.
            ("twice".to_owned(), Filter::Scalars),
            ("twice".to_owned(), Filter::Scalars),
        ];
        let array = "Array[untyped]";
        let pair = "Array[untyped] | ActionController::Parameters";
        let row = |key: &str, read: &str, fetched: &str, required: &str| {
            (
                key.to_owned(),
                read.to_owned(),
                fetched.to_owned(),
                required.to_owned(),
            )
        };
        assert_eq!(
            reads(&[], &filters, false, &touched),
            vec![
                row("address", &format!("{pair} | nil"), pair, pair),
                row(
                    "both",
                    &format!("{SCALARS} | {array} | nil"),
                    &format!("{SCALARS} | {array} | nil"),
                    &format!("{SCALARS} | {array}")
                ),
                row("comments", &format!("{pair} | nil"), pair, pair),
                row(
                    "meta",
                    "ActionController::Parameters | nil",
                    "ActionController::Parameters",
                    "ActionController::Parameters"
                ),
                row("tags", &format!("{array} | nil"), array, array),
                row(
                    "title",
                    &format!("{SCALARS} | nil"),
                    &format!("{SCALARS} | nil"),
                    SCALARS
                ),
                row("twice", &format!("{array} | nil"), array, array),
            ]
        );
        // `expect` keeps a filtered hash a hash.
        let explicit = reads(
            &["post"],
            &[("address".to_owned(), Filter::Nested(Vec::new()))],
            true,
            &touched,
        );
        assert_eq!(
            explicit,
            vec![row(
                "address",
                "ActionController::Parameters | nil",
                "ActionController::Parameters",
                "ActionController::Parameters"
            )]
        );
    }

    /// What the code writes counts: a scalar under a key `permit` reads at the top joins it, a
    /// value the text cannot name leaves the key out, and a value written below a key refuses
    /// every read below one.
    #[test]
    fn a_permitted_key_joins_what_is_written_there_and_refuses_what_cannot_be_read() {
        let touched = flat(
            "class PostsController
  def set
    params[:locale] = :en
    params[:other] =              compute
  end
end
",
        );
        let filters = vec![
            ("locale".to_owned(), Filter::Scalar),
            ("other".to_owned(), Filter::Scalar),
            ("other".to_owned(), Filter::Scalars),
            ("title".to_owned(), Filter::Scalar),
        ];
        let keys: Vec<String> = reads(&[], &filters, false, &touched)
            .into_iter()
            .map(|(key, read, _, _)| format!("{key}: {read}"))
            .collect();
        assert_eq!(
            keys,
            [
                format!("locale: {SCALARS} | Symbol | nil"),
                format!("title: {SCALARS} | nil"),
            ]
        );
        let nested = flat(
            "class PostsController
  def set
    params[:post][:title] = 1
  end
end
",
        );
        assert!(reads(&["post"], &filters, false, &nested).is_empty());
        assert_eq!(reads(&[], &filters, false, &nested).len(), 3);
    }

    /// Writes count for reads under the class they are written in, and what descends from it.
    #[test]
    fn a_permitted_read_counts_the_writes_of_its_own_classes() {
        let mut requests = Requests::default();
        requests.absorb_writes(
            &read_request_writes(
                "class PostsController
  def set
    params[:locale] = :en
  end
end
",
            ),
            &|nesting| nesting.join("::"),
        );
        let filters = vec![("locale".to_owned(), Filter::Scalar)];
        let permitted = Permitted {
            path: &[],
            filters: &filters,
            explicit: false,
        };
        let read = |ancestors: &[&str]| {
            requests
                .permitted(&permitted, ancestors)
                .into_iter()
                .map(|read| read.read)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            read(&["PostsController"]),
            [format!("{SCALARS} | Symbol | nil")]
        );
        assert_eq!(read(&["UsersController"]), [format!("{SCALARS} | nil")]);
    }

    /// A hash or an array written with something in it, in any branch of the value, holds values
    /// the text writes: a read through several keys finds those below the key.
    #[test]
    fn a_literal_written_with_values_holds_them_below_its_key() {
        for (value, holds) in [
            ("{ a: 1 }", true),
            ("{}", false),
            ("[1]", true),
            ("[]", false),
            ("(flag ? { a: 1 } : nil)", true),
            ("(flag ? 1 : 2)", false),
            ("({ a: 1 } if flag)", true),
            ("(unless flag then 1 else [2] end)", true),
            ("(1 unless flag)", false),
            ("(x || [1])", true),
            ("(x && { a: 1 })", true),
            ("(1; [2])", true),
            ("()", false),
            ("compute", false),
        ] {
            let touched = flat(&format!(
                "class PostsController
  def set
    params[:key] = {value}
  end
end
"
            ));
            assert_eq!(touched.nested, holds, "{value}");
            let merged = flat(&format!(
                "class PostsController
  def set
    params.merge!(key: {value})
  end
end
"
            ));
            assert_eq!(merged.nested, holds, "merged {value}");
        }
    }

    #[test]
    fn a_read_off_untouched_params_is_the_request_union() {
        let touched = Touched::default();
        let maybe = Some(format!("{UNION} | nil"));
        assert_eq!(asked("[]", &[Some("id")], &touched), maybe);
        assert_eq!(asked("[]", &[None], &touched), maybe);
        assert_eq!(asked("fetch", &[Some("id")], &touched), maybe);
        assert_eq!(asked("dig", &[Some("a"), Some("b")], &touched), maybe);
        assert_eq!(
            asked("require", &[Some("post")], &touched),
            Some(UNION.to_owned())
        );
        assert_eq!(
            asked("expect", &[Some("id")], &touched).as_deref(),
            Some(SCALARS)
        );
        // Left to the method's own body: a default, two keys, a list, no key at all, a block.
        assert_eq!(asked("fetch", &[Some("id"), None], &touched), None);
        assert_eq!(asked("require", &[None], &touched), None);
        assert_eq!(asked("expect", &[Some("a"), Some("b")], &touched), None);
        assert_eq!(asked("dig", &[], &touched), None);
        assert_eq!(asked("[]", &[], &touched), None);
        assert_eq!(asked("permit", &[Some("a")], &touched), None);
        let keys = [Some("id".to_owned())];
        assert_eq!(
            request_value(
                &Asked {
                    method: "fetch",
                    keys: &keys,
                    block: true,
                    proven: None,
                },
                &touched
            ),
            None
        );
    }

    #[test]
    fn a_write_joins_the_class_its_value_is_and_anything_else_refuses_the_key() {
        let source = "\
class PostsController < ApplicationController
  before_action :set_locale

  def set_locale
    params[:locale] = :en
    params[:page] ||= 1
    params[\"kind\"] = \"draft\"
    self.params[:flag] = true
    params[:ratio] = 0.5
    params[:list] = []
    params[:map] = { a: 1 }
    params[:gone] = nil
    params[:copy] = params[:other]
    params[:when] = Time.current
    params[:count] += 1
    params.store(:stored, \"x\")
    params[:a], other = [1, 2]
    params[:s] = \"#{1}\"
    params[:t] = :\"#{1}\"
    params[:u] = `ls`
    params[:either] = flag ? :x : 1
    params[:unless] = (\"x\" unless flag)
    params[:or] = params[:x] || \"y\"
    params[:and] = flag && 2.0
    params[:grouped] = (1; :z)
    params[:empty] = ()
    params[:lone] = (\"a\" if flag)
    params[:text] = params[:q].to_s
    params[:number] = params.fetch(:n).to_i
    params[:taken] = params.delete(:other).presence
    params[:copied] = params.permit(:a).to_h
    params[:other_text] = other.to_s
    params[:unknown] = params[:q].frobnicate
    params.delete(:secret)
    params.permit!
    params.transform_keys!(&:to_s)
  end
end
";
        let touched = flat(source);
        // `{ a: 1 }` holds a value of its own under `map`, which a read through several keys finds.
        assert!(!touched.everything && touched.nested);
        assert_eq!(
            keys(&touched),
            [
                ("a".to_owned(), vec![None]),
                (
                    "and".to_owned(),
                    vec![Some("Float"), Some("bool"), Some("nil")]
                ),
                ("copied".to_owned(), vec![Some(INSIDE)]),
                ("copy".to_owned(), vec![Some(INSIDE)]),
                ("count".to_owned(), vec![None]),
                ("either".to_owned(), vec![Some("Integer"), Some("Symbol")]),
                ("empty".to_owned(), vec![None]),
                ("flag".to_owned(), vec![Some("bool")]),
                ("gone".to_owned(), vec![Some("nil")]),
                ("grouped".to_owned(), vec![Some("Symbol")]),
                ("kind".to_owned(), vec![Some("String")]),
                ("list".to_owned(), vec![Some("Array[untyped]")]),
                ("locale".to_owned(), vec![Some("Symbol")]),
                ("lone".to_owned(), vec![Some("String"), Some("nil")]),
                ("map".to_owned(), vec![Some("ActionController::Parameters")]),
                ("number".to_owned(), vec![Some("Integer")]),
                ("or".to_owned(), vec![Some(INSIDE), Some("String")]),
                ("other_text".to_owned(), vec![None]),
                ("page".to_owned(), vec![Some("Integer")]),
                ("ratio".to_owned(), vec![Some("Float")]),
                ("s".to_owned(), vec![Some("String")]),
                ("stored".to_owned(), vec![Some("String")]),
                ("t".to_owned(), vec![Some("Symbol")]),
                ("taken".to_owned(), vec![Some(INSIDE)]),
                ("text".to_owned(), vec![Some("String")]),
                ("u".to_owned(), vec![Some("String")]),
                ("unknown".to_owned(), vec![None]),
                ("unless".to_owned(), vec![Some("String"), Some("nil")]),
                ("when".to_owned(), vec![None]),
            ]
        );
        assert_eq!(
            asked("[]", &[Some("locale")], &touched),
            Some(format!("{UNION} | Symbol | nil"))
        );
        assert_eq!(
            asked("require", &[Some("gone")], &touched),
            Some(UNION.to_owned())
        );
        assert_eq!(
            asked("[]", &[Some("copy")], &touched),
            Some(format!("{UNION} | nil"))
        );
        assert_eq!(asked("[]", &[Some("when")], &touched), None);
        assert_eq!(asked("[]", &[None], &touched), None);
        assert_eq!(
            asked("expect", &[Some("map")], &touched).as_deref(),
            Some(SCALARS)
        );
        assert_eq!(
            asked("expect", &[Some("locale")], &touched),
            Some(format!("{SCALARS} | Symbol"))
        );
        // A key nobody wrote is the union, and so is any key while every write stays inside it.
        assert_eq!(
            asked("[]", &[Some("id")], &touched),
            Some(format!("{UNION} | nil"))
        );
        let inside = flat("params[:a] = \"x\"\nparams[:b] = params[:c]\n");
        assert_eq!(
            asked("[]", &[None], &inside),
            Some(format!("{UNION} | nil"))
        );
    }

    #[test]
    fn a_write_under_a_key_the_text_cannot_name_joins_every_key() {
        // What the value is still counts: `nil` and a request value add nothing, a symbol adds
        // itself to every key, and a value the text does not say refuses them all.
        let cleaned =
            flat("params.each { |key, value| params[key] = value == \"null\" ? nil : value }\n");
        assert!(!cleaned.everything, "{cleaned:?}");
        assert_eq!(
            asked("[]", &[Some("id")], &cleaned),
            Some(format!("{UNION} | nil"))
        );
        let tagged = flat("params[name] = :x\n");
        assert_eq!(
            asked("[]", &[Some("id")], &tagged),
            Some(format!("{UNION} | Symbol | nil"))
        );
        assert!(flat("params[name] = Time.now\n").everything);
        assert!(flat("params.each_value { |value| value[:a] = 1 }\n").nested);
        assert!(flat("params.each { |key| params[key] = Time.now }\n").everything);
    }

    #[test]
    fn a_merge_writes_each_pair_it_is_given() {
        let touched = flat(
            "\
tag = params.delete(:tag)
params.merge!(tag.permit!) if tag
params.reverse_merge!(page: 1, \"per\" => :x, **params.permit(:a))
params.update({ kind: Time.now })
",
        );
        assert!(!touched.everything, "{touched:?}");
        assert_eq!(
            keys(&touched),
            [
                ("kind".to_owned(), vec![None]),
                ("page".to_owned(), vec![Some("Integer")]),
                ("per".to_owned(), vec![Some("Symbol")]),
            ]
        );
        assert_eq!(touched.any, BTreeSet::from([INSIDE]));
        for source in [
            "params.merge!(other)\n",
            "params.merge!(key => 1, **options)\n",
            "params.deep_merge!(name => Time.now)\n",
        ] {
            assert!(flat(source).everything, "{source}");
        }
    }

    #[test]
    fn a_key_or_a_mutator_the_text_cannot_bound_refuses_every_key() {
        for source in [
            "params[name] = other\n",
            "params.deep_symbolize_keys!\n",
            "params.transform_values!(&:to_s)\n",
            "self.params = {}\n",
            "self.params ||= {}\n",
            "self.params &&= {}\n",
            "self.params += 1\n",
            "@_params = nil\n",
            "request.parameters[:a] = 1 if x\nrequest.parameters[key] = Time.now\n",
            "request.path_parameters.replace(a: 1)\n",
            "controller.params.merge!(other)\n",
            "@controller.params.update(other)\n",
            "def x\n  all = params\n  all.reverse_merge!(other)\nend\n",
            "def x\n  @all = params\nend\ndef y\n  @all.merge!(other)\nend\n",
            "def x\n  @all ||= params\nend\ndef y\n  @all[key] = Time.now\nend\n",
            "params.tap { |all| all.merge!(other) }\n",
            "params.then { |all| all[:a] = Time.now; all[key] = Time.now }\n",
            "params.instance_eval { self[:a] = 1 }\n",
            "params[:a] ||= x\nparams[k] ||= Time.now\n",
        ] {
            assert!(flat(source).everything, "{source}");
        }
        for source in [
            "params.except!(:a)\nparams.permit!\nparams.delete(:b)\n",
            "params.merge(a: 1)[:a] = Time.now\n",
            "params.permit(:a)[:a] = Time.now\n",
            "other[:a] = 1\nsomething.params[:a] = 1\nrequest.env[:a] = 1\n",
            "params.tap { |all| all }\nparams.tap(&:freeze)\nlist.tap { |all| all.merge!(a: 1) }\n",
            "params.each { |key| key }\nparams.tap { }\n",
            "def x(params = 1)\nend\n",
        ] {
            let touched = flat(source);
            assert!(!touched.everything && touched.keys.is_empty(), "{source}");
        }
    }

    #[test]
    fn a_write_under_a_key_refuses_a_dig_through_several_keys() {
        for source in [
            "params[:a][:b] = Time.now\n",
            "params.require(:a)[:b] = 1\n",
            "params.fetch(:a).merge!(b: 1)\n",
            "params[:tags] << Time.now\n",
            "def x\n  nested = params[:a]\n  nested[:b] = 1\nend\n",
            "def x\n  nested = params.dig(:a, :b)\n  nested[:c] ||= 1\nend\n",
        ] {
            let touched = flat(source);
            assert!(touched.nested && !touched.everything, "{source}");
            assert_eq!(asked("dig", &[Some("a"), Some("b")], &touched), None);
            assert_eq!(
                asked("dig", &[Some("a")], &touched),
                Some(format!("{UNION} | nil"))
            );
        }
        // A value read off them, handed to a method that writes into its parameter.
        let mut project = flat("tidy(params[:a])\n");
        assert_eq!(project.handed_nested, BTreeSet::from(["tidy".to_owned()]));
        assert!(asked("dig", &[Some("a"), Some("b")], &project).is_some());
        project.absorb(&flat("def tidy(raw)\n  raw[:b] = 1\nend\n"));
        assert_eq!(asked("dig", &[Some("a"), Some("b")], &project), None);
        assert!(asked("[]", &[Some("a")], &project).is_some());
    }

    #[test]
    fn a_local_or_a_block_parameter_of_another_value_is_not_the_params() {
        let source = "\
def x
  all = params
  all = other
  all[:a] = Time.now
  list.each { |all| all[:b] = Time.now }
  each_item do |all|
    copy = all
  end
end
";
        assert_eq!(flat(source), Touched::default());
        // A local written before a block and assigned in it holds what the block put there.
        let touched = flat("inner = nil\nrun { inner = params }\ninner[:a] = Time.now\n");
        assert_eq!(keys(&touched), [("a".to_owned(), vec![None])]);
        let shadowed = flat("all = params\nrun { |all| all[:a] = Time.now }\n");
        assert!(shadowed.keys.is_empty(), "{shadowed:?}");
    }

    #[test]
    fn params_handed_to_a_method_that_writes_into_its_parameter_refuse_every_key() {
        let caller = flat(
            "\
def create
  Normalizer.new(params).run
  sanitize(scope: params)
  helper(params[:a])
  params.permit(:a)
end
",
        );
        assert_eq!(
            caller.handed,
            BTreeSet::from(["initialize".to_owned(), "sanitize".to_owned()])
        );
        let callee = flat(
            "\
class Normalizer
  def initialize(raw, other = nil, flag: false, kind:)
    raw[:from] = Date.parse(raw[:from])
  end

  def sanitize(scope:)
    scope.merge!(a: 1)
  end

  def deep(raw)
    raw[:a].merge!(b: 1)
  end

  def ok(raw)
    raw.permit!
    raw.fetch(:a)
    local = 1
    local[:x] = 2
  end
end

def outside(raw)
  [1].each { |raw| raw[:a] = 1 }
end
",
        );
        assert_eq!(
            callee.mutators,
            BTreeSet::from([
                "deep".to_owned(),
                "initialize".to_owned(),
                "sanitize".to_owned()
            ])
        );
        let mut project = Touched::default();
        project.absorb(&caller);
        assert!(asked("[]", &[Some("id")], &project).is_some());
        project.absorb(&callee);
        assert_eq!(asked("[]", &[Some("id")], &project), None);
        // A method of another name writing into its parameter refuses nothing.
        let mut apart = flat("def other(raw)\n  raw[:a] = 1\nend\n");
        apart.absorb(&caller);
        assert!(asked("[]", &[Some("id")], &apart).is_some());
        // Written outside any `def`, a parameter is nobody's.
        assert!(flat("raw = params\n").mutators.is_empty());
    }

    #[test]
    fn a_write_counts_for_the_class_it_is_written_in_and_what_descends_from_it() {
        // The nesting resolves as the caller's lookup says: here every level joins its outer one,
        // and `Admin::PostsController` is written compact, its own name the owner.
        let resolve = |nesting: &[String]| nesting.join("::");
        let mut requests = Requests::default();
        requests.absorb_writes(
            &read_request_writes(
                "\
module Admin
  class BaseController < ApplicationController
    def set_at
      params[:at] = Time.now
    end
  end
end

module Tidy
  def tidy(raw)
    raw[:a] = 1
  end
end

class Admin::PostsController
  class << self
    def x
      params[:kind] = :admin
    end
  end
end

params[:top] = :t
",
            ),
            &resolve,
        );
        requests.absorb_routes(&read_route_values(
            "resources :posts, defaults: { format: :json }\n",
            true,
        ));
        let read = |requests: &Requests, key: &str, ancestors: &[&str]| {
            let keys = [Some(key.to_owned())];
            requests.value(
                &Asked {
                    method: "[]",
                    keys: &keys,
                    block: false,
                    proven: None,
                },
                ancestors,
            )
        };
        // The base class's write refuses the key in it and in what inherits it; elsewhere, a
        // class of the same last name included, the key is the union.
        assert_eq!(
            read(&requests, "at", &["Admin::BaseController", "Object"]),
            None
        );
        assert_eq!(
            read(
                &requests,
                "at",
                &["Admin::UsersController", "Admin::BaseController"]
            ),
            None
        );
        assert_eq!(
            read(
                &requests,
                "at",
                &["BaseController", "ApplicationController"]
            ),
            Some(format!("{UNION} | nil"))
        );
        // `Admin::PostsController` is written compact, its own name the owner.
        assert_eq!(
            read(&requests, "kind", &["Admin::PostsController"]),
            Some(format!("{UNION} | Symbol | nil"))
        );
        assert_eq!(
            read(&requests, "kind", &["Other"]),
            Some(format!("{UNION} | nil"))
        );
        // Outside any body, and the routes: every read's.
        assert_eq!(
            read(&requests, "top", &["Other"]),
            Some(format!("{UNION} | Symbol | nil"))
        );
        assert_eq!(
            read(&requests, "format", &[]),
            Some(format!("{UNION} | Symbol | nil"))
        );
        // A method writing into its parameter counts wherever it is written.
        requests.absorb_writes(
            &read_request_writes("class Use\n  def run\n    tidy(params)\n  end\nend\n"),
            &resolve,
        );
        assert_eq!(read(&requests, "id", &["Use"]), None);
        assert_eq!(
            read(&requests, "id", &["Other"]),
            Some(format!("{UNION} | nil"))
        );
    }

    #[test]
    fn a_route_s_pairs_join_their_literal_s_class_and_a_regexp_holds_nothing() {
        let source = "\
Rails.application.routes.draw do
  resources :posts, defaults: { format: :json }, constraints: { id: /\\d+/, slug: /#{x}/ }
  get \"feed\", to: \"feeds#show\", type: \"rss\", page: 1
  scope via: :all do
    get \"x\" => \"x#y\", as: :x
  end
  get \"admin\", to: AdminApp
end
";
        let touched = read_route_values(source, true);
        assert!(!touched.everything);
        assert_eq!(
            keys(&touched),
            [
                ("as".to_owned(), vec![Some("Symbol")]),
                (
                    "defaults".to_owned(),
                    vec![Some("ActionController::Parameters")]
                ),
                ("format".to_owned(), vec![Some("Symbol")]),
                ("page".to_owned(), vec![Some("Integer")]),
                ("to".to_owned(), vec![None, Some("String")]),
                ("type".to_owned(), vec![Some("String")]),
                ("via".to_owned(), vec![Some("Symbol")]),
                // The path-to-target pair is a pair like any other: a `String` widens nothing.
                ("x".to_owned(), vec![Some("String")]),
            ]
        );
        for source in [
            "resources :posts, defaults: DEFAULTS\n",
            "resources :posts, **options\n",
            "defaults(options) do\nend\n",
        ] {
            assert!(read_route_values(source, true).everything, "{source}");
        }
        assert!(!read_route_values("defaults format: :json do\nend\ndefaults\n", true).everything);
        assert!(!read_route_values("defaults({ format: :json }) do\nend\n", true).everything);
        // A constraint holds nothing, whatever it is written with.
        let constrained = read_route_values(
            "constraints id: PATTERN do\n  get \"x\", to: \"x#y\", page: 1\nend\nget \"y\", to: \"y#z\", constraints: { id: ID, format: :json }\n",
            true,
        );
        assert!(!constrained.everything);
        assert_eq!(
            keys(&constrained),
            [
                ("page".to_owned(), vec![Some("Integer")]),
                ("to".to_owned(), vec![Some("String")]),
            ]
        );
        // Outside a routes file, only a route set's blocks are routes.
        let plugin = "\
register_asset \"x.css\", type: :admin, **options
Shop::Application.routes.append do
  get \"/x\" => \"x#y\", defaults: { format: :json }
end
Engine.routes.draw do
  get \"/y\", kind: :z
end
other.draw do
  get \"/z\", page: :p
end
";
        let touched = read_route_values(plugin, false);
        assert!(!touched.everything);
        assert_eq!(
            keys(&touched),
            [
                ("/x".to_owned(), vec![Some("String")]),
                (
                    "defaults".to_owned(),
                    vec![Some("ActionController::Parameters")]
                ),
                ("format".to_owned(), vec![Some("Symbol")]),
                ("kind".to_owned(), vec![Some("Symbol")]),
            ]
        );
        assert!(read_route_values(plugin, true).everything);
        assert!(!read_route_values("defaults(options)\n", false).everything);
    }

    #[test]
    fn only_a_parser_of_the_project_s_own_or_parse_json_times_counts() {
        for source in [
            "ActionDispatch::Request.parameter_parsers = { json: ->(raw) { raw } }\n",
            "ActionDispatch::Request.parameter_parsers[:xml] = ->(raw) { raw }\n",
            "ActionDispatch::Request.parameter_parsers.merge!(xml: parser)\n",
            "ActionDispatch::Request.parameter_parsers[:xml] ||= parser\n",
            "ActiveSupport.parse_json_times = true\n",
            "config.active_support.parse_json_times = flag\n",
        ] {
            assert!(read_request_settings(source), "{source}");
        }
        for source in [
            "self.parameter_parsers = DEFAULT_PARSERS\n",
            "mattr_accessor :parse_json_times\nif ActiveSupport.parse_json_times\nend\n",
            "ActiveSupport.parse_json_times = false\n",
            "ActiveSupport.parse_json_times = nil\n",
            "parsers = ActionDispatch::Request.parameter_parsers\nother[:a] = 1\ncache[:a] ||= 1\n",
        ] {
            assert!(!read_request_settings(source), "{source}");
        }
        // A parser that only decodes JSON hands back what Rails' own does.
        for source in [
            "\
module Jsonapi
  def self.install
    ActionDispatch::Request.parameter_parsers[:jsonapi] = parser
  end

  def self.parser
    lambda do |body|
      data = JSON.parse(body)
      data = { _json: data } unless data.is_a?(Hash)
      data.with_indifferent_access
    end
  end
end
",
            "ActionDispatch::Request.parameter_parsers[:json] = ->(raw) { ::JSON.parse(raw) }\n",
            "ActionDispatch::Request.parameter_parsers.merge!(a: proc { |raw| ActiveSupport::JSON.decode(raw) })\n",
            "ActionDispatch::Request.parameter_parsers = { json: lambda { |raw| JSON.parse(raw).deep_symbolize_keys } }\n",
        ] {
            assert!(!read_request_settings(source), "{source}");
        }
        for source in [
            "ActionDispatch::Request.parameter_parsers[:json] = ->(raw) { JSON.parse(raw, create_additions: true) }\n",
            "ActionDispatch::Request.parameter_parsers[:json] = ->(raw) { Hash.from_xml(raw) }\n",
            "ActionDispatch::Request.parameter_parsers[:json] = ->(raw) { }\n",
            "ActionDispatch::Request.parameter_parsers[:json] = parser\ndef self.parser(x) = 1\n",
            "ActionDispatch::Request.parameter_parsers[:json] = other.parser\n",
            "ActionDispatch::Request.parameter_parsers[:json] = proc\n",
            "ActionDispatch::Request.parameter_parsers[:a] = outer\ndef outer\n  inner\nend\ndef inner\n  ->(raw) { JSON.parse(raw) }\nend\n",
            "ActionDispatch::Request.parameter_parsers = PARSERS\n",
            "ActionDispatch::Request.parameter_parsers = { **others }\n",
            "ActionDispatch::Request.parameter_parsers.store(:a)\n",
            "ActionDispatch::Request.parameter_parsers.update(xml: parser)\n",
            "ActionDispatch::Request.parameter_parsers.merge!(others)\n",
            "self.parameter_parsers = \n",
        ] {
            assert!(read_request_settings(source), "{source}");
        }
    }
    #[test]
    fn the_rest_of_what_a_write_or_a_proof_can_be() {
        // Every key refused, and a proof read beside a write.
        let everything = Touched {
            everything: true,
            ..Touched::default()
        };
        assert_eq!(asked("[]", &[Some("id")], &everything), None);
        let keys = [Some("id".to_owned())];
        let proven = |method: &'static str, proven: &'static [&'static str], touched: &Touched| {
            request_value(
                &Asked {
                    method,
                    keys: &keys,
                    block: false,
                    proven: Some(proven),
                },
                touched,
            )
        };
        let untouched = Touched::default();
        assert_eq!(
            proven("[]", &["String", "nil"], &untouched).as_deref(),
            Some("String | nil")
        );
        assert_eq!(
            proven("require", &["String", "nil"], &untouched).as_deref(),
            Some("String")
        );
        assert_eq!(
            proven("expect", &["String"], &untouched).as_deref(),
            Some("String")
        );
        // A value off the params written there holds the union, which no proof covers.
        let copied = flat("params[:id] = params[:other]\n");
        assert_eq!(proven("[]", &["String"], &copied), None);
        let nil = flat("params[:id] = nil\nparams[:x] = false\n");
        assert_eq!(
            proven("[]", &["String"], &nil).as_deref(),
            Some("String | nil")
        );
        assert_eq!(keys_of(&nil, "x"), vec![Some("bool")]);
    }

    fn keys_of(touched: &Touched, key: &str) -> Vec<Option<&'static str>> {
        touched
            .keys
            .get(key)
            .into_iter()
            .flatten()
            .copied()
            .collect()
    }

    #[test]
    fn what_is_not_the_params_writes_nothing() {
        for source in [
            "@other = 1\n@held ||= 1\n@held[:a] = Time.now\n",
            "params(1)[:a] = Time.now\nparams { }[:a] = Time.now\n",
            "request(1).params[:a] = Time.now\ncontroller(1).params[:a] = Time.now\n",
            "x.request.params[:a] = Time.now\n@other.params[:a] = Time.now\n",
            "other[:a] ||= Time.now\nother[:a] &&= Time.now\nother[:a] += 1\n",
            "a, other[:b] = 1, 2\n",
            "copy = params.permit(:a)\ncopy[:x] = Time.now\n",
            "record.params = {}\nrecord.params ||= {}\n",
            "def x((a, b), c)\n  a[:x] = 1\nend\n",
            "y ||= 1\ny[:a] = Time.now\n",
            "params.tap { |all| }\n",
        ] {
            let touched = flat(source);
            assert!(
                !touched.everything && touched.keys.is_empty() && touched.mutators.is_empty(),
                "{source}: {touched:?}"
            );
        }
        let unless_else = flat("params[:ue] = unless flag then \"a\" else :b end\n");
        assert_eq!(
            keys_of(&unless_else, "ue"),
            vec![Some("String"), Some("Symbol")]
        );
        assert!(flat("params.each(1) { |key, value| value[:a] = 1 }\n").nested);
        let held = flat("x ||= params\nx[:a] = Time.now\n");
        assert_eq!(keys(&held), [("a".to_owned(), vec![None])]);
        let destructured = flat("def x((a, b), c)\n  c[:x] = 1\nend\n");
        assert_eq!(destructured.mutators, BTreeSet::from(["x".to_owned()]));
    }

    #[test]
    fn the_rest_of_what_a_route_or_a_parser_can_write() {
        let source = "\
key = \"k\"
get \"x\", to: \"a#b\", key => 1, id: /\\d+/, slug: /#{x}/
foo.constraints(id: ID) do
  get \"y\", to: \"y#y\", page: 2
end
constraints(id: OTHER)
foo.defaults(options)
";
        let touched = read_route_values(source, true);
        assert!(!touched.everything);
        assert_eq!(
            keys(&touched),
            [
                ("id".to_owned(), vec![None]),
                ("page".to_owned(), vec![Some("Integer")]),
                ("to".to_owned(), vec![Some("String")]),
            ]
        );
        for source in [
            "other.merge!(x)\nother.update(y)\ncache.store(:a, 1)\n",
            "ActionDispatch::Request.parameter_parsers[:a] = Kernel.lambda { |raw| JSON.parse(raw) }\n",
        ] {
            assert_eq!(
                read_request_settings(source),
                source.contains("Kernel"),
                "{source}"
            );
        }
        for source in [
            "ActionDispatch::Request.parameter_parsers[:a] = PARSER\n",
            "ActionDispatch::Request.parameter_parsers[:a] = parser(1)\ndef parser = ->(r) { JSON.parse(r) }\n",
            "ActionDispatch::Request.parameter_parsers[:a] = parser { }\ndef parser = ->(r) { JSON.parse(r) }\n",
        ] {
            assert!(read_request_settings(source), "{source}");
        }
        assert!(!read_request_settings(
            "ActionDispatch::Request.parameter_parsers[:a] = self.parser\ndef parser = ->(r) { JSON.parse(r) }\n"
        ));
    }
}
