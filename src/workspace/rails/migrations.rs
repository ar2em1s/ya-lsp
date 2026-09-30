//! What a migration calls on itself, and the column methods its table blocks call.
//!
//! Almost nothing a migration calls is a method of the migration. `add_column`, `create_table`
//! and `execute` reach `ActiveRecord::Migration#method_missing`, which sends them to the
//! connection:
//!
//! ```ruby
//! def method_missing(method, *arguments, &block)
//!   say_with_time "#{method}(#{format_arguments(arguments)})" do
//!     # … the table name gets its prefix and suffix …
//!     return super unless execution_strategy.respond_to?(method)
//!     execution_strategy.send(method, *arguments, &block)
//!   end
//! end
//! ```
//!
//! and the default strategy's own `method_missing` is `connection.send(method, ...)`. rubydex sees
//! none of it, so every receiverless call in `def change` fell to the name rung: `create_table`
//! listed nine definitions, a MySQL one among them in a PostgreSQL application, and the `t` of
//! `create_table … do |t|` was nothing, so `t.integer` guessed a parser gem's method.
//!
//! # What is forwarded, and why it is read from the bundle
//!
//! **The public `def`s of the connection's three statement modules**: `SchemaStatements` (every
//! command a migration writes, and `table_exists?` and its siblings), `DatabaseStatements`
//! (`execute`, `select_value`, `transaction`) and `Quoting`. Each is declared on
//! `ActiveRecord::Migration` with the parameters its `def` has, and placed at that `def`: a jump
//! from `add_index` lands on `SchemaStatements#add_index`, the method the call runs.
//!
//! **Read out of the bundle's own files, not written down here**, because the list moves with the
//! Rails version (`add_unique_constraint`, `create_virtual_table` and `enable_index` are all recent)
//! and the bundle has the list that is true for it. Measured over the six corpora's 4,388
//! migrations: 9,633 of 11,542 receiverless calls land in these three modules (`tmp/bench/
//! migrations/`).
//!
//! **Not forwarded:** the adapter class's own methods (`enable_extension`, `create_enum`,
//! `adapter_name`), because `AbstractAdapter` also overrides `Object`'s (`inspect`), and one
//! declared on a migration would hide the `Object` method Ruby really calls; and an adapter's own
//! modules (`PostgreSQL::SchemaStatements#validate_foreign_key`), because which adapter a
//! migration runs against is in `config/database.yml`, which nothing here reads.
//!
//! `initialize` is skipped: Ruby makes it private wherever it is written, so `DatabaseStatements`'
//! is no command, and writing it onto the migration would hand `Migration.new` a second signature.
//!
//! # The block's `t`
//!
//! `create_table`, `create_join_table` and `drop_table` hand their block a `TableDefinition`, and
//! `change_table` a `Table` (the command recorder does the same when a `change` is reverted: it
//! yields `update_table_definition`'s `Table`, and runs a dropped table's block through
//! `create_table`). So those four keep their `def`'s parameters and gain the block
//! ([`YIELDS`]). An adapter hands a subclass of either (`PostgreSQL::TableDefinition`), so the
//! abstract class is true of every one of them, and short of the adapter's own column types.
//!
//! # The column methods
//!
//! `t.string`, `t.integer` and the rest are written by `define_column_methods`, a `module_eval` of a
//! string, which rubydex cannot read. Each symbol that call names is declared on the module it is
//! called in, `(*untyped, **untyped)` as the `module_eval` writes it, and placed at the symbol:
//! the call names the method, as `attr_reader :name` does. Three modules call it
//! ([`COLUMN_METHODS`]): the abstract one, PostgreSQL's and MySQL's.
//!
//! **Two shapes.** Rails 8.1 calls it in the module's body, which writes the methods on the module.
//! Rails 8.0 and before call it inside `included do`, which writes them on each class that includes
//! the module instead. Both are declared on the module: every includer then reaches them, which is
//! the set of receivers Ruby gives them either way, and only the card's owner differs from 8.0's.

use ruby_prism::{CallNode, StatementsNode};

