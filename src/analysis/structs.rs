//! `Struct.new(:x, :y)` and `Data.define(:x, :y)` — the one generator that is not Rails.
//!
//! Both are plain Ruby and entirely static: read a literal list of names out of a call, say
//! which members it installs. They are here rather than in
//! [`workspace::rails`](crate::workspace::rails) because that directory is the crate's only
//! Rails knowledge and a `Struct` is not Rails'.
//!
//! # Which calls name a class
//!
//! `Struct.new` returns an anonymous class, so the name comes from what the call is assigned to.
//! Two shapes give it one: `Point = Struct.new(:x, :y)` and
//! `class Line < Struct.new(:start, :end)`. The subclass form is rare only because RuboCop's
//! `Style/StructInheritance` flags it by default — suppressed, not absent — and it names its
//! class as plainly as the constant does. A call held by a local, an instance variable or a
//! `let` block names no class and declares nothing.
//!
//! Two shapes are declined although a class is there:
//!
//! - **A namespace no file writes.** `class Reports::Reg::Metric` would introduce `Reports::Reg`
//!   itself, at the cost of that namespace's own singleton members. Narrowed rather than
//!   removed: where the owner's immediate parent is a `module` the application writes down,
//!   [`Declarations::open`](crate::generated::Declarations) opens it as a body and the call
//!   declares. This is the one thing the reader asks the graph;
//!   [`Namespaces`](crate::generated::Namespaces) owns the test, because the schema reader
//!   reaches the same shape.
//! - **`Foo::Point = Struct.new(:x)`.** A path on the left of an assignment is *resolved*, not
//!   nested, so inside `module A` it may be `A::Foo::Point` or the top-level `Foo::Point`. That
//!   is a constant lookup, and this pass runs before anything is resolved.
//!
//! # Which names count
//!
//! Every positional argument must be a symbol literal; a `keyword_init:` hash is an option and
//! is skipped. **Anything else in positional position declines the whole call** —
//! `Struct.new(*NAMES)` names members this reader cannot see, and a class missing methods is
//! worse than one with none. `Struct.new("Name", :x)` falls under that rule and genuinely puts
//! its members elsewhere, on `Struct::Name`.
//!
//! A symbol that is not a legal method name is declined **on its own**, not with the call:
//! [`Synthesized::record`](super::synthesized::Synthesized::record) refuses a whole generated
//! document over a name this crate cannot spell, and one unspellable member is not worth the
//! rest of the file. `rails::enums` declines a label the same way.
//!
//! # A `def` in the block wins
//!
//! A member this would install whose name the block also `def`s is **not declared** — the `def`
//! is the one with a place to jump to. Those `def`s are not declared as members either: rubydex
//! has already indexed them (as `private Object#…`, since a `def` in a block belongs to no class
//! it can name), so they answer on the name rung, and a declaration here would be text this
//! crate wrote pointing at a `def` rubydex owns.
//!
//! # What each installs
//!
//! `Struct` is mutable and `Data` is not, which is the whole difference:
//!
//! | | per name | fixed |
//! | --- | --- | --- |
//! | `Struct.new` | `x`, `x=` | `[]`, `each`, `members`, `self.members` |
//! | `Data.define` | `x` | `with`, `to_h`, `deconstruct_keys` |
//!
//! Fixed members are [`Source::Interface`], per-name ones [`Source::Struct`] — directly below a
//! hand-written annotation, because a `sig` above a `def` overriding a reader is the one
//! collision either can reach.
//!
//! `Struct#each` is declared `untyped` although Ruby returns `self`: [`Facts`] holds one
//! declaration per `(owner, name)`, and `each` returns the struct with a block and an
//! `Enumerator` without one. Nothing is the only honest single answer;
//! [`Types::harvest`](super::types::Types::harvest) drops it, leaving the name rung. `Data#with`
//! has no such split and chains.

use ruby_prism::{CallNode, ConstantWriteNode, Node};

use crate::generated::{Declared, Facts, Namespaces, Owner, Source};

