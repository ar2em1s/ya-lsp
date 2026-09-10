//! `db/*schema.rb`, read: what tables exist, what their columns are, and what each returns.
//!
//! The first generator written and still the one that pays most.
//! Text in, [`Facts`] out, no graph and no I/O — which class reads which table is the caller's
//! half of the bargain, because this module knows what tables exist and knows nothing at all
//! about which of a project's classes is an ActiveRecord model.
//!
//! Every table is looked up **from** a class that exists, and the two escapes from the naming
//! convention are here too: [`read_table_names`] reads the `self.table_name =` a legacy or
//! namespaced model writes, and a table two schemas both declare is declared by neither.

use std::collections::{BTreeMap, BTreeSet};

use ruby_prism::{CallNode, DefNode, Node};

use super::inflect::underscore;
use super::syntax::{
    candidates, constant_spelling, first_string, first_symbol_or_string, header, keyword,
    string_literal, symbol_or_string,
};
use super::{COLUMN_TYPES, NOT_COLUMNS, PRIMARY_KEY};
use crate::generated::{Declared, Facts, Owner, Source};

/// One `db/schema.rb`, read.
///
/// Parsed once and kept, because a project has more than one and the tables of all of them have
/// to be known before any of them generates a line — see [`Schema::signatures`]. Owns
/// everything it read, so a caller may hold several while it decides.
#[derive(Debug)]
pub struct Schema {
    tables: Vec<Table>,
}

impl Schema {
    /// The same schema, read out of something that is not Ruby.
    ///
    /// The seam between the two readers: [`structure`](super::structure) scans a `db/structure.sql` and
    /// ends **here**, so `signatures`, [`rbs_type`], the provenance line, the `retyped`
    /// withdrawal and every consumer in `analysis::synthesize` are shared rather than mirrored.
    /// There is exactly one thing a second reader may do, and it is to produce these tables —
    /// which is what makes a change to how a column is typed reach both readers or neither.
    pub(super) fn from_tables(tables: Vec<Table>) -> Self {
        Self { tables }
    }
}

/// Read every table `source` creates. Text in, no graph and no I/O.
#[must_use]
pub fn read_schema(source: &str) -> Schema {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut reader = Reader {
        source,
        tables: Vec::new(),
    };
    reader.walk(
        parsed
            .node()
            .as_program_node()
            .map(|program| program.statements().as_node()),
    );
    Schema {
        tables: reader.tables,
    }
}

impl Schema {
    /// Every table this file creates, in the order it creates them.
    ///
    /// The caller's half of the multi-database rule: a table two schema files both declare is
    /// ambiguous, and this is how it finds out before either one has written anything.
    pub fn table_names(&self) -> impl Iterator<Item = &str> {
        self.tables.iter().map(|table| table.name.as_str())
    }

    /// The RBS this schema declares about `classes`.
    ///
    /// `classes` maps a table name to **every** class that reads it, and is the caller's half of
    /// the bargain: this module knows what tables exist and what their columns are, and knows
    /// nothing about which of a project's classes is an ActiveRecord model. A table nobody
    /// claimed generates nothing, which is why a schema in a project with no models produces an
    /// empty string.
    ///
    /// More than one class per table is deliberate and not a widening for its own sake: a
    /// throwaway `class Account < ApplicationRecord` written inside a migration reads the same
    /// `accounts` the model does, and the columns belong on both. Which classes those are, and
    /// which pair of them is an inflector collision rather than one convention applied twice, is
    /// entirely `Analysis::model_tables`' decision — this writes down what it is handed.
    ///
    /// `file` is how this schema should be spelled to a reader — `db/schema.rb`,
    /// `db/animals_schema.rb` — and it is written into the provenance comment above every
    /// declaration. It is a parameter rather than a constant precisely because there is more
    /// than one of these files: a card that says `db/schema.rb` above a column that came from
    /// `db/animals_schema.rb` is the kind of confidently wrong answer this section exists to
    /// avoid.
    #[must_use]
    pub fn signatures(
        &self,
        file: &str,
        classes: &BTreeMap<String, Vec<String>>,
        retyped: &BTreeMap<String, BTreeSet<String>>,
    ) -> Facts {
        let mut facts = Facts::default();
        for table in &self.tables {
            let Some(classes) = classes.get(&table.name) else {
                continue;
            };
            for (class, column) in classes
                .iter()
                .flat_map(|class| table.columns.iter().map(move |column| (class, column)))
            {
                // The column withdrawn, spent here because the two declarations are in two
                // different generated documents and `Facts`' precedence is per document.
                // `story.status` is the label an `enum` names and the column is the integer it
                // is stored as; an `attribute :status, :string` is Rails' documented override of
                // the same column. Answering with the storage is the wrong one of the two.
                if retyped
                    .get(class)
                    .is_some_and(|attributes| attributes.contains(&column.name))
                {
                    continue;
                }
                facts.declare(Declared {
                    owner: Owner::Instance(class.clone()),
                    name: column.name.clone(),
                    returns: rbs_type(&column.kind, column.nullable, column.array),
                    parameters: "()".to_owned(),
                    // The provenance line carries the nullability as well as the type, because
                    // a hover card shows a declaration's *name* and not its RBS return type —
                    // so `String?` would otherwise be a fact the type table holds and no one is
                    // ever shown, and the point is precisely that nothing in any Ruby tool
                    // tells you which columns can be `nil`.
                    because: format!(
                        "From `{file}`, table `{}`, column `{}` (`{}{}`, {}).",
                        table.name,
                        column.name,
                        column.kind,
                        // In the type slot rather than a field of its own, because it is what
                        // pg_dump itself writes and because the card has one line to say
                        // "many of these" in.
                        if column.array { "[]" } else { "" },
                        if column.nullable {
                            "may be `nil`"
                        } else {
                            "`null: false`"
                        }
                    ),
                    at: Some((column.at, column.name_at)),
                    from: Source::Column,
                    overloads: Vec::new(),
                });
            }
        }
        facts
    }
}