use super::concerns::installed;
use super::syntax::{bodies_named, header, spellable, symbol_or_string};
use crate::generated::{Declared, Facts, Namespaces, Owner, Source};

/// The class a migration inherits: `ActiveRecord::Migration[8.0]` is a class under it, and rubydex
/// records the receiver of `[]` as the superclass.
const MIGRATION: &str = "ActiveRecord::Migration";

/// The modules whose public `def`s a migration reaches through `method_missing`.
const STATEMENTS: [&str; 3] = [
    "ActiveRecord::ConnectionAdapters::SchemaStatements",
    "ActiveRecord::ConnectionAdapters::DatabaseStatements",
    "ActiveRecord::ConnectionAdapters::Quoting",
];

/// The modules that call `define_column_methods` in their own body.
const COLUMN_METHODS: [&str; 3] = [
    "ActiveRecord::ConnectionAdapters::ColumnMethods",
    "ActiveRecord::ConnectionAdapters::PostgreSQL::ColumnMethods",
    "ActiveRecord::ConnectionAdapters::MySQL::ColumnMethods",
];

/// What `create_table` and its siblings hand their block.
const TABLE_DEFINITION: &str = "ActiveRecord::ConnectionAdapters::TableDefinition";

/// What `change_table` hands its block.
const TABLE: &str = "ActiveRecord::ConnectionAdapters::Table";

/// The forwarded commands whose block is handed a table, and which class it is.
const YIELDS: [(&str, &str); 4] = [
    ("create_table", TABLE_DEFINITION),
    ("create_join_table", TABLE_DEFINITION),
    ("drop_table", TABLE_DEFINITION),
    ("change_table", TABLE),
];

/// The one method name Ruby makes private wherever it is written, and the three statement modules
/// write it.
const IMPLICITLY_PRIVATE: &str = "initialize";

/// The modules whose files [`read_migrations`] reads, for the caller to find the files declaring
/// them.
#[must_use]
pub fn migration_sources() -> Vec<&'static str> {
    STATEMENTS.iter().chain(&COLUMN_METHODS).copied().collect()
}