/// One member name and the two spans a jump to it needs: the whole `:x`, and the `x` inside it.
type Named = (String, (u32, u32), (u32, u32));

/// Which of the two macros was written, and therefore what it installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `Struct.new`: a reader and a writer per name.
    Struct,
    /// `Data.define`: a reader per name, and no writer at all.
    Data,
}

impl Shape {
    /// The call this shape is written as: `Struct.new` and `Data.define`.
    ///
    /// Read off the receiver and the message together, because neither half is evidence on its
    /// own — `Struct.build` is somebody's own method and `Foo.define` is not a `Data`.
    fn of(node: &CallNode<'_>) -> Option<Self> {
        let receiver = node.receiver()?;
        let constant = receiver.as_constant_read_node()?;
        match (constant.name().as_slice(), node.name().as_slice()) {
            (b"Struct", b"new") => Some(Self::Struct),
            (b"Data", b"define") => Some(Self::Data),
            _ => None,
        }
    }

    /// How the call is spelled, for the provenance line a hover card shows.
    fn spelled(self) -> &'static str {
        match self {
            Self::Struct => "Struct.new",
            Self::Data => "Data.define",
        }
    }

    /// The class the fixed members really come from: `Struct` and `Data` themselves.
    fn owned(self) -> &'static str {
        match self {
            Self::Struct => "Struct",
            Self::Data => "Data",
        }
    }

    /// The members every class of this shape has, whatever names it was given.
    ///
    /// `%s` in a return type is the owning class's own name, which is the only thing either
    /// table needs from outside itself. The `bool` is the singleton side, and it is `true`
    /// exactly once: `members` is the one fixed member Ruby defines on both sides. Declaring
    /// only the instance half leaves `Point.members` on the name rung with a candidate list
    /// three entries *longer* than before this reader ran, **which is how a generator makes an
    /// answer worse without making one wrong**.
    fn framework(self) -> &'static [(&'static str, &'static str, &'static str, bool)] {
        match self {
            // `[]` takes a member's name or its index and can return any of them; `each` is the
            // two-armed case argued in this module's header; `members` is the one that chains.
            Self::Struct => &[
                ("[]", "(untyped)", "untyped", false),
                ("each", "() ?{ (untyped) -> void }", "untyped", false),
                ("members", "()", "Array[Symbol]", false),
                ("members", "()", "Array[Symbol]", true),
            ],
            // `with` hands back the same class, which is what makes a `Data` chain through a
            // copy. Both hashes are keyed by the member names, so `Symbol` is exact and the
            // value side is the same `untyped` the readers have.
            Self::Data => &[
                ("with", "(**untyped)", "%s", false),
                ("to_h", "()", "Hash[Symbol, untyped]", false),
                (
                    "deconstruct_keys",
                    "(Array[Symbol]?)",
                    "Hash[Symbol, untyped]",
                    false,
                ),
            ],
        }
    }
}