/// Everything one file says about the **name** of a table, rather than about its columns.
///
/// Three spellings and one walk, because all three are read out of the same statements-only
/// descent and a second parse of the same document to find the second of them would be a second
/// parse. What each is for is [`super::Schema`]'s caller's business: this says what the file
/// wrote down.
#[derive(Debug, Default)]
pub struct TableNames {
    /// `self.table_name = "stories"`, as the class it is written in and the table it names.
    ///
    /// The documented escape from every convention: a model whose table is not what its name
    /// implies, a namespaced model, a legacy schema. A **symbol** counts, because
    /// `table_name=` is `value&.to_s` in Rails' own source and eleven of mastodon's thirteen
    /// are written `:accounts`; an interpolation does not, because
    /// `self.table_name = "#{prefix}_stories"` is Ruby that only runs.
    ///
    /// The class is spelled with its lexical nesting, so `module Admin; class Setting` answers
    /// `Admin::Setting`; a class written `class Admin::Setting` answers the same.
    pub overrides: Vec<(String, String)>,
    /// `def self.table_name_prefix`, as the body it is written in and the string it returns.
    ///
    /// The prefix half of `compute_table_name`: `full_table_name_prefix` is
    /// `module_parents.detect { |p| p.respond_to?(:table_name_prefix) }`, so the name here is
    /// the *module* and every class under it reads a table beginning with this.
    pub prefixes: Vec<(String, String)>,
    /// `def self.table_name_suffix`, read by the same rule as [`TableNames::prefixes`].
    ///
    /// **0 occurrences in six applications**, and read anyway: it is the same syntax in the same
    /// walk, and ignoring it is the only way this reader can name a table that exists and is not
    /// the one the class reads.
    pub suffixes: Vec<(String, String)>,
    /// `isolate_namespace Spree`, as the candidate spellings of the module it names.
    ///
    /// The **commoner** of the two spellings by a factor of nearly four — 36 of the 46
    /// declarations in six corpora, and the only one solidus and every discourse plugin uses — and it is a call
    /// rather than a `def` because the engine says it about a module somebody else wrote.
    ///
    /// Candidates and **no prefix**, which is the whole reason this is a second field: what
    /// `Rails::Engine` installs is `generate_railtie_name(mod.name)`, and `mod` is the module
    /// the constant *resolved to* rather than the way it was spelled. discourse writes
    /// `isolate_namespace Provider` inside `module DiscourseChatIntegration`, whose tables begin
    /// `discourse_chat_integration_provider_` and not `provider_` — so only a caller that can
    /// settle the constant can name the prefix, and [`super::engine_prefix`] is what it then
    /// asks. Kept apart from [`TableNames::prefixes`] for Rails' own precedence as well:
    /// `unless mod.respond_to?(:table_name_prefix)` means a module that writes the method out
    /// wins.
    pub isolated: Vec<Vec<String>>,
}

/// The `table_name_prefix` `Rails::Engine#isolate_namespace` installs on `module`.
///
/// `engine_name(generate_railtie_name(mod.name))` and then an underscore, which is
/// `ActiveSupport::Inflector.underscore(name).tr("/", "_")` — so `Foo::Bar` is `foo_bar_`. The
/// argument is the module the constant **resolved to**, which is why this is a function the
/// caller asks rather than a value [`read_table_names`] could have written down.
#[must_use]
pub fn engine_prefix(module: &str) -> Option<String> {
    let segments: Vec<String> = module.split("::").filter_map(underscore).collect();
    (segments.len() == module.split("::").count()).then(|| format!("{}_", segments.join("_")))
}

/// Read what `source` says about table names. Text in, no graph and no I/O.
#[must_use]
pub fn read_table_names(source: &str) -> TableNames {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut names = Names {
        source,
        nesting: Vec::new(),
        found: TableNames::default(),
    };
    names.walk(
        parsed
            .node()
            .as_program_node()
            .map(|program| program.statements().as_node()),
    );
    names.found
}

/// One column, as the schema declares it.
#[derive(Debug)]
pub(super) struct Column {
    pub(super) name: String,
    /// The schema's own word — `string`, `bigint` — kept rather than mapped, because the
    /// provenance line quotes it and because an unmapped one still has to be named.
    ///
    /// [`structure`](super::structure) maps a dump's vocabulary **onto this word** rather than
    /// onto a Ruby class, which is what makes the two readers unable to disagree about a
    /// database: `character varying` becomes `string` and lands wherever `t.string` lands.
    pub(super) kind: String,
    pub(super) nullable: bool,
    /// Whether the column holds many of `kind` — Postgres' array type, written `array: true`
    /// by the Ruby dumper and `integer[]` by pg_dump.
    ///
    /// Read because leaving it out is a **wrong** answer rather than an absent one, which is
    /// the one kind of mistake this half of the release exists to avoid: `t.string
    /// "languages", array: true` returns an `Array[String]` and every answer derived through
    /// `String` is wrong for it. Measured over the corpora when the SQL reader asked what
    /// `integer[]` returns: **39 columns in three applications' `schema.rb` alone**.
    pub(super) array: bool,
    pub(super) at: (u32, u32),
    pub(super) name_at: (u32, u32),
}