/// Every name this reader writes onto or names, and each namespace above one, for the bundle to be
/// asked whether it declares them: [`super::framework_constants`]' rule.
#[must_use]
pub fn migration_constants() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = migration_sources()
        .into_iter()
        .chain([MIGRATION, TABLE_DEFINITION, TABLE])
        .flat_map(|name| {
            name.match_indices("::")
                .map(move |(at, _)| &name[..at])
                .chain([name])
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// What `module`'s own body in `source` declares for a migration: its public `def`s onto
/// `ActiveRecord::Migration` for a statement module, its `define_column_methods` symbols onto
/// itself for a column module, nothing for any other name.
///
/// `file` is how the source is spelled to a person. Each declaration's place is in `source`, so the
/// caller hosts what comes back on that file.
#[must_use]
pub fn read_migrations(source: &str, module: &str, file: &str, namespaces: &Namespaces) -> Facts {
    let mut facts = Facts::default();
    if STATEMENTS.contains(&module) {
        forwarded(&mut facts, source, module, file, namespaces);
    } else if COLUMN_METHODS.contains(&module) {
        column_methods(&mut facts, source, module, file, namespaces);
    }
    facts
}

/// The owner a member of `name` is declared on, in the keyword the graph opens `name` with.
fn owned(name: &str, namespaces: &Namespaces) -> Owner {
    if namespaces.opens(name) {
        Owner::Module(name.to_owned())
    } else {
        Owner::Instance(name.to_owned())
    }
}

fn forwarded(facts: &mut Facts, source: &str, module: &str, file: &str, namespaces: &Namespaces) {
    if !namespaces.declares(MIGRATION) || !namespaces.spellable(MIGRATION) {
        return;
    }
    for method in installed(source, module) {
        if method.name == IMPLICITLY_PRIVATE {
            continue;
        }
        let parameters = match YIELDS.iter().find(|(name, _)| *name == method.name) {
            Some((_, table)) if namespaces.declares(table) => {
                format!("{} ?{{ ({table}) -> void }}", method.parameters)
            }
            _ => method.parameters.clone(),
        };
        facts.declare(Declared {
            owner: owned(MIGRATION, namespaces),
            name: method.name.clone(),
            returns: "untyped".to_owned(),
            parameters,
            because: format!(
                "From `{file}`, `{}` in `{module}`: `{MIGRATION}#method_missing` sends a \
                 migration's own call to its connection, which includes that module.",
                method.written
            ),
            at: Some((method.at, method.name_at)),
            from: Source::Convention,
            overloads: Vec::new(),
            private: false,
        });
    }
}

fn column_methods(
    facts: &mut Facts,
    source: &str,
    module: &str,
    file: &str,
    namespaces: &Namespaces,
) {
    if !namespaces.declares(module) || !namespaces.spellable(module) {
        return;
    }
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut bodies: Vec<StatementsNode<'_>> = Vec::new();
    bodies_named(
        source,
        parsed
            .node()
            .as_program_node()
            .map(|program| program.statements()),
        module,
        &mut Vec::new(),
        &mut bodies,
    );
    for body in &bodies {
        for statement in body.body().iter() {
            let Some(call) = statement
                .as_call_node()
                .filter(|call| call.receiver().is_none())
            else {
                continue;
            };
            if call.name().as_slice() == b"define_column_methods" {
                declare_columns(
                    facts,
                    source,
                    &call,
                    module,
                    file,
                    namespaces,
                    ON_THE_MODULE,
                );
                continue;
            }
            // Rails 8.0 and before write the same call inside `included do`, which runs it on each
            // class that includes the module: the same receivers as a method of the module.
            let Some(block) = (call.name().as_slice() == b"included")
                .then(|| call.block()?.as_block_node()?.body()?.as_statements_node())
                .flatten()
            else {
                continue;
            };
            for inner in block.body().iter() {
                if let Some(call) = inner
                    .as_call_node()
                    .filter(|call| call.receiver().is_none())
                    .filter(|call| call.name().as_slice() == b"define_column_methods")
                {
                    declare_columns(facts, source, &call, module, file, namespaces, ON_INCLUDERS);
                }
            }
        }
    }
}

/// Where a `define_column_methods` in the module body writes each method, for its card.
const ON_THE_MODULE: &str = "writes the method with `module_eval`, so no `def` names it";

/// Where one inside `included do` writes it.
const ON_INCLUDERS: &str = "inside `included do` writes the method with `module_eval` on every \
                            class that includes the module, so no `def` names it";

/// One `define_column_methods` call's symbols, each a method on `module`. `writes` says how the
/// call made it.
fn declare_columns(
    facts: &mut Facts,
    source: &str,
    call: &CallNode<'_>,
    module: &str,
    file: &str,
    namespaces: &Namespaces,
    writes: &str,
) {
    // A call reaching here names at least one symbol only when it has arguments, and `header` needs
    // them for the same reason, so one test answers both.
    let (Some(arguments), Some(line)) = (call.arguments(), header(call)) else {
        return;
    };
    for argument in arguments.arguments().iter() {
        let Some((name, at)) = symbol_or_string(source, &argument) else {
            continue;
        };
        if !spellable(&name) {
            continue;
        }
        facts.declare(Declared {
            owner: owned(module, namespaces),
            because: format!(
                "From `{file}`: `define_column_methods :{name}` in `{module}` {writes}."
            ),
            name,
            // `names.each { |name| column(name, :string, **options) }`: the names it was given.
            returns: "Array[untyped]".to_owned(),
            parameters: "(*untyped, **untyped)".to_owned(),
            at: Some((line, at)),
            from: Source::Convention,
            overloads: Vec::new(),
            private: false,
        });
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::{migration_constants, migration_sources, read_migrations};
    use crate::generated::Namespaces;

    /// A bundle declaring what the readers look for, the modules as modules and the rest as
    /// classes, as activerecord writes them.
    fn bundle() -> Namespaces {
        let mut namespaces = Namespaces::default();
        for name in migration_constants() {
            let module = name.ends_with("Statements")
                || name.ends_with("Quoting")
                || name.ends_with("ColumnMethods")
                || !name.contains("::")
                || name.ends_with("ConnectionAdapters")
                || name.ends_with("PostgreSQL")
                || name.ends_with("MySQL");
            namespaces.declare(name.to_owned(), module);
        }
        namespaces
    }

    fn written(source: &str, module: &str, namespaces: &Namespaces) -> String {
        let facts = read_migrations(source, module, "schema_statements.rb", namespaces);
        facts.render(namespaces).rbs
    }

    const STATEMENTS: &str = "\
module ActiveRecord
  module ConnectionAdapters
    module SchemaStatements
      def initialize
      end

      def create_table(table_name, id: :primary_key, **options, &block)
      end

      def add_index(table_name, column_name, **options)
      end

      def self.helper
      end

      def [](key)
      end

      private
        def schema_creation
        end
    end
  end
end
";

    #[test]
    fn a_statement_module_s_public_defs_are_a_migration_s_own_calls() {
        let rbs = written(
            STATEMENTS,
            "ActiveRecord::ConnectionAdapters::SchemaStatements",
            &bundle(),
        );
        assert!(
            rbs.contains("module ActiveRecord\nclass Migration\n"),
            "{rbs}"
        );
        assert!(
            rbs.contains(
                "def create_table: (untyped, ?id: untyped, **untyped) ?{ \
                 (ActiveRecord::ConnectionAdapters::TableDefinition) -> void } -> untyped"
            ),
            "{rbs}"
        );
        assert!(
            rbs.contains("def add_index: (untyped, untyped, **untyped) -> untyped"),
            "{rbs}"
        );
        assert!(rbs.contains("sends a migration's own call"), "{rbs}");
        for absent in ["initialize", "helper", "[]", "schema_creation"] {
            assert!(!rbs.contains(&format!("def {absent}:")), "{absent}: {rbs}");
        }
    }

    #[test]
    fn each_forwarded_command_is_placed_at_its_def() {
        let facts = read_migrations(
            STATEMENTS,
            "ActiveRecord::ConnectionAdapters::SchemaStatements",
            "schema_statements.rb",
            &bundle(),
        );
        let declarations = facts.render(&bundle());
        let add_index = STATEMENTS.find("def add_index").expect("fixture") as u32;
        let name = add_index + 4;
        assert!(
            declarations
                .spans
                .iter()
                .any(|span| span.declared.0 == add_index && span.selection == (name, name + 9)),
            "{:?}",
            declarations.spans
        );
    }

    #[test]
    fn a_table_block_is_typed_only_where_the_bundle_declares_the_table_class() {
        let mut namespaces = Namespaces::default();
        for name in migration_constants() {
            if !name.ends_with("TableDefinition") {
                namespaces.declare(name.to_owned(), false);
            }
        }
        let rbs = written(
            STATEMENTS,
            "ActiveRecord::ConnectionAdapters::SchemaStatements",
            &namespaces,
        );
        assert!(
            rbs.contains("def create_table: (untyped, ?id: untyped, **untyped) -> untyped"),
            "{rbs}"
        );
    }

    #[test]
    fn nothing_is_forwarded_onto_a_migration_the_bundle_does_not_declare() {
        let module = "ActiveRecord::ConnectionAdapters::SchemaStatements";
        let empty = Namespaces::default();
        assert!(read_migrations(STATEMENTS, module, "f.rb", &empty).is_empty());
        // Declared, but under a namespace nothing declares: the joined name would introduce it.
        let mut orphaned = Namespaces::default();
        orphaned.declare("ActiveRecord::Migration".to_owned(), false);
        assert!(read_migrations(STATEMENTS, module, "f.rb", &orphaned).is_empty());
    }

    const COLUMNS: &str = "\
module ActiveRecord
  module ConnectionAdapters
    module ColumnMethods
      def primary_key(name, type = :primary_key, **options)
      end

      define_column_methods :bigint, :string, \"json\", :\"not-a-name\", 42
      define_column_methods
      helper.define_column_methods :ignored
      column_methods :ignored_too
    end

    module PostgreSQL
      module ColumnMethods
        included do
          define_column_methods :jsonb, :uuid
          other.define_column_methods :nope
          validates :nothing
        end
        included
        included { }
        extended do
          define_column_methods :not_either
        end
      end
    end
  end
end
";

    #[test]
    fn a_column_method_is_each_symbol_define_column_methods_names() {
        let namespaces = bundle();
        let rbs = written(
            COLUMNS,
            "ActiveRecord::ConnectionAdapters::ColumnMethods",
            &namespaces,
        );
        assert!(
            rbs.contains("module ActiveRecord::ConnectionAdapters\nmodule ColumnMethods\n"),
            "{rbs}"
        );
        for name in ["bigint", "string", "json"] {
            assert!(
                rbs.contains(&format!(
                    "def {name}: (*untyped, **untyped) -> Array[untyped]"
                )),
                "{name}: {rbs}"
            );
        }
        for absent in [
            "ignored",
            "ignored_too",
            "jsonb",
            "primary_key",
            "not-a-name",
        ] {
            assert!(!rbs.contains(&format!("def {absent}:")), "{absent}: {rbs}");
        }
        assert!(
            rbs.contains("writes the method with `module_eval`"),
            "{rbs}"
        );

        let postgres = written(
            COLUMNS,
            "ActiveRecord::ConnectionAdapters::PostgreSQL::ColumnMethods",
            &namespaces,
        );
        assert!(postgres.contains("def jsonb:"), "{postgres}");
        assert!(postgres.contains("def uuid:"), "{postgres}");
        assert!(postgres.contains("inside `included do`"), "{postgres}");
        for absent in ["string", "nope", "not_either"] {
            assert!(
                !postgres.contains(&format!("def {absent}:")),
                "{absent}: {postgres}"
            );
        }
    }

    #[test]
    fn a_column_method_is_placed_at_its_symbol() {
        let namespaces = bundle();
        let declarations = read_migrations(
            COLUMNS,
            "ActiveRecord::ConnectionAdapters::ColumnMethods",
            "schema_definitions.rb",
            &namespaces,
        )
        .render(&namespaces);
        let line = COLUMNS
            .find("define_column_methods :bigint")
            .expect("fixture") as u32;
        let symbol = COLUMNS.find(":string").expect("fixture") as u32 + 1;
        assert!(
            declarations
                .spans
                .iter()
                .any(|span| span.declared.0 == line && span.selection == (symbol, symbol + 6)),
            "{:?}",
            declarations.spans
        );
    }

    #[test]
    fn a_column_module_the_bundle_does_not_declare_declares_nothing() {
        let module = "ActiveRecord::ConnectionAdapters::ColumnMethods";
        assert!(read_migrations(COLUMNS, module, "f.rb", &Namespaces::default()).is_empty());
        // Declared, but under a namespace nothing declares: the joined name would introduce it.
        let mut orphaned = Namespaces::default();
        orphaned.declare(module.to_owned(), true);
        assert!(read_migrations(COLUMNS, module, "f.rb", &orphaned).is_empty());
    }

    #[test]
    fn any_other_module_is_read_for_nothing() {
        assert!(read_migrations(COLUMNS, "ActiveRecord::Base", "f.rb", &bundle()).is_empty());
    }

    #[test]
    fn the_sources_and_the_names_asked_of_the_bundle() {
        assert_eq!(migration_sources().len(), 6);
        let names = migration_constants();
        for name in [
            "ActiveRecord",
            "ActiveRecord::ConnectionAdapters",
            "ActiveRecord::ConnectionAdapters::PostgreSQL",
            "ActiveRecord::Migration",
            "ActiveRecord::ConnectionAdapters::Table",
        ] {
            assert!(names.contains(&name), "{name}: {names:?}");
        }
    }
}