/// Read every `Struct.new` and `Data.define` in `source` that names a class.
///
/// `file` is how the source should be spelled to a reader and goes into every provenance line.
/// `namespaces` is what may be spelled around a name — the one thing this reader needs that the
/// text cannot tell it, and the reason is [`Reader::spellable`]. Text in otherwise — no I/O, the
/// same contract every generator has.
#[must_use]
pub fn read(source: &str, file: &str, namespaces: &Namespaces) -> Facts {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut reader = Reader {
        source,
        file,
        namespaces,
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
    namespaces: &'src Namespaces,
    nesting: Vec<String>,
    out: Facts,
}

impl Reader<'_> {
    /// One body, and then the class and module bodies written as statements of it.
    ///
    /// Statements and not a walk of the whole tree, which is [`super::annotations`]' bound and is
    /// taken here for both of its reasons: the depth stays the depth of `module A; class B`
    /// rather than the depth of every method body in the file, and it says what every reader
    /// here says — a constant assigned inside a `describe` block is not a statement of the
    /// enclosing body. Three assignments in six corpora are that shape and all three are in
    /// specs.
    fn walk(&mut self, body: Option<Node<'_>>) {
        let Some(statements) = body.and_then(|body| body.as_statements_node()) else {
            return;
        };
        for statement in statements.body().iter() {
            if let Some(assignment) = statement.as_constant_write_node() {
                self.assigned(&assignment);
                continue;
            }
            let (path, inner) = if let Some(class) = statement.as_class_node() {
                // The superclass is read *before* the nesting is pushed, because
                // `class Line < Struct.new(:x)` declares members on `Line` and the call is
                // written outside its body.
                let name = spelling(self.source, &class.constant_path());
                if let Some(node) = class.superclass()
                    && let Some(call) = node.as_call_node()
                {
                    self.declared(&call, &self.owner(&name));
                }
                (name, class.body())
            } else if let Some(module) = statement.as_module_node() {
                (
                    spelling(self.source, &module.constant_path()),
                    module.body(),
                )
            } else {
                continue;
            };
            self.nesting.push(path);
            self.walk(inner);
            self.nesting.pop();
        }
    }

    /// A constant assigned the result of a call: `Point = Struct.new(:x, :y)`.
    fn assigned(&mut self, node: &ConstantWriteNode<'_>) {
        let Some(call) = node.value().as_call_node() else {
            return;
        };
        let name = String::from_utf8_lossy(node.name().as_slice()).into_owned();
        let owner = self.owner(&name);
        self.declared(&call, &owner);
    }

    /// The class a name written in this body belongs to, fully spelled.
    fn owner(&self, name: &str) -> Owner {
        let mut path = self.nesting.clone();
        path.push(name.to_owned());
        Owner::Instance(path.join("::"))
    }

    /// Whether this owner's name can be written without introducing a namespace.
    ///
    /// [`Namespaces::spellable`] is the rule, and it is there rather than here because the
    /// schema generator declares on a nested model too — two generators ask the question and
    /// neither may answer it differently.
    fn spellable(&self, owner: &Owner) -> bool {
        self.namespaces.spellable(owner.name())
    }

    /// Everything one `Struct.new` or `Data.define` installs on `owner`, or nothing.
    fn declared(&mut self, node: &CallNode<'_>, owner: &Owner) {
        if !self.spellable(owner) {
            return;
        }
        let Some(shape) = Shape::of(node) else {
            return;
        };
        let Some(names) = members(self.source, node) else {
            return;
        };
        let shadowed = block_methods(node);
        let call = format!(
            "`{}({})`",
            shape.spelled(),
            names
                .iter()
                .map(|(name, _, _)| format!(":{name}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut say = |owner: Owner, name: String, parameters: &str, returns: &str, at, from| {
            // Instance side only: `block_methods` collects receiverless `def`s, which are
            // instance methods, and a `def members` in the block says nothing about
            // `Point.members`.
            if matches!(owner, Owner::Instance(_)) && shadowed.contains(&name) {
                return;
            }
            self.out.declare(Declared {
                owner,
                name,
                returns: returns.to_owned(),
                parameters: parameters.to_owned(),
                because: match from {
                    // The file really does declare a member per name it wrote.
                    Source::Struct => format!("From `{}`, {call}.", self.file),
                    // And it really does not declare the fixed half: the card carries no place,
                    // and a line claiming the file
                    // said so would be the only thing on it suggesting there is one.
                    _ => format!(
                        "Every `{}` has this; ya-lsp writes it, and no file declares it.",
                        shape.owned()
                    ),
                },
                at,
                from,
                overloads: Vec::new(),
            });
        };
        for (name, declared, selection) in &names {
            let at = Some((*declared, *selection));
            say(
                owner.clone(),
                name.clone(),
                "()",
                "untyped",
                at,
                Source::Struct,
            );
            if shape == Shape::Struct {
                say(
                    owner.clone(),
                    format!("{name}="),
                    "(untyped)",
                    "untyped",
                    at,
                    Source::Struct,
                );
            }
        }
        for (name, parameters, returns, singleton) in shape.framework() {
            let returns = returns.replace("%s", owner.name());
            let side = if *singleton {
                Owner::Singleton(owner.name().to_owned())
            } else {
                owner.clone()
            };
            say(
                side,
                (*name).to_owned(),
                parameters,
                &returns,
                None,
                Source::Interface,
            );
        }
    }
}

/// The member names one call was given, and where each of them is written.
///
/// `None` for a call this reader may not believe: no positional arguments at all, or one that is
/// not a symbol literal. The `(whole symbol, the name inside it)` pair is what an editor shows
/// and what it selects, which is `rails::enums`' shape for a label — `point.x` should land on
/// the `:x` and highlight the `x`.
fn members(source: &str, node: &CallNode<'_>) -> Option<Vec<Named>> {
    let arguments = node.arguments()?;
    let mut names = Vec::new();
    for argument in arguments.arguments().iter() {
        // `keyword_init: true` is an option and not a member. It changes how the constructor is
        // called and nothing this reader declares, which is why it is skipped rather than
        // refused — 81 of the corpus' 268 calls write one.
        if argument.as_keyword_hash_node().is_some() {
            continue;
        }
        let symbol = argument.as_symbol_node()?;
        let whole = symbol.location();
        let value = symbol.value_loc()?;
        let name = source.get(value.start_offset()..value.end_offset())?;
        if !is_method_name(name) {
            continue;
        }
        names.push((
            name.to_owned(),
            (whole.start_offset() as u32, whole.end_offset() as u32),
            (value.start_offset() as u32, value.end_offset() as u32),
        ));
    }
    (!names.is_empty()).then_some(names)
}

/// Whether a symbol can be written as a `def` name in RBS.
///
/// A struct member always can in practice — `Struct.new` raises on anything else — so this is
/// the belt for a source that never runs: an unparseable name would take
/// [`Synthesized::record`](super::synthesized::Synthesized::record)'s gate down on the whole
/// document, which costs every other member in the file rather than the one.
fn is_method_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// The names of the methods a call's block `def`s, if it has one.
///
/// Statements of the block body only, for the same depth reason [`Reader::walk`] gives, and
/// receiverless only: a `def self.build` inside the block is on the struct's singleton and
/// shadows nothing this reader declares on the instance side.
fn block_methods(node: &CallNode<'_>) -> Vec<String> {
    let Some(body) = node
        .block()
        .and_then(|block| block.as_block_node())
        .and_then(|block| block.body())
        .and_then(|body| body.as_statements_node())
    else {
        return Vec::new();
    };
    body.body()
        .iter()
        .filter_map(|statement| statement.as_def_node())
        .filter(|definition| definition.receiver().is_none())
        .map(|definition| String::from_utf8_lossy(definition.name().as_slice()).into_owned())
        .collect()
}

/// The constant a `class` or `module` keyword names, exactly as it is written.
///
/// Sliced rather than walked, so `class Admin::Setting` nests one name and not two and joining
/// the stack with `::` reproduces what rubydex calls the same class. A leading `::` is dropped.
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
    use super::read;
    use crate::generated::{Namespaces, declaring, declaring_kinds};

    /// Every namespace the fixtures below nest into, so that [`Reader::spellable`] is not
    /// what each of them is testing. The one test that *is* about it names its own sets.
    fn known() -> Namespaces {
        declaring_kinds(&["Vite", "Vite::Manifest", "Reports", "Reports::Reg"], &[])
    }

    fn rbs(source: &str) -> String {
        read(source, "app/models/point.rb", &known())
            .render(&declaring(&[]))
            .rbs
    }

    /// The whole of what one `Struct.new` declares, pinned as a document.
    ///
    /// Every other test here reads one line out of this; this one is the shape — which bodies
    /// open, which side each `def` is on, and which of the two sentences each member carries.
    #[test]
    fn the_rbs_a_struct_declares() {
        assert_eq!(
            rbs("Point = Struct.new(:x, :y)\n"),
            "\
class Point
  # From `app/models/point.rb`, `Struct.new(:x, :y)`.
  def x: () -> untyped
  # From `app/models/point.rb`, `Struct.new(:x, :y)`.
  def x=: (untyped) -> untyped
  # From `app/models/point.rb`, `Struct.new(:x, :y)`.
  def y: () -> untyped
  # From `app/models/point.rb`, `Struct.new(:x, :y)`.
  def y=: (untyped) -> untyped
  # Every `Struct` has this; ya-lsp writes it, and no file declares it.
  def []: (untyped) -> untyped
  # Every `Struct` has this; ya-lsp writes it, and no file declares it.
  def each: () ?{ (untyped) -> void } -> untyped
  # Every `Struct` has this; ya-lsp writes it, and no file declares it.
  def members: () -> Array[Symbol]
  # Every `Struct` has this; ya-lsp writes it, and no file declares it.
  def self.members: () -> Array[Symbol]
end
"
        );
    }

    /// The other half of the same pin: no writers, and `with` hands the class back.
    ///
    /// `-> Coord` is what makes `coord.with(lat: 1).lng` answer, and it is the one return type in
    /// this module that is not fixed text.
    #[test]
    fn the_rbs_a_data_declares() {
        assert_eq!(
            rbs("Coord = Data.define(:lat)\n"),
            "\
class Coord
  # From `app/models/point.rb`, `Data.define(:lat)`.
  def lat: () -> untyped
  # Every `Data` has this; ya-lsp writes it, and no file declares it.
  def with: (**untyped) -> Coord
  # Every `Data` has this; ya-lsp writes it, and no file declares it.
  def to_h: () -> Hash[Symbol, untyped]
  # Every `Data` has this; ya-lsp writes it, and no file declares it.
  def deconstruct_keys: (Array[Symbol]?) -> Hash[Symbol, untyped]
end
"
        );
    }

    /// The subclass spelling, which declares on the class it names.
    ///
    /// The call is written outside the body it declares into, which is why the superclass is
    /// read before the nesting is pushed rather than by the walk of the body.
    #[test]
    fn a_class_that_inherits_a_struct_declares_its_members() {
        let declared = rbs("class Line < Struct.new(:start_line)\n  def span; end\nend\n");
        assert!(declared.starts_with("class Line\n"), "{declared}");
        assert!(
            declared.contains("def start_line: () -> untyped"),
            "{declared}"
        );
        // The `def` in the body is rubydex's and this reader never touches it.
        assert!(!declared.contains("span"), "{declared}");
    }

    /// The nesting is the class's, whichever of the two spellings named it.
    #[test]
    fn a_nested_constant_is_declared_with_its_nesting() {
        for source in [
            "module Vite\n  class Manifest\n    Entry = Struct.new(:name)\n  end\nend\n",
            "module Vite\n  class Manifest\n    class Entry < Struct.new(:name)\n    end\n  end\nend\n",
            "module Vite::Manifest\n  Entry = Struct.new(:name)\nend\n",
        ] {
            assert!(
                rbs(source).starts_with("class Vite::Manifest::Entry\n"),
                "{source}"
            );
        }
        assert!(
            rbs("module ::Vite\n  Entry = Struct.new(:name)\nend\n")
                .starts_with("class Vite::Entry\n")
        );
    }

    /// A namespace nobody defines has two safe spellings and no third, which is the whole rule.
    ///
    /// `module Reports::Missing` is a whole application's spelling for a `Reports` that Zeitwerk
    /// conjures and no file writes, and a joined `class Reports::Missing::Metric` introduces
    /// `Reports::Missing` itself — as a **class** — which costs it its own singleton members.
    /// There is a safe spelling when the parent is a `module` the application writes down, and
    /// none at all when it is not: an explicit `class Reports` wrapper declares a kind this
    /// crate cannot know, measured at 234 chatwoot positions.
    #[test]
    fn a_namespace_nobody_defines_declares_only_where_there_is_a_safe_spelling() {
        let source = "module Reports::Missing\n  Metric = Data.define(:name)\nend\n";
        // The parent is a `module` a file writes down, so it is opened as a body of its own
        // and the members are declared.
        let module = declaring(&["Reports::Missing"]);
        assert!(
            read(source, "f.rb", &module)
                .render(&module)
                .rbs
                .starts_with("module Reports::Missing\nclass Metric\n"),
            "{}",
            read(source, "f.rb", &module).render(&module).rbs
        );
        // The same file with the parent a **class** instead: nothing declares `Reports`, the
        // joined name would introduce the parent, and no wrapper can be written for it — so
        // the call declares nothing at all.
        let class = declaring_kinds(&["Reports::Missing"], &[]);
        assert!(read(source, "f.rb", &class).render(&class).rbs.is_empty());
        // And once something writes `Reports` down, every segment is declared, the joined name
        // introduces nothing, and it is spelled exactly as it is written.
        // The something may be a **gem**, which is what the graph projection widens.
        let defined = declaring_kinds(&["Reports", "Reports::Missing"], &[]);
        assert!(
            read(source, "f.rb", &defined)
                .render(&defined)
                .rbs
                .starts_with("class Reports::Missing::Metric\n")
        );
        // A top-level constant has no namespace to ask about, which is why the commonest
        // spelling never reaches this rule at all.
        assert!(
            read("Point = Struct.new(:x)\n", "f.rb", &declaring(&[]))
                .render(&declaring(&[]))
                .rbs
                .starts_with("class Point\n")
        );
    }

    /// A call whose members this reader cannot see declares nothing at all.
    ///
    /// The splat has two real occurrences; the string is `Struct.new("Name", :x)`,
    /// which defines `Struct::Name` and so really does put the members somewhere else; and a
    /// call with no arguments has nothing to say.
    #[test]
    fn a_call_this_cannot_read_the_names_out_of_declares_nothing() {
        for source in [
            "Point = Struct.new(*NAMES)\n",
            "Point = Struct.new(\"Name\", :x)\n",
            "Point = Struct.new(NAMES)\n",
            "Point = Struct.new\n",
            "Point = Struct.new()\n",
            "Point = Data.define()\n",
            "Point = Struct.new(keyword_init: true)\n",
        ] {
            assert_eq!(rbs(source), "", "{source}");
        }
    }

    /// `keyword_init:` changes the constructor, which this declares nothing about.
    #[test]
    fn a_keyword_option_is_not_a_member() {
        let declared = rbs("Point = Struct.new(:x, keyword_init: true)\n");
        assert!(declared.contains("def x: () -> untyped"), "{declared}");
        assert!(!declared.contains("keyword_init"), "{declared}");
    }

    /// A name that is not a legal method name is declined on its own.
    ///
    /// One member this crate cannot spell would make `Synthesized::record` refuse the whole
    /// document, which costs every other member in the file rather than the one.
    #[test]
    fn a_name_that_is_not_a_method_name_is_declined_by_itself() {
        let declared = rbs("Point = Struct.new(:x, :\"a b\", :Y, :\"9\", :_z)\n");
        assert!(declared.contains("def x: () -> untyped"), "{declared}");
        assert!(declared.contains("def _z: () -> untyped"), "{declared}");
        for refused in ["a b", "def Y", "def 9"] {
            assert!(!declared.contains(refused), "{refused}: {declared}");
        }
        // And a call whose every name is refused says nothing rather than opening an empty body.
        assert_eq!(rbs("Point = Struct.new(:\"a b\")\n"), "");
    }

    /// A `def` in the block keeps the member this would otherwise install.
    ///
    /// The `def` is the one with a place to jump to, and it is the one that runs.
    #[test]
    fn a_def_in_the_block_shadows_the_member_it_replaces() {
        let declared =
            rbs("Point = Struct.new(:x) do\n  def x; 1; end\n  def to_h; {}; end\nend\n");
        assert!(!declared.contains("def x:"), "{declared}");
        assert!(declared.contains("def x=:"), "{declared}");
        // `to_h` is not one a `Struct` declares here at all, so nothing changes for it.
        assert!(!declared.contains("to_h"), "{declared}");
        let data = rbs("Coord = Data.define(:lat) do\n  def to_h; {}; end\nend\n");
        assert!(!data.contains("def to_h:"), "{data}");
        assert!(data.contains("def lat:"), "{data}");
    }

    /// The block's `def self.` is on the singleton and shadows nothing on the instance side.
    ///
    /// `Struct#members` and `Struct.members` are two methods, so a block that writes one of them
    /// must not take the other away.
    #[test]
    fn a_singleton_def_in_the_block_shadows_only_its_own_side() {
        let declared = rbs("Point = Struct.new(:x) do\n  def self.members; []; end\nend\n");
        assert!(
            declared.contains("def members: () -> Array[Symbol]"),
            "{declared}"
        );
        let instance = rbs("Point = Struct.new(:x) do\n  def members; []; end\nend\n");
        assert!(!instance.contains("\n  def members:"), "{instance}");
        assert!(
            instance.contains("def self.members: () -> Array[Symbol]"),
            "{instance}"
        );
    }

    /// A block that is not a `do … end` full of statements is read as one with no `def`s.
    #[test]
    fn a_block_with_no_body_shadows_nothing() {
        for source in [
            "Point = Struct.new(:x) do\nend\n",
            "Point = Struct.new(:x, &block)\n",
        ] {
            assert!(rbs(source).contains("def x: () -> untyped"), "{source}");
        }
    }

    /// Neither name on its own is evidence, and neither is a call that names no class.
    ///
    /// The last three are 94 of the corpus' 268: a local, an instance variable and a bare
    /// expression each name a class whose name dies with the method.
    #[test]
    fn only_the_two_calls_assigned_to_a_class_declare_anything() {
        for source in [
            "Point = Struct.build(:x)\n",
            "Point = Data.new(:x)\n",
            "Point = Widget.define(:x)\n",
            "Point = Struct::Value.new(:x)\n",
            "Point = NAMES\n",
            "Foo::Point = Struct.new(:x)\n",
            "point = Struct.new(:x)\n",
            "@point = Struct.new(:x)\n",
            "Struct.new(:state).new({})\n",
            "class Line < Widget\n  def span; end\nend\n",
            "class Line < NAMES\nend\n",
        ] {
            assert_eq!(rbs(source), "", "{source}");
        }
    }

    /// A constant assigned inside a block is not a statement of the enclosing body.
    ///
    /// The bounding rule every reader here applies, and the three occurrences in six corpora
    /// are all in specs.
    #[test]
    fn a_constant_written_inside_a_block_declares_nothing() {
        assert_eq!(
            rbs("describe Thing do\n  Fake = Struct.new(:id)\nend\n"),
            ""
        );
    }

    /// A `class` with no body still gets walked past rather than stopping the file.
    #[test]
    fn an_empty_body_is_walked_past() {
        let declared = rbs("class Empty\nend\nmodule Also\nend\nPoint = Struct.new(:x)\n");
        assert!(declared.contains("def x: () -> untyped"), "{declared}");
    }

    /// Where the jump lands: the whole `:x`, selecting the `x` inside it.
    ///
    /// The reader and the writer point at the same symbol, which is `enum`'s answer for the four
    /// members one label installs.
    #[test]
    fn a_member_points_at_the_symbol_that_named_it() {
        let source = "Point = Struct.new(:x)\n";
        let spans = read(source, "app/models/point.rb", &known())
            .render(&declaring(&[]))
            .spans;
        assert_eq!(spans.len(), 2);
        for span in spans {
            assert_eq!(
                &source[span.declared.0 as usize..span.declared.1 as usize],
                ":x"
            );
            assert_eq!(
                &source[span.selection.0 as usize..span.selection.1 as usize],
                "x"
            );
        }
        // The fixed members carry no span at all, which is what keeps them off every jump.
        assert_eq!(
            read("Coord = Data.define(:lat)\n", "f.rb", &known())
                .render(&declaring(&[]))
                .spans
                .len(),
            1
        );
    }

    /// A file that mentions neither macro says nothing, which is what most of the list does.
    #[test]
    fn a_file_with_no_call_declares_nothing() {
        assert_eq!(rbs("class Story\n  def title; end\nend\n"), "");
        assert_eq!(rbs(""), "");
        assert_eq!(rbs("Point = 1\n"), "");
    }
}