/// One `create_table` block.
#[derive(Debug)]
pub(super) struct Table {
    pub(super) name: String,
    pub(super) columns: Vec<Column>,
}

/// The RBS type a column of `kind` returns.
///
/// One column in three carries no `null: false`, and RBS is the one output format in reach that
/// can say so. `untyped` is deliberately never optional: it already
/// includes `nil`, and `untyped?` is not RBS.
/// An array wraps rather than replaces, and it wraps an unmapped element too: `Array[untyped]`
/// is RBS and says the one true thing — many of something — where `untyped` alone says nothing
/// and `String` would say something false. It is also the only way this function returns an
/// optional over an unknown, which is correct: it is the *array* that may be `nil`, not its
/// elements.
pub(super) fn rbs_type(kind: &str, nullable: bool, array: bool) -> String {
    let ruby = COLUMN_TYPES
        .iter()
        .find(|(schema, _)| *schema == kind)
        .map_or("untyped", |(_, ruby)| *ruby);
    let returns = if array {
        format!("Array[{ruby}]")
    } else if ruby == "untyped" {
        return "untyped".to_owned();
    } else {
        ruby.to_owned()
    };
    if nullable {
        format!("{returns}?")
    } else {
        returns
    }
}

struct Reader<'src> {
    source: &'src str,
    tables: Vec<Table>,
}

impl Reader<'_> {
    /// Every `create_table` in a body, and in the bodies of the blocks it holds.
    ///
    /// A dumped schema is `ActiveRecord::Schema[7.1].define do ... end` with the tables inside,
    /// so one level of block has to be descended; nothing else does. Bounded for the reason
    /// [`Models::walk`] gives, and exact for a file whose shape a generator decides.
    fn walk(&mut self, body: Option<Node<'_>>) {
        let Some(statements) = body.and_then(|body| body.as_statements_node()) else {
            return;
        };
        for statement in statements.body().iter() {
            let Some(call) = statement.as_call_node() else {
                continue;
            };
            if call.receiver().is_none()
                && call.name().as_slice() == b"create_table"
                && let Some(table) = self.table(&call)
            {
                self.tables.push(table);
                continue;
            }
            self.walk(call.block().and_then(|block| block.as_block_node()?.body()));
        }
    }

    fn table(&self, node: &CallNode<'_>) -> Option<Table> {
        let (name, name_at) = first_string(self.source, node)?;
        let body = node.block()?.as_block_node()?.body()?;
        let mut columns = Vec::new();
        columns.extend(self.primary_key(node, name_at));
        // The columns are statements of the block, which is what the dumper writes and the only
        // shape this reader claims to understand.
        for statement in body.as_statements_node()?.body().iter() {
            if let Some(call) = statement.as_call_node()
                && let Some(column) = self.column(&call)
            {
                columns.push(column);
            }
        }
        Some(Table { name, columns })
    }

    fn column(&self, node: &CallNode<'_>) -> Option<Column> {
        // The block parameter, whatever it is called. `|t|` is the convention and the dumper
        // always writes it, but the rule is "a call on the block's local", not "a call on `t`".
        node.receiver()?.as_local_variable_read_node()?;
        let kind = String::from_utf8_lossy(node.name().as_slice()).into_owned();
        if NOT_COLUMNS.contains(&kind.as_str()) {
            return None;
        }
        let (name, name_at) = first_string(self.source, node)?;
        if !is_column_name(&name) {
            return None;
        }
        let location = node.location();
        Some(Column {
            name,
            kind,
            nullable: keyword(node, "null").is_none_or(|null| null.as_false_node().is_none()),
            // The dumper writes it as a keyword rather than as part of the type, so this is
            // the one place the Ruby side spells what pg_dump spells `integer[]`.
            array: keyword(node, "array").is_some_and(|array| array.as_true_node().is_some()),
            at: (location.start_offset() as u32, location.end_offset() as u32),
            name_at,
        })
    }

    /// The column `create_table` declares by existing, when it declares one.
    ///
    /// `id: false` is the join table that has none; `primary_key: "sid"` renames it;
    /// `id: :uuid` retypes it, and a type this crate has not measured lands on `untyped` the
    /// way any other column's would. A composite `primary_key: ["a", "b"]` declares no single
    /// column and so declares nothing here.
    ///
    /// It is worth reading at all because `story.id` is one of the most common expressions in a
    /// Rails view and `id` has not been a method on `Object` since Ruby 1.9 — so without this it
    /// is a member lookup that fails, which is exactly what the schema reader is measured by.
    fn primary_key(&self, node: &CallNode<'_>, table_at: (u32, u32)) -> Option<Column> {
        let kind = match keyword(node, "id") {
            Some(id) if id.as_false_node().is_some() => return None,
            Some(id) => symbol_or_string(self.source, &id)?.0,
            None => PRIMARY_KEY.1.to_owned(),
        };
        // Its name is written down only when it is renamed, so the table's own name is what an
        // editor selects otherwise — the nearest thing in the file to "where `id` comes from".
        let (name, name_at) = match keyword(node, "primary_key") {
            Some(key) => symbol_or_string(self.source, &key)?,
            None => (PRIMARY_KEY.0.to_owned(), table_at),
        };
        if !is_column_name(&name) {
            return None;
        }
        Some(Column {
            name,
            kind,
            nullable: false,
            // A primary key is one value, whatever `id:` retypes it to.
            array: false,
            at: header(node)?,
            name_at,
        })
    }
}

struct Names<'src> {
    source: &'src str,
    nesting: Vec<String>,
    found: TableNames,
}

impl Names<'_> {
    /// The same statements-only descent [`Models::walk`] uses, and for the same two reasons.
    fn walk(&mut self, body: Option<Node<'_>>) {
        let Some(statements) = body.and_then(|body| body.as_statements_node()) else {
            return;
        };
        for statement in statements.body().iter() {
            if !self.nesting.is_empty() {
                if let Some(call) = statement.as_call_node() {
                    self.call(&call);
                } else if let Some(def) = statement.as_def_node() {
                    self.affix(&def);
                }
            }
            let (path, inner) = if let Some(class) = statement.as_class_node() {
                (class.constant_path(), class.body())
            } else if let Some(module) = statement.as_module_node() {
                (module.constant_path(), module.body())
            } else {
                continue;
            };
            self.nesting.push(constant_spelling(self.source, &path));
            self.walk(inner);
            self.nesting.pop();
        }
    }

    /// The two calls: the table this class reads, and the prefix an engine isolates.
    fn call(&mut self, call: &CallNode<'_>) {
        if call.name().as_slice() == b"table_name="
            && call
                .receiver()
                .is_some_and(|receiver| receiver.as_self_node().is_some())
            // `first_symbol_or_string` rather than the argument list inlined: a `table_name=`
            // with no argument at all is not Ruby anybody can write, so asking here would be an
            // arm no fixture can take.
            && let Some((table, _)) = first_symbol_or_string(self.source, call)
        {
            self.found.overrides.push((self.nesting.join("::"), table));
        }
        if call.name().as_slice() == b"isolate_namespace"
            && call.receiver().is_none()
            && let Some(argument) = call.arguments().and_then(|it| it.arguments().iter().next())
            // Sliced from the source like every other constant here, so the node has to be one:
            // `isolate_namespace self.class` would otherwise be spelled as whatever it reads.
            && (argument.as_constant_read_node().is_some()
                || argument.as_constant_path_node().is_some())
        {
            let spelled = constant_spelling(self.source, &argument);
            // `::Spree` is a path with no parent, which is Ruby's own escape from the lexical
            // walk and the same one a `class_name:` gets. `constant_spelling` drops
            // the colons, so the node is what says the name was absolute.
            let absolute = argument
                .as_constant_path_node()
                .is_some_and(|path| path.parent().is_none());
            self.found.isolated.push(if absolute {
                vec![spelled]
            } else {
                candidates(&self.nesting.join("::"), &spelled)
            });
        }
    }

    /// `def self.table_name_prefix` and its twin, when the body is one string literal.
    fn affix(&mut self, def: &DefNode<'_>) {
        if def.receiver().is_none_or(|it| it.as_self_node().is_none()) {
            return;
        }
        let into = match def.name().as_slice() {
            b"table_name_prefix" => &mut self.found.prefixes,
            b"table_name_suffix" => &mut self.found.suffixes,
            _ => return,
        };
        let Some(statements) = def.body().and_then(|body| body.as_statements_node()) else {
            return;
        };
        let mut body = statements.body().iter();
        let (Some(only), None) = (body.next(), body.next()) else {
            return;
        };
        if let Some((affix, _)) = string_literal(self.source, &only) {
            into.push((self.nesting.join("::"), affix));
        }
    }
}

/// A column name ya-lsp is willing to write into RBS.
///
/// Snake case and nothing else. Not a keyword rule — RBS takes `def type:` and `def class:`
/// without complaint, which was measured rather than assumed — but a **text** rule: this crate
/// is about to write the name into a file it then parses, and a column called `foo bar` or one
/// holding a quote would take the whole table's declarations down with it.
pub(super) fn is_column_name(name: &str) -> bool {
    name.starts_with(|first: char| first.is_ascii_lowercase() || first == '_')
        && name.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        })
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::declaring;

    /// Every shape the dumper writes that this reader has an opinion about, in one file.
    ///
    /// Deliberately one fixture rather than one per rule: what has to stay legible is that a
    /// `create_table` block is read as a whole, and a rule that only holds when it is the sole
    /// thing in the file is not a rule about `db/schema.rb`.
    const SCHEMA: &str = r#"ActiveRecord::Schema[7.1].define(version: 2024_01_01_000000) do
  create_table "stories", force: :cascade do |t|
    t.string "title", limit: 150, null: false
    t.text "description"
    t.datetime "created_at", precision: nil, null: false
    t.boolean "is_expired", default: false, null: false
    t.decimal "hotness", precision: 20, scale: 10
    t.string "tags", default: [], null: false, array: true
    t.jsonb "payload"
    t.index ["created_at"], name: "index_stories_on_created_at"
    t.check_constraint "length(title) > 0"
  end

  create_table "taggings", id: false, force: :cascade do |t|
    t.bigint "story_id", null: false
    t.foreign_key "stories"
  end

  create_table "keystores", primary_key: "key", id: :string, force: :cascade do |t|
    t.bigint "value"
  end

  create_table "unclaimed", force: :cascade do |t|
    t.string "nobody"
  end

  add_foreign_key "taggings", "stories"
end
"#;

    /// A workspace with no `enum` anywhere, which is every test in this file but one.
    fn no_enums() -> BTreeMap<String, BTreeSet<String>> {
        BTreeMap::new()
    }

    fn models() -> BTreeMap<String, Vec<String>> {
        [
            ("stories", "Story"),
            ("taggings", "Tagging"),
            ("keystores", "Keystore"),
        ]
        .into_iter()
        .map(|(table, class)| (table.to_owned(), vec![class.to_owned()]))
        .collect()
    }

    /// The whole output for [`SCHEMA`], pinned as one string.
    ///
    /// A pinned document rather than a set of assertions, because the thing that has to be
    /// reviewable is the *text* — this is a file the crate then parses, and a reader who wants
    /// to know what ya-lsp tells rubydex about a Rails model can read it here.
    #[test]
    fn the_rbs_a_schema_declares() {
        let signatures = read_schema(SCHEMA)
            .signatures("db/schema.rb", &models(), &no_enums())
            .render(&declaring(&[]));
        assert_eq!(
            signatures.rbs,
            r#"class Story
  # From `db/schema.rb`, table `stories`, column `id` (`bigint`, `null: false`).
  def id: () -> Integer
  # From `db/schema.rb`, table `stories`, column `title` (`string`, `null: false`).
  def title: () -> String
  # From `db/schema.rb`, table `stories`, column `description` (`text`, may be `nil`).
  def description: () -> String?
  # From `db/schema.rb`, table `stories`, column `created_at` (`datetime`, `null: false`).
  def created_at: () -> Time
  # From `db/schema.rb`, table `stories`, column `is_expired` (`boolean`, `null: false`).
  def is_expired: () -> bool
  # From `db/schema.rb`, table `stories`, column `hotness` (`decimal`, may be `nil`).
  def hotness: () -> BigDecimal?
  # From `db/schema.rb`, table `stories`, column `tags` (`string[]`, `null: false`).
  def tags: () -> Array[String]
  # From `db/schema.rb`, table `stories`, column `payload` (`jsonb`, may be `nil`).
  def payload: () -> untyped
end
class Tagging
  # From `db/schema.rb`, table `taggings`, column `story_id` (`bigint`, `null: false`).
  def story_id: () -> Integer
end
class Keystore
  # From `db/schema.rb`, table `keystores`, column `key` (`string`, `null: false`).
  def key: () -> String
  # From `db/schema.rb`, table `keystores`, column `value` (`bigint`, may be `nil`).
  def value: () -> Integer?
end
"#
        );
    }

    /// The ten types, both ways round, plus the eleventh that is not a type at all.
    ///
    /// A wrong mapping has to be a failing line here rather than a surprise in an editor. `untyped` is never optional — it already
    /// includes `nil`, and `untyped?` is not RBS.
    #[test]
    fn every_column_type_and_what_it_returns() {
        let rows: Vec<(&str, String, String)> = [
            "string", "text", "binary", "integer", "bigint", "boolean", "float", "decimal",
            "datetime", "date", "jsonb",
        ]
        .into_iter()
        .map(|kind| {
            (
                kind,
                rbs_type(kind, false, false),
                rbs_type(kind, true, false),
            )
        })
        .collect();

        let expected: Vec<(&str, String, String)> = [
            ("string", "String", "String?"),
            ("text", "String", "String?"),
            ("binary", "String", "String?"),
            ("integer", "Integer", "Integer?"),
            ("bigint", "Integer", "Integer?"),
            ("boolean", "bool", "bool?"),
            ("float", "Float", "Float?"),
            ("decimal", "BigDecimal", "BigDecimal?"),
            ("datetime", "Time", "Time?"),
            ("date", "Date", "Date?"),
            ("jsonb", "untyped", "untyped"),
        ]
        .into_iter()
        .map(|(kind, plain, optional)| (kind, plain.to_owned(), optional.to_owned()))
        .collect();

        assert_eq!(rows, expected);
    }

    /// The second dimension, and the one row it changes the shape of.
    ///
    /// `array: true` has to be read here as well as in the SQL reader, which sees the same fact
    /// as pg_dump's `integer[]`. Unread, `t.string "languages", array: true` answers `String`,
    /// which is a **wrong** answer rather than an absent one and the only kind these readers
    /// exist to prevent: 39 such columns in three of the six corpora's `schema.rb` alone.
    /// The unmapped row is the argument for wrapping rather than replacing — `Array[untyped]`
    /// says the one true thing where `untyped` says nothing.
    #[test]
    fn a_column_that_holds_many_of_its_type() {
        let rows: Vec<(&str, String, String)> = ["string", "bigint", "jsonb"]
            .into_iter()
            .map(|kind| {
                (
                    kind,
                    rbs_type(kind, false, true),
                    rbs_type(kind, true, true),
                )
            })
            .collect();

        let expected: Vec<(&str, String, String)> = [
            ("string", "Array[String]", "Array[String]?"),
            ("bigint", "Array[Integer]", "Array[Integer]?"),
            ("jsonb", "Array[untyped]", "Array[untyped]?"),
        ]
        .into_iter()
        .map(|(kind, plain, optional)| (kind, plain.to_owned(), optional.to_owned()))
        .collect();

        assert_eq!(rows, expected);
    }

    /// Where a generated declaration says it came from, against the file it came from.
    ///
    /// Spans on both sides, checked by slicing rather than by counting: the left is what the
    /// side table keys on, the right is what an editor reveals and selects.
    #[test]
    fn each_declaration_points_at_the_line_that_declared_it() {
        let signatures = read_schema(SCHEMA)
            .signatures("db/schema.rb", &models(), &no_enums())
            .render(&declaring(&[]));
        let rows: Vec<(&str, &str, &str)> = signatures
            .spans
            .iter()
            .map(|span| {
                (
                    signatures.rbs[span.generated.0 as usize..span.generated.1 as usize].trim(),
                    &SCHEMA[span.declared.0 as usize..span.declared.1 as usize],
                    &SCHEMA[span.selection.0 as usize..span.selection.1 as usize],
                )
            })
            .collect();

        assert_eq!(
            rows,
            vec![
                // The primary key is declared by the `create_table` line itself, so that is
                // what an editor reveals — and the table's name is what it selects, because
                // the column's name is nowhere in the file.
                (
                    "def id: () -> Integer",
                    "create_table \"stories\", force: :cascade",
                    "stories"
                ),
                (
                    "def title: () -> String",
                    "t.string \"title\", limit: 150, null: false",
                    "title"
                ),
                (
                    "def description: () -> String?",
                    "t.text \"description\"",
                    "description"
                ),
                (
                    "def created_at: () -> Time",
                    "t.datetime \"created_at\", precision: nil, null: false",
                    "created_at"
                ),
                (
                    "def is_expired: () -> bool",
                    "t.boolean \"is_expired\", default: false, null: false",
                    "is_expired"
                ),
                (
                    "def hotness: () -> BigDecimal?",
                    "t.decimal \"hotness\", precision: 20, scale: 10",
                    "hotness"
                ),
                (
                    "def tags: () -> Array[String]",
                    "t.string \"tags\", default: [], null: false, array: true",
                    "tags"
                ),
                (
                    "def payload: () -> untyped",
                    "t.jsonb \"payload\"",
                    "payload"
                ),
                (
                    "def story_id: () -> Integer",
                    "t.bigint \"story_id\", null: false",
                    "story_id"
                ),
                // Renamed, so the name *is* in the file and is what gets selected.
                (
                    "def key: () -> String",
                    "create_table \"keystores\", primary_key: \"key\", id: :string, force: :cascade",
                    "key"
                ),
                ("def value: () -> Integer?", "t.bigint \"value\"", "value"),
            ]
        );
    }

    /// A schema in a project whose models this reader cannot name declares nothing at all.
    #[test]
    fn a_table_nobody_claims_declares_nothing() {
        assert_eq!(
            read_schema(SCHEMA).signatures("db/schema.rb", &BTreeMap::new(), &no_enums()),
            Facts::default()
        );
    }

    /// The one column a schema declines: the one an `enum` re-types.
    ///
    /// Rank 2 over rank 3 spent by the loser rather than by `Facts`, because the two
    /// declarations are never in one document — `synthesized.md` has the argument. It is scoped
    /// to the *class*, so a `status` column on a table with no `enum` is untouched.
    #[test]
    fn a_column_an_enum_re_types_is_not_declared_here() {
        let source = "\
create_table \"stories\", force: :cascade do |t|
  t.integer \"status\", default: 0, null: false
  t.string \"title\", null: false
end
create_table \"comments\", force: :cascade do |t|
  t.integer \"status\", default: 0, null: false
end
";
        let classes = [("stories", "Story"), ("comments", "Comment")]
            .into_iter()
            .map(|(table, class)| (table.to_owned(), vec![class.to_owned()]))
            .collect();
        let enums = [("Story".to_owned(), ["status".to_owned()].into())]
            .into_iter()
            .collect();
        let rbs = read_schema(source)
            .signatures("db/schema.rb", &classes, &enums)
            .render(&declaring(&[]))
            .rbs;
        assert!(
            !rbs.contains("class Story\n  # From `db/schema.rb`, table `stories`, column `status`"),
            "{rbs}"
        );
        assert!(rbs.contains("def title: () -> String"), "{rbs}");
        assert!(
            rbs.contains("def status: () -> Integer"),
            "the other class keeps its own column: {rbs}"
        );
    }

    /// A `create_table` with nothing in it but constraints is not a class worth reopening.
    #[test]
    fn a_table_with_no_columns_is_not_written_out() {
        let source = "create_table \"joins\", id: false do |t|\n  t.index [\"a\"]\nend\n";
        let classes = [("joins".to_owned(), vec!["Join".to_owned()])]
            .into_iter()
            .collect();
        assert_eq!(
            read_schema(source).signatures("db/schema.rb", &classes, &no_enums()),
            Facts::default()
        );
    }

    /// What the reader refuses to write down, each for a different reason.
    #[test]
    fn what_is_not_a_column() {
        let source = r#"create_table "oddities" do |t|
  t.string "ok2"
  t.string "Not Snake Case"
  t.string
  t.string :symbolic
  string "no_receiver"
end
"#;
        let classes = [("oddities".to_owned(), vec!["Oddity".to_owned()])]
            .into_iter()
            .collect();
        let signatures = read_schema(source)
            .signatures("db/schema.rb", &classes, &no_enums())
            .render(&declaring(&[]));
        // `id` and `ok`, and nothing else: a name that cannot be written into RBS, a call with
        // no arguments, a symbol where a string was needed, and a call on no receiver at all.
        assert_eq!(
            signatures
                .rbs
                .lines()
                .filter(|line| line.trim_start().starts_with("def "))
                .collect::<Vec<_>>(),
            vec!["  def id: () -> Integer", "  def ok2: () -> String?"]
        );
    }

    /// What is not a table, each for a different reason — and what survives one anyway.
    #[test]
    fn what_is_not_a_table() {
        let source = r#"NOT_A_CALL = 1
create_table :symbolic do |t|
  t.string "a"
end
create_table "no_block"
create_table
create_table "renamed", primary_key: "Not A Name" do |t|
  ALSO_NOT_A_CALL = 2
  t.string "b"
end
"#;
        let classes = [
            ("symbolic", "Symbolic"),
            ("no_block", "NoBlock"),
            ("renamed", "Renamed"),
        ]
        .into_iter()
        .map(|(table, class)| (table.to_owned(), vec![class.to_owned()]))
        .collect();

        // A symbol where the dumper writes a string, a table with no block, a call with no
        // arguments at all, a renamed primary key that is not a name — which costs that table
        // its `id` and not its columns — and, at both levels, a statement that is not a call at
        // all, which the dumper never writes and a hand-edited schema might.
        let signatures = read_schema(source)
            .signatures("db/schema.rb", &classes, &no_enums())
            .render(&declaring(&[]));
        assert_eq!(
            signatures
                .rbs
                .lines()
                .filter(|line| line.trim_start().starts_with("def "))
                .collect::<Vec<_>>(),
            vec!["  def b: () -> String?"]
        );
    }

    /// A composite primary key names no single column, so it declares none.
    #[test]
    fn a_composite_primary_key_declares_nothing() {
        let source =
            "create_table \"pairs\", primary_key: [\"a\", \"b\"] do |t|\n  t.string \"a\"\nend\n";
        let classes = [("pairs".to_owned(), vec!["Pair".to_owned()])]
            .into_iter()
            .collect();
        let signatures = read_schema(source)
            .signatures("db/schema.rb", &classes, &no_enums())
            .render(&declaring(&[]));
        assert!(!signatures.rbs.contains("def id"), "{}", signatures.rbs);
        assert!(signatures.rbs.contains("def a:"), "{}", signatures.rbs);
    }

    /// The documented escape, in every nesting it can be written in.
    #[test]
    fn a_class_that_names_its_own_table() {
        let source = r##"module Admin
  class Setting
    self.table_name = "admin_settings_v2"
  end
end

class ::Tag
  self.table_name = 'tags_v2'
end

class Legacy::Story
  self.table_name = "stories_2009"
end

class Symbolic
  self.table_name = :symbols_v2
end

class Interpolated
  self.table_name = "#{prefix}_stories"
end

class Elsewhere
  other.table_name = "not_ours"
  NOT_A_CALL = 1
  validates :name
end

class Empty
end

module AlsoEmpty; end

self.table_name = "outside_any_class"
"##;
        assert_eq!(
            read_table_names(source).overrides,
            vec![
                ("Admin::Setting".to_owned(), "admin_settings_v2".to_owned()),
                ("Tag".to_owned(), "tags_v2".to_owned()),
                ("Legacy::Story".to_owned(), "stories_2009".to_owned()),
                // A symbol counts: `table_name=` is `value&.to_s` in Rails' own source, and
                // eleven of mastodon's thirteen are written this way.
                ("Symbolic".to_owned(), "symbols_v2".to_owned()),
            ]
        );
    }

    /// Every shape a table-name affix is read out of, and every one that is refused.
    #[test]
    fn a_namespace_that_declares_a_prefix() {
        let source = r##"module Admin
  def self.table_name_prefix
    "admin_"
  end

  def self.table_name_suffix = "_v2"

  def self.table_name_prefix_ish
    "no_"
  end

  def table_name_prefix
    "instance_"
  end
end

module Interpolated
  def self.table_name_prefix
    "#{Rails.env}_"
  end
end

module TooMuch
  def self.table_name_prefix
    warn "hello"
    "too_much_"
  end
end

module Empty
  def self.table_name_prefix
  end
end
"##;
        let names = read_table_names(source);
        assert_eq!(
            names.prefixes,
            vec![("Admin".to_owned(), "admin_".to_owned())]
        );
        // The endless `def` is the same node with the same one-statement body, so it needs no
        // rule of its own — which is what this row is here to show.
        assert_eq!(names.suffixes, vec![("Admin".to_owned(), "_v2".to_owned())]);
        assert!(names.overrides.is_empty());
        assert!(names.isolated.is_empty());
    }

    /// `isolate_namespace`, which is the commoner spelling and the one that names its module.
    #[test]
    fn an_engine_that_isolates_a_namespace() {
        let source = r##"module Spree
  module Core
    class Engine < ::Rails::Engine
      isolate_namespace Spree
    end
  end
end

module DiscourseChatIntegration
  module Provider
    class Engine < ::Rails::Engine
      isolate_namespace Provider
    end
  end
end

class Deep < ::Rails::Engine
  isolate_namespace Foo::Bar
end

class Absolute < ::Rails::Engine
  isolate_namespace ::Spree
end

class NotAConstant < ::Rails::Engine
  isolate_namespace self.class
  isolate_namespace
  Rails.isolate_namespace Other
  isolate_namespace Ünicode
end
"##;
        assert_eq!(
            read_table_names(source).isolated,
            vec![
                // Innermost first and the bare name last, which is what settles discourse's
                // `Provider`: written inside `DiscourseChatIntegration`, it is that module's,
                // and its tables begin `discourse_chat_integration_provider_`.
                vec![
                    "Spree::Core::Engine::Spree".to_owned(),
                    "Spree::Core::Spree".to_owned(),
                    "Spree::Spree".to_owned(),
                    "Spree".to_owned(),
                ],
                vec![
                    "DiscourseChatIntegration::Provider::Engine::Provider".to_owned(),
                    "DiscourseChatIntegration::Provider::Provider".to_owned(),
                    "DiscourseChatIntegration::Provider".to_owned(),
                    "Provider".to_owned(),
                ],
                vec!["Deep::Foo::Bar".to_owned(), "Foo::Bar".to_owned()],
                // A leading `::` is Ruby's own escape from the walk, so there is one candidate
                // and it is the one that was written.
                vec!["Spree".to_owned()],
                // A name no prefix can be spelled for is still a name: this reader says what
                // the call named and [`engine_prefix`] is what declines it.
                vec!["NotAConstant::Ünicode".to_owned(), "Ünicode".to_owned()],
            ]
        );
    }

    /// The prefix `Rails::Engine` installs, and the names it cannot spell one for.
    #[test]
    fn the_prefix_an_isolated_namespace_installs() {
        // `generate_railtie_name` is `underscore(mod.name).tr("/", "_")`, so a namespaced module
        // is one word with the separator underscored away.
        assert_eq!(engine_prefix("Spree").as_deref(), Some("spree_"));
        assert_eq!(
            engine_prefix("DiscourseAi").as_deref(),
            Some("discourse_ai_")
        );
        assert_eq!(engine_prefix("Foo::Bar").as_deref(), Some("foo_bar_"));
        // `underscore` wants an ASCII capital and there is no acronym table, for the reason
        // every other inflection here has none: a miss costs an answer, never a wrong one.
        assert_eq!(engine_prefix("Ünicode"), None);
        assert_eq!(engine_prefix("Foo::ünicode"), None);
    }

    /// A table more than one class reads declares its columns on every one of them.
    #[test]
    fn two_classes_that_read_one_table_both_get_its_columns() {
        let source = "create_table \"dogs\" do |t|\n  t.string \"name\", null: false\nend\n";
        let classes = [(
            "dogs".to_owned(),
            vec!["Dog".to_owned(), "Migration::Dog".to_owned()],
        )]
        .into_iter()
        .collect();
        let rbs = read_schema(source)
            .signatures("db/schema.rb", &classes, &no_enums())
            .render(&declaring(&["Migration"]))
            .rbs;
        assert!(rbs.starts_with("class Dog\n"), "{rbs}");
        // `declaring` writes `module`, so the second owner is opened inside a wrapper rather
        // than spelled joined — which is `Declarations::open`'s rule and not this reader's.
        assert!(rbs.contains("module Migration\nclass Dog\n"), "{rbs}");
        assert_eq!(rbs.matches("def name: () -> String").count(), 2, "{rbs}");
    }

    /// A file with no schema in it costs one parse and answers nothing.
    #[test]
    fn a_file_that_creates_no_tables() {
        assert_eq!(
            read_schema("puts 'hello'\n").signatures("db/schema.rb", &models(), &no_enums()),
            Facts::default()
        );
        assert!(read_table_names("puts 'hello'\n").overrides.is_empty());
    }

    /// The table names a caller needs before any schema may write a line.
    #[test]
    fn the_tables_a_schema_creates() {
        assert_eq!(
            read_schema(SCHEMA).table_names().collect::<Vec<_>>(),
            vec!["stories", "taggings", "keystores", "unclaimed"]
        );
    }

    /// The provenance line names the file it actually came from.
    #[test]
    fn a_secondary_schema_says_which_file_it_is() {
        let source = "create_table \"dogs\" do |t|\n  t.string \"name\", null: false\nend\n";
        let classes = [("dogs".to_owned(), vec!["Dog".to_owned()])]
            .into_iter()
            .collect();
        let rbs = read_schema(source)
            .signatures("db/animals_schema.rb", &classes, &no_enums())
            .render(&declaring(&[]))
            .rbs;
        assert!(
            rbs.contains("From `db/animals_schema.rb`, table `dogs`"),
            "{rbs}"
        );
        assert!(!rbs.contains("db/schema.rb"), "{rbs}");
    }
}
